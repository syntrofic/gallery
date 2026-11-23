use serde_json::Value;

#[tokio::test]
async fn openrouter_deepseek_answers() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("secrets/openrouter.key");
    let key = std::fs::read_to_string(&path)
        .expect("secrets/openrouter.key")
        .trim()
        .to_string();
    assert!(key.starts_with("sk-or-"), "openrouter key format");
    let client = reqwest::Client::new();
    let reply: Value = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": "deepseek/deepseek-v4.1-flash",
            "messages": [{"role": "user", "content": "Reply with the single word pong."}],
            "max_tokens": 64,
            "reasoning": {"enabled": false}
        }))
        .send()
        .await
        .expect("chat")
        .error_for_status()
        .expect("chat status")
        .json()
        .await
        .expect("chat json");
    let text = reply["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    assert!(text.contains("pong"), "deepseek reply did not contain pong");
}

#[tokio::test]
async fn deepseek_flash_captions_synthetic_image() {
    let key_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("secrets/openrouter.key");
    assert!(key_path.is_file(), "openrouter key file");
    std::env::set_var("GALLERY_OPENROUTER_KEY_FILE", &key_path);
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = gallery::config::Config::from_env().unwrap();
    cfg.data_dir = dir.path().to_path_buf();
    cfg.admin_password = Some("owner-pass-12".into());
    cfg.ai_enabled = true;
    let state = gallery::app::bootstrap(cfg, true).await.unwrap();
    let actor_row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let actor = gallery::auth::Actor {
        id: actor_row.0,
        username: actor_row.1,
        role: actor_row.2,
        must_change_password: false,
        revision: actor_row.3,
    };
    let mut canvas = image::RgbImage::from_pixel(64, 64, image::Rgb([220, 20, 20]));
    for x in 0..16 {
        for y in 0..16 {
            canvas.put_pixel(x, y, image::Rgb([20, 40, 220]));
        }
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(canvas)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    let sha = gallery::ids::sha256_hex(&bytes);
    let key = format!("media/ab/synthetic/{sha}.bin");
    let media = state.media().await.unwrap();
    media.put_bytes(&key, &bytes, "image/png").await.unwrap();
    let mut tx = gallery::db::begin_immediate(&state.pool).await.unwrap();
    let blob_id = gallery::storage::record_blob(
        &mut tx,
        &sha,
        bytes.len() as i64,
        "image/png",
        "original",
        &media.backend_id,
        &key,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, original_mime, created_seq, created_at)
         VALUES('img-synthetic', ?, ?, 'synthetic.png', 64, 64, 'image/png', 1, ?)",
    )
    .bind(&sha)
    .bind(&blob_id)
    .bind(gallery::db::now_ms())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let scan = gallery::ai::start_scan(
        &state,
        &actor,
        gallery::ai::ScanRequest {
            scope: serde_json::json!({"image_ids": ["img-synthetic"]}),
        },
    )
    .await
    .unwrap();
    let mut caption = None;
    let mut failure = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        caption = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT text, model_key, input_sha, prompt_revision FROM captions WHERE image_id = 'img-synthetic' AND source = 'model'",
        )
        .fetch_optional(&state.pool)
        .await
        .unwrap();
        if caption.is_some() {
            break;
        }
        let job = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT state, failure_code FROM jobs WHERE kind = 'caption_image' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&state.pool)
        .await
        .unwrap();
        if let Some((state_name, code)) = job {
            if state_name == "failed" {
                failure = Some(code.unwrap_or_default());
                break;
            }
        }
    }
    if let Some(code) = failure {
        panic!("deepseek caption failed: {code}");
    }
    let (text, model_key, input_sha, prompt_revision) = caption.expect("deepseek caption");
    assert!(model_key.to_ascii_lowercase().contains("deepseek"), "{model_key}");
    assert_eq!(prompt_revision, gallery::vision::PROMPT_REVISION);
    assert_eq!(input_sha.len(), 64);
    assert!(!text.is_empty() && text.chars().count() <= 160);
    assert!(!text.chars().any(|c| c.is_control()));
    let lower = text.to_lowercase();
    assert!(
        text.contains('红') || text.contains('蓝') || lower.contains("red") || lower.contains("blue"),
        "deepseek caption was: {text}"
    );
    let event = sqlx::query_as::<_, (String,)>(
        "SELECT payload_json FROM events WHERE type = 'caption.model_written'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(!event.0.contains("sk-or-") && !text.contains("sk-or-"));
    let scan_state = gallery::ai::get_scan(&state.pool, &actor, &scan.id).await.unwrap();
    assert_eq!(scan_state["state"], "succeeded");
}
