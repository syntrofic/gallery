use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;
use std::io::Write;

use crate::auth::Actor;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::new_id;
use crate::storage::archive_key;

#[derive(Serialize)]
pub struct ClaimItem {
    pub image_id: String,
    pub ok: bool,
    pub claim_id: Option<String>,
    pub body_url: Option<String>,
    pub code: Option<String>,
}

pub async fn selected(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    image_ids: &[String],
) -> AppResult<serde_json::Value> {
    let max_selection = crate::settings::limits(pool).await?.max_download_selection.clamp(1, 50) as usize;
    if image_ids.len() > max_selection {
        return Err(AppError::unprocessable(
            "SELECTION_LIMIT",
            format!("max {max_selection} images"),
        ));
    }
    let album = crate::album::get(pool, Some(actor), album_id).await?;
    if !album.capabilities.download {
        return Err(AppError::not_found());
    }
    let mut items = Vec::new();
    let now = db::now_ms();
    for image_id in image_ids {
        let row = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT i.state, l.state
             FROM album_images ai
             JOIN images i ON i.id = ai.image_id
             JOIN storage_route r ON r.id = 1
             LEFT JOIN media_locations l
               ON l.blob_id = i.original_blob_id AND l.backend_id = r.active_backend_id
             WHERE ai.album_id = ? AND i.id = ?",
        )
        .bind(album_id)
        .bind(image_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
        let Some((image_state, location_state)) = row else {
            items.push(failed_claim(image_id, "NOT_FOUND"));
            continue;
        };
        if image_state != "active" {
            items.push(failed_claim(image_id, "IMAGE_UNAVAILABLE"));
            continue;
        }
        if location_state.as_deref() != Some("verified") {
            items.push(failed_claim(image_id, "MEDIA_MISSING"));
            continue;
        }
        let id = new_id();
        sqlx::query(
            "INSERT INTO download_claims(id, actor_id, album_id, image_id, expires_at, created_at)
             VALUES(?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&actor.id)
        .bind(album_id)
        .bind(image_id)
        .bind(now + 10 * 60 * 1000)
        .bind(now)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
        items.push(ClaimItem {
            image_id: image_id.clone(),
            ok: true,
            claim_id: Some(id.clone()),
            body_url: Some(format!("/api/v1/downloads/claims/{id}/body")),
            code: None,
        });
    }
    Ok(serde_json::json!({ "items": items }))
}

fn failed_claim(image_id: &str, code: &str) -> ClaimItem {
    ClaimItem {
        image_id: image_id.to_string(),
        ok: false,
        claim_id: None,
        body_url: None,
        code: Some(code.to_string()),
    }
}

pub async fn claim_meta(
    pool: &SqlitePool,
    actor: &Actor,
    claim_id: &str,
) -> AppResult<(String, String, String)> {
    let row = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT actor_id, album_id, image_id, expires_at FROM download_claims WHERE id = ?",
    )
    .bind(claim_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != actor.id {
        return Err(AppError::not_found());
    }
    if row.3 < db::now_ms() {
        return Err(AppError::new(
            axum::http::StatusCode::GONE,
            "CLAIM_EXPIRED",
            "Gone",
            "download claim expired",
        ));
    }
    let album = crate::album::get(pool, Some(actor), &row.1).await?;
    if !album.capabilities.download {
        return Err(AppError::not_found());
    }
    Ok((row.1, row.2, actor.id.clone()))
}

pub async fn request_archive(
    state: &crate::app::AppState,
    actor: &Actor,
    album_id: &str,
) -> AppResult<(u16, serde_json::Value)> {
    let album = crate::album::get(&state.pool, Some(actor), album_id).await?;
    let deleted: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT deleted_at FROM albums WHERE id = ?")
            .bind(album_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    if deleted.and_then(|row| row.0).is_some() {
        return Err(AppError::not_found());
    }
    if !album.capabilities.download || !album.capabilities.archive_enabled {
        return Err(AppError::new(
            axum::http::StatusCode::FORBIDDEN,
            "ARCHIVE_DISABLED",
            "Forbidden",
            "archive disabled",
        ));
    }
    let existing = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, archive_state, job_id FROM archive_packages
         WHERE album_id = ? AND content_revision = ? AND acl_revision = ?
           AND archive_state IN ('queued','building','ready')
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(album_id)
    .bind(album.content_revision)
    .bind(album.acl_revision)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some((id, st, job)) = existing {
        let status = if st == "ready" { 200 } else { 202 };
        return Ok((
            status,
            serde_json::json!({
                "id": id,
                "state": st,
                "job_id": job,
                "status_url": format!("/api/v1/archives/{id}")
            }),
        ));
    }
    let images = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT i.id, i.original_blob_id, COALESCE(ai.display_name, i.original_name), b.size
         FROM album_images ai
         JOIN images i ON i.id = ai.image_id
         JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE ai.album_id = ? AND i.state = 'active'
         ORDER BY ai.sort_key, i.id",
    )
    .bind(album_id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if images.len() > 10_000 {
        return Err(AppError::unprocessable("ARCHIVE_LIMIT", "too many items"));
    }
    let total: i64 = images.iter().map(|r| r.3).sum();
    if total > 20 * 1024 * 1024 * 1024 {
        return Err(AppError::unprocessable("ARCHIVE_LIMIT", "too large"));
    }
    if !archive_fits(free_bytes(&state.config.media_root()), total as u64) {
        return Err(AppError::unprocessable("ARCHIVE_LIMIT", "not enough free space"));
    }
    let archive_id = new_id();
    let now = db::now_ms();
    let epoch = crate::db::policy_epoch(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO archive_packages(id, album_id, actor_id, content_revision, acl_revision, policy_epoch, archive_state, created_at)
         VALUES(?, ?, ?, ?, ?, ?, 'queued', ?)",
    )
    .bind(&archive_id)
    .bind(album_id)
    .bind(&actor.id)
    .bind(album.content_revision)
    .bind(album.acl_revision)
    .bind(epoch)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    for (i, (image_id, blob_id, name, size)) in images.iter().enumerate() {
        sqlx::query(
            "INSERT INTO archive_items(archive_id, position, image_id, blob_id, name, size)
             VALUES(?, ?, ?, ?, ?, ?)",
        )
        .bind(&archive_id)
        .bind(i as i64)
        .bind(image_id)
        .bind(blob_id)
        .bind(name)
        .bind(size)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    }
    let payload = crate::jobs::JobPayload {
        kind: "build_archive".into(),
        payload_version: 1,
        scope: serde_json::json!({"album_id": album_id, "archive_id": archive_id}),
        target: serde_json::json!({
            "content_revision": album.content_revision,
            "acl_revision": album.acl_revision
        }),
        actor_id: Some(actor.id.clone()),
    };
    let job_id = crate::jobs::enqueue(
        &mut tx,
        "build_archive",
        &format!(
            "build_archive:{album_id}:{}:{}:{epoch}",
            album.content_revision, album.acl_revision
        ),
        &payload,
        Some(&actor.id),
        80,
    )
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE archive_packages SET job_id = ? WHERE id = ?")
        .bind(&job_id)
        .bind(&archive_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok((
        202,
        serde_json::json!({
            "id": archive_id,
            "state": "queued",
            "job_id": job_id,
            "status_url": format!("/api/v1/archives/{archive_id}")
        }),
    ))
}

pub async fn hold_archive(pool: &SqlitePool, archive_id: &str, holder: &str) -> AppResult<bool> {
    let now = db::now_ms();
    let row = sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT archive_state, expires_at FROM archive_packages WHERE id = ?",
    )
    .bind(archive_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((state, expires_at)) = row else {
        return Ok(false);
    };
    if state != "ready" {
        return Ok(false);
    }
    if expires_at.is_some_and(|expires| expires <= now) {
        let active: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM archive_reads WHERE archive_id = ? AND until_ms > ?",
        )
        .bind(archive_id)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if active.is_none() {
            return Ok(false);
        }
    }
    sqlx::query(
        "INSERT INTO archive_reads(archive_id, holder, until_ms) VALUES(?, ?, ?)
         ON CONFLICT(archive_id, holder) DO UPDATE SET until_ms = excluded.until_ms",
    )
    .bind(archive_id)
    .bind(holder)
    .bind(now + 24 * 60 * 60 * 1000)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(true)
}

pub async fn release_archive(pool: &SqlitePool, archive_id: &str, holder: &str) -> Result<()> {
    sqlx::query("DELETE FROM archive_reads WHERE archive_id = ? AND holder = ?")
        .bind(archive_id)
        .bind(holder)
        .execute(pool)
        .await?;
    Ok(())
}

pub struct ArchiveHold {
    pool: SqlitePool,
    archive_id: String,
    holder: String,
}

impl ArchiveHold {
    pub fn new(pool: SqlitePool, archive_id: impl Into<String>, holder: impl Into<String>) -> Self {
        Self {
            pool,
            archive_id: archive_id.into(),
            holder: holder.into(),
        }
    }
}

impl Drop for ArchiveHold {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let archive_id = self.archive_id.clone();
        let holder = self.holder.clone();
        tokio::spawn(async move {
            let _ = release_archive(&pool, &archive_id, &holder).await;
        });
    }
}

pub async fn expire_archives(pool: &SqlitePool, storage: &crate::storage::Storage) -> Result<u64> {
    let now = db::now_ms();
    sqlx::query("DELETE FROM archive_reads WHERE until_ms <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    let due = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT id, blob_id FROM archive_packages
         WHERE archive_state = 'ready' AND expires_at IS NOT NULL AND expires_at <= ?
           AND NOT EXISTS (
             SELECT 1 FROM archive_reads r WHERE r.archive_id = archive_packages.id AND r.until_ms > ?
           )",
    )
    .bind(now)
    .bind(now)
    .fetch_all(pool)
    .await?;
    let mut expired = 0_u64;
    for (id, blob_id) in due {
        if let Some(blob_id) = blob_id {
            if let Ok((key, _, _, _)) = crate::storage::location_for(pool, &blob_id, &storage.backend_id).await {
                let _ = storage.delete_key(&key).await;
            }
        }
        sqlx::query("UPDATE archive_packages SET archive_state = 'expired' WHERE id = ? AND archive_state = 'ready'")
            .bind(&id)
            .execute(pool)
            .await?;
        expired += 1;
    }
    Ok(expired)
}

pub async fn archive_status(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, i64, i64, i64, Option<String>)>(
        "SELECT album_id, actor_id, archive_state, job_id, content_revision, acl_revision, policy_epoch, failure_detail
         FROM archive_packages WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.1 != actor.id && actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let album = sqlx::query_as::<_, (i64, i64, Option<i64>)>(
        "SELECT content_revision, acl_revision, deleted_at FROM albums WHERE id = ?",
    )
    .bind(&row.0)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let failure_detail = row.7.clone();
    let mut state = row.2;
    let epoch = crate::db::policy_epoch(pool).await.map_err(anyhow::Error::from)?;
    if matches!(state.as_str(), "queued" | "building" | "ready") {
        if let Some((content, acl, deleted_at)) = album {
            if deleted_at.is_some() || content != row.4 || acl != row.5 || epoch != row.6 {
                state = "stale".into();
                sqlx::query("UPDATE archive_packages SET archive_state = 'stale' WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
        }
    }
    let missing = failure_detail
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|value| value.get("missing").cloned())
        .unwrap_or_else(|| serde_json::json!([]));
    Ok(serde_json::json!({
        "id": id,
        "album_id": row.0,
        "state": state,
        "job_id": row.3,
        "missing": missing,
        "body_url": if state == "ready" { Some(format!("/api/v1/archives/{id}/body")) } else { None }
    }))
}

pub async fn build_zip(state: &crate::app::AppState, archive_id: &str) -> Result<()> {
    let items = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT image_id, blob_id, name, size, position FROM archive_items WHERE archive_id = ? ORDER BY position",
    )
    .bind(archive_id)
    .fetch_all(&state.pool)
    .await?;
    let claimed = sqlx::query(
        "UPDATE archive_packages SET archive_state = 'building'
         WHERE id = ? AND archive_state IN ('queued', 'building')",
    )
    .bind(archive_id)
    .execute(&state.pool)
    .await?;
    if claimed.rows_affected() != 1 {
        return Ok(());
    }
    let media = state.media().await?;
    let key = archive_key(archive_id);
    let partial_path = media
        .backend()
        .local_file(&key)
        .map(|path| path.with_extension("partial"))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("gallery-{archive_id}.partial")));
    if let Some(parent) = partial_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut partial = PartialZip(partial_path.clone(), false);
    {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .read(true)
            .open(&partial.0)?;
        let mut zip = zip::ZipWriter::new(file);
        let mut used = std::collections::HashSet::new();
        let mut missing = Vec::new();
        for (image_id, blob_id, name, _size, position) in items.iter() {
            if archive_cancel_requested(&state.pool, archive_id).await? {
                sqlx::query(
                    "UPDATE archive_packages SET archive_state = 'canceled' WHERE id = ? AND archive_state = 'building'",
                )
                .bind(archive_id)
                .execute(&state.pool)
                .await?;
                return Ok(());
            }
            let located = match crate::storage::location_for(&state.pool, blob_id, &media.backend_id).await {
                Ok(row) => row,
                Err(err) if err.to_string().contains("missing verified location") => {
                    missing.push(image_id.clone());
                    continue;
                }
                Err(_) => anyhow::bail!("storage unavailable"),
            };
            let name = sanitize_name(name, (*position).max(0) as usize, &mut used);
            match write_zip_entry(&mut zip, &media, &name, &located.0, located.1).await {
                Ok(()) => {}
                Err(err) if crate::storage::is_missing(&err) => missing.push(image_id.clone()),
                Err(err) if err.to_string() == "storage unavailable" => {
                    anyhow::bail!("storage unavailable")
                }
                Err(err) => return Err(err),
            }
        }
        if !missing.is_empty() {
            fail_archive(&state.pool, archive_id, &missing).await?;
            anyhow::bail!("permanent:MEDIA_MISSING");
        }
        let file = zip.finish()?;
        file.sync_all()?;
    }
    let (sha, size) = hash_file(&partial.0)?;
    if let Some(dest) = media.backend().local_file(&key) {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            std::fs::remove_file(&dest)?;
        }
        if std::fs::rename(&partial.0, &dest).is_err() {
            std::fs::copy(&partial.0, &dest)?;
            std::fs::remove_file(&partial.0)?;
        }
        partial.1 = true;
    } else {
        media.backend().put_file(&key, &partial.0, "application/zip").await?;
    }
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    let blob_id = crate::storage::record_blob(
        &mut tx,
        &sha,
        size as i64,
        "application/zip",
        "archive",
        &media.backend_id,
        &key,
    )
    .await?;
    sqlx::query(
        "UPDATE archive_packages SET archive_state = 'ready', blob_id = ?, expires_at = ?
         WHERE id = ? AND archive_state = 'building'",
    )
    .bind(&blob_id)
    .bind(db::now_ms() + 24 * 3600 * 1000)
    .bind(archive_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub fn archive_fits(free: Option<u64>, total: u64) -> bool {
    free.is_none_or(|free| free >= total)
}

pub(crate) fn free_bytes(path: &std::path::Path) -> Option<u64> {
    let text = path.to_string_lossy();
    let c = std::ffi::CString::new(text.as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

const ZIP_CHUNK: u64 = 256 * 1024;

async fn write_zip_entry(
    zip: &mut zip::ZipWriter<std::fs::File>,
    media: &crate::storage::Storage,
    name: &str,
    object_key: &str,
    size: i64,
) -> Result<()> {
    if let Some(path) = media.backend().local_file(object_key) {
        let mut input = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => anyhow::bail!("storage missing"),
            Err(err) => anyhow::bail!("storage unavailable: {err}"),
        };
        let meta = input
            .metadata()
            .map_err(|err| anyhow::anyhow!("storage unavailable: {err}"))?;
        if !meta.is_file() {
            anyhow::bail!("storage unavailable: not a file");
        }
        zip.start_file(name.to_string(), archive_entry_options(meta.len()))?;
        std::io::copy(&mut input, zip)?;
        return Ok(());
    }
    let total = size.max(0) as u64;
    zip.start_file(name.to_string(), archive_entry_options(total))?;
    let mut offset = 0u64;
    while offset < total {
        let end = (offset + ZIP_CHUNK - 1).min(total - 1);
        let chunk = media
            .backend()
            .open(object_key, Some((offset, end)))
            .await
            .map_err(|err| {
                if crate::storage::is_missing(&err) {
                    anyhow::anyhow!("storage missing")
                } else {
                    anyhow::anyhow!("storage unavailable")
                }
            })?;
        if chunk.len() as u64 != end - offset + 1 {
            anyhow::bail!("storage unavailable");
        }
        zip.write_all(&chunk)?;
        offset = end + 1;
    }
    Ok(())
}

async fn fail_archive(pool: &SqlitePool, archive_id: &str, missing: &[String]) -> Result<()> {
    let detail = serde_json::json!({"missing": missing}).to_string();
    let mut tx = crate::db::begin_immediate(pool).await?;
    sqlx::query(
        "UPDATE jobs SET failure_code = 'MEDIA_MISSING', updated_at = ?
         WHERE id = (SELECT job_id FROM archive_packages WHERE id = ?) AND state = 'running'",
    )
    .bind(crate::db::now_ms())
    .bind(archive_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE archive_packages SET archive_state = 'failed', failure_detail = ?
         WHERE id = ? AND archive_state = 'building'",
    )
    .bind(detail)
    .bind(archive_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn archive_cancel_requested(pool: &SqlitePool, archive_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT j.state FROM archive_packages p
         LEFT JOIN jobs j ON j.id = p.job_id
         WHERE p.id = ?",
    )
    .bind(archive_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|row| row.0).as_deref() == Some("cancel_requested"))
}

const ZIP64_ENTRY: u64 = 0xFFFF_FFFF;

pub fn entry_needs_zip64(size: u64) -> bool {
    size >= ZIP64_ENTRY
}

fn archive_entry_options(size: u64) -> zip::write::SimpleFileOptions {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    if entry_needs_zip64(size) {
        options.large_file(true)
    } else {
        options
    }
}

struct PartialZip(std::path::PathBuf, bool);

impl Drop for PartialZip {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

fn hash_file(path: &std::path::Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn sanitize_name(name: &str, position: usize, used: &mut std::collections::HashSet<String>) -> String {
    let base = entry_base_name(name, position);
    if used.insert(base.clone()) {
        return base;
    }
    let (stem, ext) = split_archive_ext(&base);
    let stem = stem.to_string();
    let ext = ext.to_string();
    let mut n = position.max(1);
    loop {
        let candidate = if ext.is_empty() {
            format!("{stem}-{n}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        if !candidate.contains('/') && !candidate.contains('\\') && used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
        if n > position.saturating_add(10_000) {
            let fallback = format!("image-{position}-{n}");
            if used.insert(fallback.clone()) {
                return fallback;
            }
        }
    }
}

fn entry_base_name(name: &str, position: usize) -> String {
    let segment = name
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let cleaned: String = segment
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '/' && *ch != '\\')
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return format!("image-{position}");
    }
    let (stem, ext) = split_archive_ext(cleaned);
    if reserved_entry_stem(stem) {
        if ext.is_empty() {
            format!("_{stem}")
        } else {
            format!("_{stem}.{ext}")
        }
    } else {
        cleaned.to_string()
    }
}

fn split_archive_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

fn reserved_entry_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit())
}

pub async fn list_archives(pool: &SqlitePool, actor: &Actor) -> Result<serde_json::Value> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, album_id, archive_state FROM archive_packages WHERE actor_id = ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&actor.id)
    .fetch_all(pool)
    .await?;
    Ok(serde_json::json!({
        "items": rows.into_iter().map(|(id, album_id, state)| serde_json::json!({
            "id": id, "album_id": album_id, "state": state
        })).collect::<Vec<_>>(),
        "next_cursor": null
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip64_is_only_for_entries_past_four_gib() {
        assert!(!entry_needs_zip64(100));
        assert!(!entry_needs_zip64(ZIP64_ENTRY - 1));
        assert!(entry_needs_zip64(ZIP64_ENTRY));
    }

    #[test]
    fn archive_names_drop_paths_and_keep_a_stable_suffix() {
        let mut used = std::collections::HashSet::new();
        let first = sanitize_name("../album/photo.jpg", 0, &mut used);
        let nested = sanitize_name("a/../../etc/passwd", 1, &mut used);
        let reserved = sanitize_name("folder\\CON.jpg", 2, &mut used);
        let blank = sanitize_name("..", 4, &mut used);
        let again = sanitize_name("photo.jpg", 7, &mut used);
        assert_eq!(first, "photo.jpg");
        assert_eq!(nested, "passwd");
        assert_eq!(reserved, "_CON.jpg");
        assert_eq!(blank, "image-4");
        assert_eq!(again, "photo-7.jpg");
        for name in [&first, &nested, &reserved, &blank, &again] {
            assert!(!name.contains('/'));
            assert!(!name.contains('\\'));
        }
    }
}
