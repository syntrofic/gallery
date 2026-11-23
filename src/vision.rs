use anyhow::{anyhow, bail, Result};
use base64::Engine;
use std::io::Cursor;
use std::time::Duration;

use crate::app::AppState;
use crate::ids::sha256_hex;

pub const PROMPT_REVISION: &str = "caption-zh-160-v1";
const PROMPT: &str = "用中文一两句话描述画面里看得见的内容，不超过160字。不要推测身份、地点或画面里没有的信息。只输出描述正文。";
const MAX_EDGE: u32 = 1024;
const MAX_BYTES: usize = 5 * 1024 * 1024;

pub fn model_key() -> String {
    std::env::var("GALLERY_VISION_MODEL")
        .unwrap_or_else(|_| "deepseek/deepseek-v4.1-flash".into())
}

fn vision_url() -> String {
    std::env::var("GALLERY_VISION_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1/chat/completions".into())
}

pub fn inference_png(bytes: &[u8]) -> Result<(Vec<u8>, String)> {
    let img = image::load_from_memory(bytes).map_err(|e| anyhow!("permanent:FORMAT {e}"))?;
    encode_inference_png(img)
}

fn encode_inference_png(img: image::DynamicImage) -> Result<(Vec<u8>, String)> {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let scaled = if long <= MAX_EDGE {
        img
    } else {
        let scale = MAX_EDGE as f32 / long as f32;
        let nw = ((w as f32) * scale).round().max(1.0) as u32;
        let nh = ((h as f32) * scale).round().max(1.0) as u32;
        img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
    };
    let mut png = Vec::new();
    scaled
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| anyhow!("permanent:FORMAT {e}"))?;
    if png.is_empty() || png.len() > MAX_BYTES {
        bail!("permanent:FORMAT inference image size");
    }
    let sha = sha256_hex(&png);
    Ok((png, sha))
}

pub fn sanitize_caption(raw: &str) -> Result<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = collapsed.chars().take(160).collect();
    if text.is_empty() {
        bail!("permanent:FORMAT empty caption");
    }
    Ok(text)
}

pub async fn caption_image(state: &AppState, payload_json: &str) -> Result<()> {
    let payload: crate::jobs::JobPayload = serde_json::from_str(payload_json)?;
    let image_id = payload.scope["image_id"]
        .as_str()
        .ok_or_else(|| anyhow!("permanent:FORMAT image_id"))?
        .to_string();
    let scan_id = payload.scope["scan_id"].as_str().map(|s| s.to_string());
    let expected_sha = payload.scope["source_sha256"].as_str().unwrap_or("").to_string();
    let model = model_key();
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT state, source_sha256, original_blob_id FROM images WHERE id = ?",
    )
    .bind(&image_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((image_state, source_sha, blob_id)) = row else {
        return Ok(());
    };
    if image_state != "active" || source_sha != expected_sha {
        return Ok(());
    }
    let media = state.media().await?;
    let input = match published_inference_input(&state.pool, &media, &image_id).await? {
        Some(input) => input,
        None => {
            let (key, size, sha, _) =
                crate::storage::location_for(&state.pool, &blob_id, &media.backend_id).await?;
            let original = tempfile::NamedTempFile::new_in(state.config.work_root())?;
            media.copy_verified(&key, size, &sha, original.path()).await?;
            let (bytes, sha) = encode_inference_png(crate::processing::decode_file(original.path())?)?;
            InferenceInput { bytes, sha, mime: "image/png".into(), reused_spec: None }
        }
    };
    let input_sha = input.sha.clone();
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT input_sha FROM captions
         WHERE image_id = ? AND source = 'model' AND model_key = ? AND prompt_revision = ?",
    )
    .bind(&image_id)
    .bind(&model)
    .bind(PROMPT_REVISION)
    .fetch_optional(&state.pool)
    .await?;
    if existing.as_ref().map(|(sha,)| sha == &input_sha).unwrap_or(false) {
        finish_related_scans(&state.pool, &image_id, &expected_sha, &model, scan_id.as_deref()).await?;
        return Ok(());
    }
    let started = std::time::Instant::now();
    let reply = describe_image(&input.bytes, &input.mime).await?;
    let text = sanitize_caption(&reply.text)?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    let current = sqlx::query_as::<_, (String, String)>(
        "SELECT state, source_sha256 FROM images WHERE id = ?",
    )
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((image_state, source_sha)) = current else {
        tx.commit().await?;
        return Ok(());
    };
    if image_state != "active" || source_sha != expected_sha {
        tx.commit().await?;
        return Ok(());
    }
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO captions(image_id, source, model_key, prompt_revision, input_sha, source_sha256, text, review_state, updated_at)
         VALUES(?, 'model', ?, ?, ?, ?, ?, 'published', ?)
         ON CONFLICT(image_id, source) DO UPDATE SET
            model_key = excluded.model_key,
            prompt_revision = excluded.prompt_revision,
            input_sha = excluded.input_sha,
            source_sha256 = excluded.source_sha256,
            text = excluded.text,
            review_state = 'published',
            updated_at = excluded.updated_at",
    )
    .bind(&image_id)
    .bind(&model)
    .bind(PROMPT_REVISION)
    .bind(&input_sha)
    .bind(&expected_sha)
    .bind(&text)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    crate::events::emit_tx(
        &mut tx,
        "caption.model_written",
        "image",
        Some(&image_id),
        Some(&image_id),
        None,
        payload.actor_id.as_deref(),
        serde_json::json!({
            "model_key": model,
            "prompt_revision": PROMPT_REVISION,
            "input_sha": input_sha,
            "reused_spec": input.reused_spec,
            "chars": text.chars().count(),
            "prompt_tokens": reply.prompt_tokens,
            "completion_tokens": reply.completion_tokens,
            "elapsed_ms": elapsed_ms
        }),
    )
    .await?;
    crate::search::sync_image(&mut tx, &image_id).await?;
    tx.commit().await?;
    finish_related_scans(&state.pool, &image_id, &expected_sha, &model, scan_id.as_deref()).await?;
    Ok(())
}

struct VisionReply {
    text: String,
    prompt_tokens: i64,
    completion_tokens: i64,
}

pub struct InferenceInput {
    pub bytes: Vec<u8>,
    pub sha: String,
    pub mime: String,
    pub reused_spec: Option<String>,
}

pub fn inference_rank(width: i64, height: i64, size: i64, mime: &str) -> Option<i64> {
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    if !matches!(mime, "image/png" | "image/jpeg" | "image/jpg") {
        return None;
    }
    let long = width.max(height);
    if long <= 0 || long > MAX_EDGE as i64 || size <= 0 || size > MAX_BYTES as i64 {
        return None;
    }
    Some(long)
}

pub async fn load_inference_input(
    pool: &sqlx::SqlitePool,
    media: &crate::storage::Storage,
    image_id: &str,
    original: &[u8],
) -> Result<InferenceInput> {
    if let Some(input) = published_inference_input(pool, media, image_id).await? {
        return Ok(input);
    }
    let (bytes, sha) = inference_png(original)?;
    Ok(InferenceInput { bytes, sha, mime: "image/png".into(), reused_spec: None })
}

async fn published_inference_input(
    pool: &sqlx::SqlitePool,
    media: &crate::storage::Storage,
    image_id: &str,
) -> Result<Option<InferenceInput>> {
    let rows = sqlx::query_as::<_, (String, Option<i64>, Option<i64>, i64, String, String)>(
        "SELECT v.spec_id, v.width, v.height, b.size, b.mime, l.object_key
         FROM variant_heads h
         JOIN image_variants v ON v.id = h.variant_id
         JOIN media_blobs b ON b.id = v.blob_id
         JOIN media_locations l ON l.blob_id = b.id AND l.backend_id = ? AND l.state = 'verified'
         WHERE h.image_id = ? AND v.state = 'published'",
    )
    .bind(&media.backend_id)
    .bind(image_id)
    .fetch_all(pool)
    .await?;
    let mut ranked: Vec<_> = rows
        .into_iter()
        .filter_map(|(spec, width, height, size, mime, key)| {
            let long = inference_rank(width.unwrap_or(0), height.unwrap_or(0), size, &mime)?;
            Some((long, spec, mime, key))
        })
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    for (_long, spec, mime, key) in ranked {
        let Ok(bytes) = media.read_bounded(&key, MAX_BYTES).await else {
            continue;
        };
        let Ok(decoded) = image::load_from_memory(&bytes) else {
            continue;
        };
        if decoded.width().max(decoded.height()) > MAX_EDGE {
            continue;
        }
        return Ok(Some(InferenceInput {
            sha: sha256_hex(&bytes),
            mime,
            reused_spec: Some(spec),
            bytes,
        }));
    }
    Ok(None)
}

async fn describe_image(bytes: &[u8], mime: &str) -> Result<VisionReply> {
    let key = crate::config::openrouter_key().ok_or_else(|| anyhow!("permanent:CREDENTIALS"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let body = serde_json::json!({
        "model": model_key(),
        "max_tokens": 256,
        "reasoning": {"enabled": false},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": PROMPT},
                {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}"), "detail": "low"}}
            ]
        }]
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()?;
    let response = client
        .post(vision_url())
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|e| anyhow!("retryable:UPSTREAM {}", redact(&e.to_string())))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| anyhow!("retryable:UPSTREAM {}", redact(&e.to_string())))?;
    if !status.is_success() {
        let snippet = redact(&String::from_utf8_lossy(&bytes));
        let snippet: String = snippet.chars().take(180).collect();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            bail!("permanent:CREDENTIALS");
        }
        if status.as_u16() == 429 {
            bail!("retryable:RATE_LIMIT {snippet}");
        }
        if status.is_server_error() || status.as_u16() == 408 {
            bail!("retryable:UPSTREAM {snippet}");
        }
        bail!("permanent:MODEL_REJECTED {snippet}");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("permanent:FORMAT unreadable vision response"))?;
    let text = message_text(&value).ok_or_else(|| anyhow!("permanent:FORMAT missing caption"))?;
    Ok(VisionReply {
        prompt_tokens: value["usage"]["prompt_tokens"].as_i64().unwrap_or(0),
        completion_tokens: value["usage"]["completion_tokens"].as_i64().unwrap_or(0),
        text,
    })
}

fn message_text(value: &serde_json::Value) -> Option<String> {
    let content = &value["choices"][0]["message"]["content"];
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts = content.as_array()?;
    let mut text = String::new();
    for part in parts {
        if let Some(piece) = part["text"].as_str() {
            text.push_str(piece);
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn redact(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i..].starts_with(&['s', 'k', '-']) {
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            out.push_str("[redacted]");
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

async fn finish_related_scans(
    pool: &sqlx::SqlitePool,
    image_id: &str,
    expected_sha: &str,
    model: &str,
    fallback: Option<&str>,
) -> Result<()> {
    let dedupe = format!("caption:{image_id}:{expected_sha}:{model}:{PROMPT_REVISION}");
    let stored: Option<(String,)> = sqlx::query_as("SELECT scope_json FROM jobs WHERE dedupe_key = ?")
        .bind(&dedupe)
        .fetch_optional(pool)
        .await?;
    let mut scan_ids = Vec::new();
    if let Some((scope_json,)) = stored {
        let scope: serde_json::Value = serde_json::from_str(&scope_json).unwrap_or(serde_json::json!({}));
        if let Some(id) = scope["scan_id"].as_str() {
            scan_ids.push(id.to_string());
        }
        if let Some(values) = scope["scan_ids"].as_array() {
            for value in values {
                if let Some(id) = value.as_str() {
                    scan_ids.push(id.to_string());
                }
            }
        }
    } else if let Some(id) = fallback {
        scan_ids.push(id.to_string());
    }
    scan_ids.sort();
    scan_ids.dedup();
    for id in scan_ids {
        finish_scan(pool, Some(&id)).await?;
    }
    Ok(())
}

async fn finish_scan(pool: &sqlx::SqlitePool, scan_id: Option<&str>) -> Result<()> {
    let Some(scan_id) = scan_id else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE ai_scans SET state = 'succeeded', updated_at = ?
         WHERE id = ?
           AND state IN ('queued', 'running')
           AND (
             SELECT COUNT(*) FROM jobs
             WHERE kind IN ('caption_image', 'scan_plan')
               AND state IN ('queued', 'running', 'retry_wait', 'cancel_requested')
               AND (
                    json_extract(scope_json, '$.scan_id') = ?
                    OR EXISTS (
                        SELECT 1 FROM json_each(json_extract(scope_json, '$.scan_ids'))
                        WHERE value = ?
                    )
               )
           ) <= 1",
    )
    .bind(crate::db::now_ms())
    .bind(scan_id)
    .bind(scan_id)
    .bind(scan_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_and_limits_chars() {
        let raw = format!("红色方块\u{0001}{}", "啊".repeat(200));
        let text = sanitize_caption(&raw).unwrap();
        assert!(text.chars().count() <= 160);
        assert!(!text.chars().any(|c| c.is_control()));
        assert!(text.starts_with("红色方块"));
    }

    #[test]
    fn inference_rank_accepts_only_small_jpeg_or_png() {
        assert_eq!(inference_rank(960, 640, 1000, "image/png"), Some(960));
        assert_eq!(inference_rank(800, 1000, 1000, "image/jpeg"), Some(1000));
        assert!(inference_rank(1440, 900, 1000, "image/png").is_none());
        assert!(inference_rank(800, 600, MAX_BYTES as i64 + 1, "image/png").is_none());
        assert!(inference_rank(256, 256, 1000, "image/webp").is_none());
    }

    #[test]
    fn inference_png_shrinks_long_edge() {
        let img = image::RgbImage::from_pixel(2000, 12, image::Rgb([200, 10, 10]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let (png, sha) = inference_png(&bytes).unwrap();
        assert_eq!(sha.len(), 64);
        let decoded = image::load_from_memory(&png).unwrap();
        assert!(decoded.width().max(decoded.height()) <= MAX_EDGE);
        assert!(png.len() <= MAX_BYTES);
    }
}
