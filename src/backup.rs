use anyhow::{anyhow, Result};
use chrono::Datelike;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use serde_json::json;
use sqlx::SqlitePool;
use age::secrecy::ExposeSecret;
use std::io::Write;
use std::path::PathBuf;

use crate::app::AppState;
use crate::auth::Actor;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::{new_id, sha256_hex};

pub fn estimate_freeze_ms(sqlite_bytes: u64, images: i64) -> i64 {
    let base = (sqlite_bytes / 1_048_576).max(1) as i64 * 200;
    (base + images.max(0)).min(60_000)
}

pub async fn preview_backup(
    state: &AppState,
    actor: &Actor,
    repository_id: Option<&str>,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if let Some(id) = repository_id.map(str::trim).filter(|id| !id.is_empty()) {
        if id != "local" {
            return Err(AppError::conflict(
                "BACKUP_REPOSITORY",
                "only the local backup repository is configured",
            ));
        }
    }
    let (bytes, images): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(b.size), 0), COUNT(*)
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')",
    )
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let free = crate::download::free_bytes(&state.config.backup_root());
    let sqlite_bytes = std::fs::metadata(state.config.db_path()).map(|meta| meta.len()).unwrap_or(0);
    Ok(json!({
        "dry_run": true,
        "backup_repository_id": "local",
        "bytes": bytes.max(0),
        "images": images.max(0),
        "free_bytes": free,
        "capacity_ok": backup_capacity_ok(free, bytes.max(0) as u64),
        "freeze_ms": estimate_freeze_ms(sqlite_bytes, images)
    }))
}

pub async fn start_backup(state: &AppState, actor: &Actor) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if db::write_barrier(&state.pool)
        .await
        .map_err(anyhow::Error::from)?
    {
        return Err(AppError::conflict(
            "MAINTENANCE",
            "another maintenance operation holds the lock",
        ));
    }
    let (originals,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(b.size), 0)
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')",
    )
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let free = crate::download::free_bytes(&state.config.backup_root());
    if !backup_capacity_ok(free, originals.max(0) as u64) {
        return Err(AppError::conflict(
            "BACKUP_CAPACITY",
            "not enough space for a new snapshot",
        ));
    }
    let id = new_id();
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO backup_runs(id, state, created_at, updated_at) VALUES(?, 'queued', ?, ?)",
    )
    .bind(&id)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let branch = db::kv_get(&state.pool, "recovery_branch")
        .await
        .map_err(anyhow::Error::from)?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "main".into());
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE backup_runs SET branch_id = ? WHERE id = ?")
        .bind(&branch)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let payload = crate::jobs::JobPayload {
        kind: "backup".into(),
        payload_version: 1,
        scope: json!({"backup_id": id}),
        target: json!({}),
        actor_id: Some(actor.id.clone()),
    };
    let job_id = crate::jobs::enqueue(
        &mut tx,
        "backup",
        &format!("backup:{id}"),
        &payload,
        Some(&actor.id),
        5,
    )
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE backup_runs SET job_id = ? WHERE id = ?")
        .bind(&job_id)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    Ok(json!({
        "id": id,
        "state": "queued",
        "job_id": job_id,
        "status_url": format!("/api/v1/admin/backups/{id}")
    }))
}

pub async fn ensure_daily_backup(state: &AppState) -> Result<Option<String>> {
    let owner = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, role, revision FROM actors WHERE role = 'owner' AND status = 'active' ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    let actor = crate::auth::Actor {
        id: owner.0,
        username: owner.1,
        role: owner.2,
        must_change_password: false,
        revision: owner.3,
    };
    let day_start = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("utc day"))?
        .and_utc()
        .timestamp_millis();
    if let Some((id,)) = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND snapshot_at >= ?
         ORDER BY snapshot_at DESC LIMIT 1",
    )
    .bind(day_start)
    .fetch_optional(&state.pool)
    .await?
    {
        return Ok(Some(id));
    }
    if let Some((id,)) = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs WHERE state IN ('queued', 'running') ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?
    {
        return Ok(Some(id));
    }
    if db::write_barrier(&state.pool).await? {
        return Ok(None);
    }
    let started = start_backup(state, &actor).await.map_err(|err| anyhow!(err.to_string()))?;
    Ok(started["id"].as_str().map(str::to_string))
}

pub async fn write_snapshot(pool: &SqlitePool, dest: &std::path::Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let escaped = dest.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{escaped}'"))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn snapshot_original_ids(path: &std::path::Path) -> Result<Vec<String>> {
    let rows = originals_in_snapshot(path).await?;
    Ok(rows.into_iter().map(|row| row.1).collect())
}

async fn originals_in_snapshot(path: &std::path::Path) -> Result<Vec<(String, String, i64, String)>> {
    let snapshot = crate::db::connect(path).await?;
    let rows = sqlx::query_as(
        "SELECT i.id, i.original_blob_id, b.size, b.sha256
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')
         ORDER BY i.id",
    )
    .fetch_all(&snapshot)
    .await?;
    Ok(rows)
}

pub async fn run_backup(state: &AppState, payload_json: &str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let backup_id = payload["scope"]["backup_id"]
        .as_str()
        .ok_or_else(|| anyhow!("backup_id"))?;
    let dest = state.config.backup_root().join(backup_id);
    std::fs::create_dir_all(&dest)?;
    if resume_sealed_copy(state, backup_id, &dest).await? {
        return Ok(());
    }
    let snapshot_at = db::now_ms();
    let db_copy = dest.join("gallery.sqlite");
    if db_copy.exists() {
        std::fs::remove_file(&db_copy)?;
    }
    let escaped = db_copy.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{escaped}'"))
        .execute(&state.pool)
        .await?;
    let sqlite_plain = std::fs::read(&db_copy)?;
    let sqlite_age = age_encrypt_bytes(&state.config.keys_root(), &sqlite_plain)?;
    let sqlite_age_path = dest.join("gallery.sqlite.age");
    std::fs::write(&sqlite_age_path, &sqlite_age)?;
    let sqlite_read_back = std::fs::read(&sqlite_age_path)?;
    if sha256_hex(&sqlite_read_back) != sha256_hex(&sqlite_age) {
        anyhow::bail!("backup readback mismatch");
    }
    add_verify_stats(
        &state.pool,
        backup_id,
        sqlite_age.len() as i64,
        sqlite_read_back.len() as i64,
        sqlite_plain.len() as i64,
    )
    .await?;
    let originals = originals_in_snapshot(&db_copy).await?;
    let catalog: (i64,) =
        sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
            .fetch_one(&state.pool)
            .await?;
    let route_epoch: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await?;
    let library = db::instance_id(&state.pool).await?;
    let planned: Vec<_> = originals
        .iter()
        .map(|(_image_id, blob_id, size, sha)| {
            json!({"blob_id": blob_id, "sha256": sha, "size": size, "object": sha256_hex(blob_id.as_bytes())})
        })
        .collect();
    let manifest = json!({
        "backup_id": backup_id,
        "format_version": 1,
        "snapshot_at": snapshot_at,
        "catalog_revision": catalog.0,
        "routing_epoch": route_epoch.0,
        "instance_id": library,
        "items": planned
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(dest.join("manifest.json"), &manifest_bytes)?;
    let digest = sha256_hex(&manifest_bytes);
    sqlx::query(
        "UPDATE backup_runs SET snapshot_at = ?, catalog_revision = ?, routing_epoch = ?, instance_id = ?, manifest_digest = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(snapshot_at)
    .bind(catalog.0)
    .bind(route_epoch.0)
    .bind(&library)
    .bind(&digest)
    .bind(db::now_ms())
    .bind(backup_id)
    .execute(&state.pool)
    .await?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0 WHERE id = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
    .execute(&state.pool)
    .await?;
    let mut items = Vec::new();
    for (_image_id, blob_id, size, sha) in &originals {
        copy_backup_object(state, backup_id, &dest, blob_id, *size, sha).await?;
        items.push(json!({"blob_id": blob_id, "sha256": sha, "size": size}));
    }
    let _ = items;
    let signed = sign_complete(&state.config.keys_root(), backup_id, &digest)?;
    std::fs::write(dest.join("COMPLETE"), signed)?;
    commit_sealed_success(&state.pool, backup_id).await?;
    let _ = prune_retention(&state.pool, &state.config.backup_root(), db::now_ms()).await;
    Ok(())
}

pub async fn abandon_unsealed(pool: &SqlitePool, backup_id: &str) -> AppResult<()> {
    let row = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT state, manifest_digest, job_id FROM backup_runs WHERE id = ?",
    )
    .bind(backup_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 == "succeeded" || row.1.is_some() {
        return Err(AppError::conflict("BACKUP_SEALED", "sealed backups are not abandoned here"));
    }
    if let Some(job_id) = row.2 {
        sqlx::query(
            "UPDATE jobs SET state = 'canceled', lease_token = NULL, lease_until = NULL, updated_at = ?
             WHERE id = ? AND state IN ('queued', 'running', 'retry_wait')",
        )
        .bind(db::now_ms())
        .bind(job_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    sqlx::query("UPDATE backup_runs SET state = 'abandoned', updated_at = ? WHERE id = ?")
        .bind(db::now_ms())
        .bind(backup_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM backup_pins WHERE backup_id = ?")
        .bind(backup_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0 WHERE id = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

pub async fn resume_backup(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<i64>)>(
        "SELECT state, manifest_digest, job_id, snapshot_at FROM backup_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.1.is_none() {
        return Err(AppError::conflict(
            "BACKUP_UNSEALED",
            "the snapshot was not sealed; start a new backup",
        ));
    }
    if row.0 == "succeeded" {
        return Err(AppError::conflict("BACKUP_SEALED", "the backup already completed"));
    }
    let now = db::now_ms();
    if let Some(job_id) = row.2.as_deref() {
        sqlx::query(
            "UPDATE jobs SET state = 'queued', available_at = ?, lease_token = NULL, lease_until = NULL, updated_at = ?
             WHERE id = ? AND state IN ('failed', 'canceled', 'retry_wait')",
        )
        .bind(now)
        .bind(now)
        .bind(job_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    sqlx::query(
        "UPDATE backup_runs SET state = 'queued', updated_at = ?
         WHERE id = ? AND manifest_digest IS NOT NULL AND state != 'succeeded'",
    )
    .bind(now)
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(json!({
        "id": id,
        "state": "queued",
        "job_id": row.2,
        "snapshot_at": row.3
    }))
}

pub async fn recover_interrupted_backups(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT id, manifest_digest, job_id FROM backup_runs
         WHERE state IN ('queued', 'running')
         ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut resumed = Vec::new();
    let mut hold_barrier = false;
    for (id, digest, job_id) in &rows {
        if digest.is_none() {
            hold_barrier = true;
        }
        ensure_backup_job(pool, id, job_id.as_deref()).await?;
        resumed.push(id.clone());
    }
    if hold_barrier {
        sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
            .execute(pool)
            .await?;
    }
    Ok(resumed)
}

async fn ensure_backup_job(pool: &SqlitePool, backup_id: &str, job_id: Option<&str>) -> Result<()> {
    let now = db::now_ms();
    if let Some(job_id) = job_id {
        let existing: Option<(String,)> = sqlx::query_as("SELECT state FROM jobs WHERE id = ?")
            .bind(job_id)
            .fetch_optional(pool)
            .await?;
        if existing.is_some() {
            sqlx::query(
                "UPDATE jobs SET state = 'queued', lease_token = NULL, lease_until = NULL, available_at = ?, updated_at = ?
                 WHERE id = ? AND state IN ('running', 'failed', 'canceled', 'retry_wait')",
            )
            .bind(now)
            .bind(now)
            .bind(job_id)
            .execute(pool)
            .await?;
            sqlx::query(
                "UPDATE backup_runs SET state = 'queued', updated_at = ? WHERE id = ? AND state = 'running'",
            )
            .bind(now)
            .bind(backup_id)
            .execute(pool)
            .await?;
            return Ok(());
        }
    }
    let mut tx = db::begin_immediate(pool).await?;
    let payload = crate::jobs::JobPayload {
        kind: "backup".into(),
        payload_version: 1,
        scope: json!({"backup_id": backup_id}),
        target: json!({}),
        actor_id: None,
    };
    let created = crate::jobs::enqueue(
        &mut tx,
        "backup",
        &format!("backup:{backup_id}"),
        &payload,
        None,
        5,
    )
    .await?;
    sqlx::query(
        "UPDATE backup_runs SET job_id = ?, state = 'queued', updated_at = ? WHERE id = ?",
    )
    .bind(&created)
    .bind(now)
    .bind(backup_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn finish_signed_backups(
    pool: &SqlitePool,
    backup_root: &std::path::Path,
    keys_root: &PathBuf,
) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, manifest_digest FROM backup_runs
         WHERE state IN ('queued', 'running') AND manifest_digest IS NOT NULL
         ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut finished = Vec::new();
    for (id, digest) in rows {
        if try_finish_signed(pool, &id, &digest, &backup_root.join(&id), keys_root).await? {
            finished.push(id);
        }
    }
    Ok(finished)
}

async fn try_finish_signed(
    pool: &SqlitePool,
    backup_id: &str,
    digest: &str,
    dest: &std::path::Path,
    keys_root: &PathBuf,
) -> Result<bool> {
    let Ok(manifest) = std::fs::read(dest.join("manifest.json")) else {
        return Ok(false);
    };
    if sha256_hex(&manifest) != digest {
        return Ok(false);
    }
    let Ok(complete) = std::fs::read_to_string(dest.join("COMPLETE")) else {
        return Ok(false);
    };
    if verify_complete(keys_root, backup_id, digest, &complete).is_err() {
        return Ok(false);
    }
    match commit_sealed_success(pool, backup_id).await {
        Ok(()) => Ok(true),
        Err(err) if err.to_string().contains("no longer running") => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn signed_complete(keys: &PathBuf, backup_id: &str, digest: &str) -> Result<String> {
    sign_complete(keys, backup_id, digest)
}

async fn resume_sealed_copy(state: &AppState, backup_id: &str, dest: &std::path::Path) -> Result<bool> {
    let digest: Option<String> = sqlx::query_as(
        "SELECT manifest_digest FROM backup_runs WHERE id = ? AND state IN ('queued', 'running')",
    )
    .bind(backup_id)
    .fetch_optional(&state.pool)
    .await?
    .and_then(|row: (Option<String>,)| row.0);
    let Some(digest) = digest else {
        return Ok(false);
    };
    if try_finish_signed(
        &state.pool,
        backup_id,
        &digest,
        dest,
        &state.config.keys_root(),
    )
    .await?
    {
        return Ok(true);
    }
    let manifest_bytes = std::fs::read(dest.join("manifest.json"))?;
    if sha256_hex(&manifest_bytes) != digest {
        anyhow::bail!("sealed manifest mismatch");
    }
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    for item in manifest["items"].as_array().unwrap_or(&Vec::new()) {
        let blob_id = item["blob_id"].as_str().ok_or_else(|| anyhow!("manifest blob"))?;
        let sha = item["sha256"].as_str().ok_or_else(|| anyhow!("manifest sha"))?;
        let size = item["size"].as_i64().ok_or_else(|| anyhow!("manifest size"))?;
        copy_backup_object(state, backup_id, dest, blob_id, size, sha).await?;
    }
    let signed = sign_complete(&state.config.keys_root(), backup_id, &digest)?;
    std::fs::write(dest.join("COMPLETE"), signed)?;
    commit_sealed_success(&state.pool, backup_id).await?;
    Ok(true)
}

pub async fn copy_backup_object(
    state: &AppState,
    backup_id: &str,
    dest: &std::path::Path,
    blob_id: &str,
    size: i64,
    sha: &str,
) -> Result<&'static str> {
    let stable_key = sha256_hex(blob_id.as_bytes());
    let stable_path = dest.join(&stable_key);
    let existing = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT state, plaintext_sha256, ciphertext_sha256, ciphertext_key
         FROM backup_items WHERE backup_id = ? AND blob_id = ?",
    )
    .bind(backup_id)
    .bind(blob_id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((item_state, stored_sha, ciphertext_sha, _)) = &existing {
        if item_state == "verified" && stored_sha == sha && stable_path.is_file() {
            if let Some(ciphertext_sha) = ciphertext_sha {
                let bytes = std::fs::read(&stable_path)?;
                if sha256_hex(&bytes) == *ciphertext_sha {
                    add_verify_stats(&state.pool, backup_id, 0, bytes.len() as i64, size).await?;
                    return Ok("reused");
                }
            }
        }
    }
    let attempt_key = if stable_path.is_file() {
        stable_key.clone()
    } else {
        new_id()
    };
    sqlx::query("INSERT OR IGNORE INTO backup_pins(backup_id, blob_id) VALUES(?, ?)")
        .bind(backup_id)
        .bind(blob_id)
        .execute(&state.pool)
        .await?;
    let media = state.media().await?;
    let (key, _, _, _) = crate::storage::location_for(&state.pool, blob_id, &media.backend_id).await?;
    let bytes = media.get_bytes(&key).await?;
    if crate::storage::sha_of(&bytes) != sha {
        anyhow::bail!("backup hash mismatch for {blob_id}");
    }
    let ciphertext = age_encrypt_bytes(&state.config.keys_root(), &bytes)?;
    let ciphertext_sha = sha256_hex(&ciphertext);
    sqlx::query(
        "INSERT INTO backup_items(backup_id, blob_id, ciphertext_key, plaintext_sha256, ciphertext_sha256, size, state)
         VALUES(?, ?, ?, ?, ?, ?, 'uploading')
         ON CONFLICT(backup_id, blob_id) DO UPDATE SET
            ciphertext_key = excluded.ciphertext_key,
            plaintext_sha256 = excluded.plaintext_sha256,
            ciphertext_sha256 = excluded.ciphertext_sha256,
            size = excluded.size,
            state = 'uploading'",
    )
    .bind(backup_id)
    .bind(blob_id)
    .bind(&attempt_key)
    .bind(sha)
    .bind(&ciphertext_sha)
    .bind(size)
    .execute(&state.pool)
    .await?;
    if let Some(parent) = stable_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if attempt_key == stable_key {
        std::fs::write(&stable_path, &ciphertext)?;
    } else {
        let attempt_dir = dest.join("attempts");
        std::fs::create_dir_all(&attempt_dir)?;
        let attempt_path = attempt_dir.join(&attempt_key);
        std::fs::write(&attempt_path, &ciphertext)?;
        std::fs::rename(&attempt_path, &stable_path)?;
    }
    let read_back = std::fs::read(&stable_path)?;
    if sha256_hex(&read_back) != ciphertext_sha {
        anyhow::bail!("backup readback mismatch");
    }
    sqlx::query(
        "UPDATE backup_items SET state = 'verified' WHERE backup_id = ? AND blob_id = ?",
    )
    .bind(backup_id)
    .bind(blob_id)
    .execute(&state.pool)
    .await?;
    add_verify_stats(
        &state.pool,
        backup_id,
        ciphertext.len() as i64,
        read_back.len() as i64,
        size,
    )
    .await?;
    Ok(if attempt_key == stable_key { "retried" } else { "copied" })
}

async fn add_verify_stats(
    pool: &SqlitePool,
    backup_id: &str,
    written: i64,
    read_back: i64,
    verified: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE backup_runs SET
            bytes_written = bytes_written + ?,
            bytes_read_back = bytes_read_back + ?,
            bytes_verified = bytes_verified + ?,
            updated_at = ?
         WHERE id = ?",
    )
    .bind(written)
    .bind(read_back)
    .bind(verified)
    .bind(db::now_ms())
    .bind(backup_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn commit_sealed_success(pool: &SqlitePool, backup_id: &str) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE backup_runs SET state = 'succeeded', updated_at = ?
         WHERE id = ? AND manifest_digest IS NOT NULL AND state IN ('queued', 'running')",
    )
    .bind(db::now_ms())
    .bind(backup_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        anyhow::bail!("backup is no longer running");
    }
    sqlx::query("DELETE FROM backup_pins WHERE backup_id = ?")
        .bind(backup_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn abandon_incomplete_copy(pool: &SqlitePool, backup_id: &str, root: &std::path::Path) -> AppResult<()> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, manifest_digest FROM backup_runs WHERE id = ?",
    )
    .bind(backup_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.1.is_none() {
        return Err(AppError::conflict("BACKUP_UNSEALED", "capture is not sealed"));
    }
    if row.0 == "succeeded" || row.0 == "abandoned" {
        return Err(AppError::conflict("BACKUP_CLOSED", "backup already finished"));
    }
    let updated = sqlx::query(
        "UPDATE backup_runs SET state = 'abandoned', updated_at = ?
         WHERE id = ? AND state IN ('queued', 'running')",
    )
    .bind(db::now_ms())
    .bind(backup_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if updated.rows_affected() != 1 {
        return Err(AppError::conflict("BACKUP_CLOSED", "backup already finished"));
    }
    sqlx::query("DELETE FROM backup_pins WHERE backup_id = ?")
        .bind(backup_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let staging = root.join(backup_id);
    if staging.is_dir() {
        std::fs::remove_dir_all(staging).map_err(anyhow::Error::from)?;
    }
    Ok(())
}

pub async fn prune_retention(pool: &SqlitePool, root: &std::path::Path, now_ms: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, snapshot_at FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL
         ORDER BY snapshot_at DESC, created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut keep = std::collections::HashSet::new();
    if let Some((id, _)) = rows.first() {
        keep.insert(id.clone());
    }
    let mut seen_days = std::collections::HashSet::new();
    for (id, at) in &rows {
        let day = utc_day(*at);
        if seen_days.insert(day) && seen_days.len() <= 7 {
            keep.insert(id.clone());
        }
    }
    let mut seen_weeks = std::collections::HashSet::new();
    for (id, at) in &rows {
        let week = utc_week(*at);
        if seen_weeks.insert(week) && seen_weeks.len() <= 4 {
            keep.insert(id.clone());
        }
    }
    let windows = sqlx::query_as::<_, (Option<i64>, i64)>(
        "SELECT cutover_at, protected_until FROM storage_migrations
         WHERE stage IN ('protected', 'retiring') AND protected_until IS NOT NULL AND protected_until > ?",
    )
    .bind(now_ms)
    .fetch_all(pool)
    .await?;
    for (cutover_at, until) in windows {
        let prior: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM backup_runs
             WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND snapshot_at < ?
             ORDER BY snapshot_at DESC LIMIT 1",
        )
        .bind(cutover_at.unwrap_or(until - MIGRATION_PROTECT_MS))
        .fetch_optional(pool)
        .await?;
        if let Some((id,)) = prior {
            keep.insert(id);
        }
    }
    let mut expired = Vec::new();
    for (id, _) in &rows {
        if keep.contains(id) {
            continue;
        }
        sqlx::query("UPDATE backup_runs SET state = 'expired', updated_at = ? WHERE id = ? AND state = 'succeeded'")
            .bind(now_ms)
            .bind(id)
            .execute(pool)
            .await?;
        let dir = root.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        expired.push(id.clone());
    }
    Ok(expired)
}

const MIGRATION_PROTECT_MS: i64 = 14 * 24 * 3600 * 1000;

fn utc_day(ms: i64) -> i64 {
    ms.div_euclid(86_400_000)
}

fn utc_week(ms: i64) -> i64 {
    let date = chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .date_naive();
    let week = date.iso_week();
    week.year() as i64 * 100 + week.week() as i64
}

pub async fn run_restore(state: &AppState, payload_json: &str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let restore_id = payload["scope"]["restore_id"]
        .as_str()
        .ok_or_else(|| anyhow!("restore_id"))?;
    let backup_id = payload["scope"]["backup_id"]
        .as_str()
        .ok_or_else(|| anyhow!("backup_id"))?;
    let outcome = restore_verified(
        state,
        restore_id,
        backup_id,
        payload["scope"]["parent_backup_id"].as_str(),
        payload["scope"]["branch_id"].as_str(),
    )
    .await;
    if restore_canceled(&state.pool, restore_id).await? {
        return Ok(());
    }
    if let Err(err) = &outcome {
        sqlx::query("UPDATE restore_runs SET state = 'failed', updated_at = ? WHERE id = ? AND state != 'canceled'")
            .bind(db::now_ms())
            .bind(restore_id)
            .execute(&state.pool)
            .await?;
        return Err(anyhow!(err.to_string()));
    }
    sqlx::query("UPDATE restore_runs SET state = 'succeeded', updated_at = ? WHERE id = ? AND state != 'canceled'")
        .bind(db::now_ms())
        .bind(restore_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn restore_canceled(pool: &SqlitePool, restore_id: &str) -> Result<bool> {
    let state: Option<(String,)> = sqlx::query_as("SELECT state FROM restore_runs WHERE id = ?")
        .bind(restore_id)
        .fetch_optional(pool)
        .await?;
    Ok(state.as_ref().map(|row| row.0.as_str()) == Some("canceled"))
}

pub async fn get_restore(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, Option<String>, Option<i64>)>(
        "SELECT r.state, r.backup_id, r.branch_id, b.snapshot_at
         FROM restore_runs r
         LEFT JOIN backup_runs b ON b.id = r.backup_id
         WHERE r.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let (verified,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM restore_items WHERE restore_id = ? AND state = 'verified'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let (missing,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM restore_items WHERE restore_id = ? AND state = 'missing'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM backup_items WHERE backup_id = ?")
        .bind(&row.1)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (epoch,): (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(json!({
        "id": id,
        "state": row.0,
        "backup_id": row.1,
        "branch_id": row.2,
        "snapshot_at": row.3,
        "objects_verified": verified,
        "objects_missing": missing,
        "objects_total": total,
        "schema_stage": row.0,
        "routing_epoch": epoch
    }))
}

pub async fn abort_restore(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, Option<String>, i64)>(
        "SELECT r.state, r.job_id, route.routing_epoch
         FROM restore_runs r, storage_route route
         WHERE r.id = ? AND route.id = 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 == "succeeded" {
        return Err(AppError::conflict("RESTORE_DONE", "a finished restore stays on its isolated copy"));
    }
    let now = db::now_ms();
    sqlx::query("UPDATE restore_runs SET state = 'canceled', updated_at = ? WHERE id = ? AND state != 'succeeded'")
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(job_id) = row.1 {
        sqlx::query(
            "UPDATE jobs SET state = 'canceled', lease_token = NULL, lease_until = NULL, updated_at = ?
             WHERE id = ? AND state IN ('queued', 'running', 'retry_wait')",
        )
        .bind(now)
        .bind(job_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    let view = get_restore(&state.pool, actor, id).await?;
    if view["routing_epoch"] != row.2 {
        return Err(AppError::conflict("ROUTE_CHANGED", "abort changed the live route"));
    }
    Ok(view)
}

pub async fn promote_restore(
    state: &AppState,
    actor: &Actor,
    id: &str,
    expected_revision: i64,
    source_ack: &str,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if source_ack.trim().is_empty() {
        return Err(AppError::conflict(
            "HANDOFF_SOURCE",
            "confirm the old host can no longer write",
        ));
    }
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT state, backup_id FROM restore_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "succeeded" {
        return Err(AppError::conflict(
            "RESTORE_STATE",
            "promote opens writes only after the isolated restore succeeds",
        ));
    }
    let live_epoch: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let report_path = state.config.work_root().join("restore").join(id).join("report.json");
    let report: serde_json::Value = std::fs::read(&report_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| AppError::conflict("PROMOTE_REPORT", "the acceptance report is missing"))?;
    let missing = report["originals_missing"].as_array().map(|items| items.len()).unwrap_or(1);
    if report["integrity"] != "ok" || report["catalog_ok"] != true || missing > 0 {
        return Err(AppError::conflict(
            "PROMOTE_REPORT",
            "the acceptance report is not clear",
        ));
    }
    let isolated = crate::db::connect(&state.config.work_root().join("restore").join(id).join("gallery.sqlite"))
        .await
        .map_err(|_| AppError::conflict("PROMOTE_REPORT", "the isolated database is not ready"))?;
    let route = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT catalog_revision, write_barrier, routing_epoch FROM storage_route WHERE id = 1",
    )
    .fetch_one(&isolated)
    .await
    .map_err(anyhow::Error::from)?;
    if route.0 != expected_revision {
        return Err(AppError::precondition("REVISION_CONFLICT", "isolated catalog revision"));
    }
    if route.1 != 1 {
        return Err(AppError::conflict(
            "RESTORE_STATE",
            "the isolated copy is not write-isolated",
        ));
    }
    let now = db::now_ms();
    let mut tx = db::begin_immediate(&isolated).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0 WHERE id = 1 AND catalog_revision = ? AND write_barrier = 1",
    )
    .bind(expected_revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "restore_accepted_at", &now.to_string())
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "promote_source_ack", source_ack.trim())
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    let after: (i64,) = sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if after.0 != live_epoch.0 {
        return Err(AppError::conflict("ROUTE_CHANGED", "promote changed the live route"));
    }
    Ok(json!({
        "id": id,
        "backup_id": row.1,
        "writes": "open",
        "catalog_revision": expected_revision,
        "routing_epoch": route.2,
        "accepted_at": now
    }))
}

pub async fn resume_restore(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, job_id FROM restore_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "canceled" && row.0 != "failed" {
        return Err(AppError::conflict("RESTORE_STATE", "resume continues a canceled or failed restore"));
    }
    let now = db::now_ms();
    sqlx::query("UPDATE restore_runs SET state = 'queued', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(job_id) = row.1 {
        sqlx::query(
            "UPDATE jobs SET state = 'queued', available_at = ?, lease_token = NULL, lease_until = NULL, updated_at = ?
             WHERE id = ? AND state IN ('canceled', 'failed')",
        )
        .bind(now)
        .bind(now)
        .bind(job_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    get_restore(&state.pool, actor, id).await
}

pub async fn select_restore_branch(
    pool: &SqlitePool,
    backup_id: &str,
    branch_id: Option<&str>,
) -> AppResult<serde_json::Value> {
    let stored = sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT state, snapshot_at FROM backup_runs WHERE id = ?",
    )
    .bind(backup_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if stored.0 == "unavailable" {
        return Err(AppError::conflict(
            "BACKUP_UNAVAILABLE",
            "signature, hash, or database check failed",
        ));
    }
    let chosen = sqlx::query_as::<_, (String, i64, String, i64, i64, Option<String>)>(
        "SELECT id, snapshot_at, branch_id, COALESCE(catalog_revision, 0), COALESCE(routing_epoch, 0), instance_id
         FROM backup_runs
         WHERE id = ? AND state = 'succeeded' AND snapshot_at IS NOT NULL",
    )
    .bind(backup_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let library = db::instance_id(pool).await.map_err(anyhow::Error::from)?;
    if chosen.5.as_ref().is_some_and(|stored| stored != &library) {
        return Err(AppError::conflict(
            "LIBRARY_MISMATCH",
            "backup belongs to another library",
        ));
    }
    let head = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT id, snapshot_at, COALESCE(catalog_revision, 0) FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND branch_id = ?
           AND (instance_id IS NULL OR instance_id = ?)
         ORDER BY snapshot_at DESC, COALESCE(catalog_revision, 0) DESC, id DESC LIMIT 1",
    )
    .bind(&chosen.2)
    .bind(&library)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let requested = branch_id.map(str::trim).filter(|value| !value.is_empty());
    let behind = chosen.1 < head.1 || (chosen.1 == head.1 && chosen.3 < head.2);
    let (branch, branched) = if behind {
        let Some(name) = requested else {
            return Err(AppError::conflict(
                "BRANCH_REQUIRED",
                "an older snapshot opens a new restore branch",
            ));
        };
        if name == chosen.2 {
            return Err(AppError::conflict(
                "BRANCH_REQUIRED",
                "an older snapshot opens a new restore branch",
            ));
        }
        (name.to_string(), true)
    } else {
        if let Some(name) = requested {
            if name != chosen.2 {
                return Err(AppError::conflict(
                    "BRANCH_MATCH",
                    "the newest snapshot stays on its branch",
                ));
            }
        }
        (chosen.2.clone(), false)
    };
    Ok(json!({
        "parent_backup_id": chosen.0,
        "snapshot_at": chosen.1,
        "catalog_revision": chosen.3,
        "routing_epoch": chosen.4,
        "branch_id": branch,
        "branched": branched,
        "head_backup_id": head.0
    }))
}

async fn restore_verified(
    state: &AppState,
    restore_id: &str,
    backup_id: &str,
    parent_backup_id: Option<&str>,
    branch_id: Option<&str>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let src = state.config.backup_root().join(backup_id);
    let complete = std::fs::read_to_string(src.join("COMPLETE"))?;
    let manifest_bytes = std::fs::read(src.join("manifest.json"))?;
    let digest = sha256_hex(&manifest_bytes);
    verify_complete(&state.config.keys_root(), backup_id, &digest, &complete)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let isolated = state.config.work_root().join("restore").join(restore_id);
    std::fs::create_dir_all(isolated.join("media"))?;
    let sqlite_ct = std::fs::read(src.join("gallery.sqlite.age"))?;
    let sqlite_plain = age_decrypt_bytes(&state.config.keys_root(), &sqlite_ct)?;
    std::fs::write(isolated.join("gallery.sqlite"), &sqlite_plain)?;
    let check = crate::db::connect(&isolated.join("gallery.sqlite")).await?;
    let (integrity,): (String,) = sqlx::query_as("PRAGMA integrity_check").fetch_one(&check).await?;
    if integrity != "ok" {
        anyhow::bail!("sqlite integrity_check failed");
    }
    let violations = sqlx::query("PRAGMA foreign_key_check").fetch_all(&check).await?;
    if !violations.is_empty() {
        anyhow::bail!("sqlite foreign_key_check failed");
    }
    for item in manifest["items"].as_array().unwrap_or(&Vec::new()) {
        if restore_canceled(&state.pool, restore_id).await? {
            return Ok(());
        }
        let object = item["object"].as_str().ok_or_else(|| anyhow!("manifest object"))?;
        let sha = item["sha256"].as_str().ok_or_else(|| anyhow!("manifest sha"))?;
        let size = item["size"].as_i64().ok_or_else(|| anyhow!("manifest size"))?;
        let blob_id = item["blob_id"].as_str().unwrap_or(object);
        let already: Option<(String,)> = sqlx::query_as(
            "SELECT state FROM restore_items WHERE restore_id = ? AND blob_id = ?",
        )
        .bind(restore_id)
        .bind(blob_id)
        .fetch_optional(&state.pool)
        .await?;
        let restored_path = isolated.join("media").join(object);
        if already.as_ref().map(|row| row.0.as_str()) == Some("verified") {
            if let Ok(existing) = std::fs::read(&restored_path) {
                if existing.len() as i64 == size && sha256_hex(&existing) == sha {
                    continue;
                }
            }
        }
        let ciphertext = std::fs::read(src.join(object))?;
        let bytes = age_decrypt_bytes(&state.config.keys_root(), &ciphertext)?;
        if bytes.len() as i64 != size || sha256_hex(&bytes) != sha {
            sqlx::query(
                "INSERT INTO restore_items(restore_id, blob_id, state) VALUES(?, ?, 'missing')
                 ON CONFLICT(restore_id, blob_id) DO UPDATE SET state = 'missing'",
            )
            .bind(restore_id)
            .bind(blob_id)
            .execute(&state.pool)
            .await?;
            anyhow::bail!("backup object hash mismatch");
        }
        std::fs::write(&restored_path, &bytes)?;
        sqlx::query(
            "INSERT INTO restore_items(restore_id, blob_id, state) VALUES(?, ?, 'verified')
             ON CONFLICT(restore_id, blob_id) DO UPDATE SET state = 'verified'",
        )
        .bind(restore_id)
        .bind(blob_id)
        .execute(&state.pool)
        .await?;
    }
    let rebound = rebind_restored_media(&check, &isolated.join("media")).await?;
    let mut report = rebuild_restored_runtime(&check, restore_id, backup_id).await?;
    if let (Some(parent), Some(branch)) = (parent_backup_id, branch_id) {
        let mut tx = db::begin_immediate(&check).await?;
        db::kv_set(&mut tx, "parent_backup_id", parent).await?;
        db::kv_set(&mut tx, "recovery_branch", branch).await?;
        tx.commit().await?;
        report["parent_backup_id"] = json!(parent);
        report["branch_id"] = json!(branch);
    }
    let audit = audit_restored_originals(&check, &isolated.join("media")).await?;
    let catalog = catalog_constraints(&check).await?;
    report["snapshot_at"] = manifest["snapshot_at"].clone();
    report["target_backend_id"] = rebound["target_backend_id"].clone();
    report["locations_rebuilt"] = rebound["locations"].clone();
    report["old_backend_id"] = rebound["old_backend_id"].clone();
    report["old_path_used"] = json!(false);
    report["catalog_ok"] = catalog["ok"].clone();
    report["catalog_covers"] = catalog["covers"].clone();
    report["catalog_members"] = catalog["members"].clone();
    report["catalog_grants"] = catalog["grants"].clone();
    report["originals"] = audit["originals"].clone();
    report["originals_verified"] = audit["originals_verified"].clone();
    report["originals_missing"] = audit["originals_missing"].clone();
    report["originals_mismatched"] = audit["originals_mismatched"].clone();
    report["smoke"] = match smoke_restored_target(&check).await {
        Ok(smoke) => smoke,
        Err(err) => json!({
            "ok": false,
            "checked": 0,
            "writes": "isolated",
            "reason": smoke_reason(&err)
        }),
    };
    report["rto_ms"] = json!(started.elapsed().as_millis() as i64);
    let missing = report["originals_missing"].as_array().map(|items| items.len()).unwrap_or(0);
    let mismatched = report["originals_mismatched"].as_array().map(|items| items.len()).unwrap_or(0);
    if missing + mismatched > 0 {
        sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
            .execute(&check)
            .await?;
    }
    std::fs::write(
        isolated.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn smoke_reason(err: &anyhow::Error) -> &'static str {
    let text = err.to_string();
    if text.contains("hash") {
        "hash"
    } else if text.contains("route") {
        "route"
    } else {
        "read"
    }
}

async fn smoke_restored_target(pool: &SqlitePool) -> Result<serde_json::Value> {
    let route = sqlx::query_as::<_, (String, i64)>(
        "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    if route.0 != "restore-target" || route.1 != 1 {
        anyhow::bail!("smoke route");
    }
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT i.original_blob_id, b.sha256, b.size
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')
         ORDER BY i.original_blob_id",
    )
    .fetch_all(pool)
    .await?;
    let storage = crate::storage::open_by_id(pool, "restore-target").await?;
    let mut checked = 0i64;
    for (blob_id, sha, size) in rows {
        let bytes = storage
            .get_bytes(&sha256_hex(blob_id.as_bytes()))
            .await
            .map_err(|_| anyhow!("smoke read"))?;
        if bytes.len() as i64 != size || sha256_hex(&bytes) != sha {
            anyhow::bail!("smoke hash");
        }
        checked += 1;
    }
    Ok(json!({"ok": true, "checked": checked, "writes": "isolated"}))
}

pub async fn export_restore_report(
    state: &AppState,
    actor: &Actor,
    restore_id: &str,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let known: Option<(String,)> = sqlx::query_as("SELECT backup_id FROM restore_runs WHERE id = ?")
        .bind(restore_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let Some((backup_id,)) = known else {
        return Err(AppError::not_found());
    };
    let path = state
        .config
        .work_root()
        .join("restore")
        .join(restore_id)
        .join("report.json");
    let report: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| AppError::conflict("RESTORE_REPORT", "the recovery report is not ready"))?;
    let smoke = report.get("smoke").cloned().filter(|value| value.is_object());
    let rto_ms = report.get("rto_ms").and_then(|value| value.as_i64());
    let (Some(smoke), Some(rto_ms)) = (smoke, rto_ms) else {
        return Err(AppError::conflict("RESTORE_REPORT", "the recovery report is not ready"));
    };
    if report["integrity"] != "ok" || report["backup_id"] != backup_id {
        return Err(AppError::conflict("RESTORE_REPORT", "the recovery report is not ready"));
    }
    Ok(json!({
        "restore_id": restore_id,
        "backup_id": backup_id,
        "snapshot_at": report["snapshot_at"],
        "integrity": report["integrity"],
        "catalog_ok": report["catalog_ok"],
        "originals_missing": report["originals_missing"],
        "originals_mismatched": report["originals_mismatched"],
        "originals_verified": report["originals_verified"],
        "target_backend_id": report["target_backend_id"],
        "routing_epoch": report["routing_epoch"],
        "writes": report["writes"],
        "variants_queued": report["variants_queued"],
        "search_job": report["search_job"],
        "smoke": smoke,
        "rto_ms": rto_ms
    }))
}

pub async fn rebind_restored_media(
    pool: &SqlitePool,
    media_dir: &std::path::Path,
) -> Result<serde_json::Value> {
    let old: (String,) = sqlx::query_as("SELECT active_backend_id FROM storage_route WHERE id = 1")
        .fetch_one(pool)
        .await?;
    let now = db::now_ms();
    let cfg = serde_json::json!({"kind": "local", "root": media_dir}).to_string();
    let mut tx = db::begin_immediate(pool).await?;
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, config_revision, created_at)
         VALUES('restore-target', 'local', ?, 1, ?)
         ON CONFLICT(id) DO UPDATE SET kind = 'local', config_json = excluded.config_json",
    )
    .bind(&cfg)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT i.original_blob_id, b.sha256, b.size
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')
         ORDER BY i.original_blob_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut rebuilt = 0i64;
    for (blob_id, sha, size) in rows {
        let key = sha256_hex(blob_id.as_bytes());
        let bytes = match std::fs::read(media_dir.join(&key)) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if bytes.len() as i64 != size || sha256_hex(&bytes) != sha {
            anyhow::bail!("restored original hash mismatch");
        }
        sqlx::query(
            "INSERT INTO media_locations(
                blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at
             ) VALUES(?, 'restore-target', ?, 1, ?, ?, 'verified', ?)
             ON CONFLICT(blob_id, backend_id) DO UPDATE SET
                object_key = excluded.object_key,
                size = excluded.size,
                sha256 = excluded.sha256,
                state = 'verified',
                verified_at = excluded.verified_at",
        )
        .bind(&blob_id)
        .bind(&key)
        .bind(size)
        .bind(&sha)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        rebuilt += 1;
    }
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = 'restore-target', write_barrier = 1 WHERE id = 1",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({
        "target_backend_id": "restore-target",
        "locations": rebuilt,
        "old_backend_id": old.0
    }))
}

pub async fn audit_restored_originals(
    pool: &SqlitePool,
    media_dir: &std::path::Path,
) -> Result<serde_json::Value> {
    let rows = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT i.original_blob_id, b.sha256, b.size
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed')
         ORDER BY i.original_blob_id",
    )
    .fetch_all(pool)
    .await?;
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    let mut verified = 0i64;
    for (blob_id, sha, size) in &rows {
        let path = media_dir.join(sha256_hex(blob_id.as_bytes()));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                missing.push(blob_id.clone());
                continue;
            }
        };
        if bytes.len() as i64 != *size || sha256_hex(&bytes) != *sha {
            mismatched.push(blob_id.clone());
        } else {
            verified += 1;
        }
    }
    Ok(json!({
        "originals": rows.len(),
        "originals_verified": verified,
        "originals_missing": missing,
        "originals_mismatched": mismatched
    }))
}

pub async fn catalog_constraints(pool: &SqlitePool) -> Result<serde_json::Value> {
    let covers = sqlx::query_as::<_, (String, String)>(
        "SELECT a.id, a.cover_image_id FROM albums a
         WHERE a.deleted_at IS NULL AND a.cover_image_id IS NOT NULL
           AND NOT EXISTS (
             SELECT 1 FROM album_images ai
             WHERE ai.album_id = a.id AND ai.image_id = a.cover_image_id
           )
         ORDER BY a.id",
    )
    .fetch_all(pool)
    .await?;
    let members = sqlx::query_as::<_, (String, String)>(
        "SELECT ai.album_id, ai.image_id FROM album_images ai
         JOIN images i ON i.id = ai.image_id
         WHERE i.state IN ('purged', 'purging')
         ORDER BY ai.album_id, ai.image_id",
    )
    .fetch_all(pool)
    .await?;
    let grants = sqlx::query_as::<_, (String, String)>(
        "SELECT g.album_id, g.actor_id FROM album_grants g
         JOIN albums a ON a.id = g.album_id
         WHERE a.deleted_at IS NOT NULL
            OR (g.browse = 0 AND (g.edit = 1 OR g.upload = 1 OR g.view_original = 1 OR g.download = 1))
         ORDER BY g.album_id, g.actor_id",
    )
    .fetch_all(pool)
    .await?;
    let covers: Vec<_> = covers
        .into_iter()
        .map(|(album_id, image_id)| json!({"album_id": album_id, "image_id": image_id}))
        .collect();
    let members: Vec<_> = members
        .into_iter()
        .map(|(album_id, image_id)| json!({"album_id": album_id, "image_id": image_id}))
        .collect();
    let grants: Vec<_> = grants
        .into_iter()
        .map(|(album_id, actor_id)| json!({"album_id": album_id, "actor_id": actor_id}))
        .collect();
    Ok(json!({
        "ok": covers.is_empty() && members.is_empty() && grants.is_empty(),
        "covers": covers,
        "members": members,
        "grants": grants
    }))
}

pub async fn accept_isolated_restore(
    pool: &SqlitePool,
    actor: &Actor,
    media_dir: &std::path::Path,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let audit = audit_restored_originals(pool, media_dir).await?;
    let catalog = catalog_constraints(pool).await?;
    let missing = audit["originals_missing"].as_array().map(|items| items.len()).unwrap_or(0);
    let mismatched = audit["originals_mismatched"].as_array().map(|items| items.len()).unwrap_or(0);
    if missing + mismatched > 0 {
        sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
            .execute(pool)
            .await
            .map_err(anyhow::Error::from)?;
        return Err(AppError::conflict(
            "RESTORE_MISSING",
            "required originals are missing; writes stay isolated",
        ));
    }
    if catalog["ok"] != true {
        sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
            .execute(pool)
            .await
            .map_err(anyhow::Error::from)?;
        return Err(AppError::conflict(
            "RESTORE_CATALOG",
            "album membership, cover, or grant constraints failed; writes stay isolated",
        ));
    }
    if db::source_readonly(pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict(
            "SOURCE_READONLY",
            "the source stays read-only until handoff finishes",
        ));
    }
    let now = db::now_ms();
    let mut tx = db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0 WHERE id = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "restore_accepted_at", &now.to_string())
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(json!({
        "writes": "open",
        "accepted_at": now,
        "originals_verified": audit["originals_verified"],
        "next": "backup"
    }))
}

pub async fn readonly_entry(pool: &SqlitePool) -> AppResult<serde_json::Value> {
    let frozen = db::source_readonly(pool).await.map_err(anyhow::Error::from)?;
    let frozen_at = db::kv_get(pool, "source_frozen_at")
        .await
        .map_err(anyhow::Error::from)?
        .and_then(|value| value.parse::<i64>().ok());
    Ok(json!({
        "mode": if frozen { "readonly" } else { "open" },
        "reads": "open",
        "writes": if frozen { "blocked" } else { "open" },
        "frozen_at": frozen_at
    }))
}

pub async fn freeze_source(pool: &SqlitePool, actor: &Actor) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let now = db::now_ms();
    let instance = db::instance_id(pool).await.map_err(anyhow::Error::from)?;
    let ack = sha256_hex(format!("{instance}:{now}").as_bytes());
    let mut tx = db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "source_readonly", "1")
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "source_frozen_at", &now.to_string())
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "source_readonly_ack", &ack)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(json!({
        "writes": "readonly",
        "frozen_at": now,
        "ack": ack
    }))
}

pub async fn finish_handoff(
    pool: &SqlitePool,
    actor: &Actor,
    source_ack: &str,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if db::write_barrier(pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict(
            "RESTORE_ISOLATED",
            "writes are still isolated",
        ));
    }
    let accepted = db::kv_get(pool, "restore_accepted_at")
        .await
        .map_err(anyhow::Error::from)?
        .and_then(|value| value.parse::<i64>().ok());
    let Some(accepted_at) = accepted else {
        return Err(AppError::conflict(
            "HANDOFF_ACCEPT",
            "accept the restore before the new backup",
        ));
    };
    if let Some(done) = db::kv_get(pool, "handoff_backup_id")
        .await
        .map_err(anyhow::Error::from)?
        .filter(|value| !value.is_empty())
    {
        let completed = db::kv_get(pool, "handoff_completed_at")
            .await
            .map_err(anyhow::Error::from)?
            .and_then(|value| value.parse::<i64>().ok());
        return Ok(json!({
            "writes": "open",
            "backup_id": done,
            "accepted_at": accepted_at,
            "completed_at": completed
        }));
    }
    let backup = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, snapshot_at FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND snapshot_at >= ?
         ORDER BY snapshot_at DESC LIMIT 1",
    )
    .bind(accepted_at)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((backup_id, snapshot_at)) = backup else {
        return Err(AppError::conflict(
            "HANDOFF_BACKUP",
            "the new environment needs a succeeded backup",
        ));
    };
    if source_ack.trim().is_empty() {
        return Err(AppError::conflict(
            "HANDOFF_SOURCE",
            "confirm the old host can no longer write",
        ));
    }
    let now = db::now_ms();
    let mut tx = db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "handoff_source_ack", source_ack.trim())
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "handoff_backup_id", &backup_id)
        .await
        .map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, "handoff_completed_at", &now.to_string())
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(json!({
        "writes": "open",
        "backup_id": backup_id,
        "snapshot_at": snapshot_at,
        "accepted_at": accepted_at,
        "completed_at": now
    }))
}

pub async fn drill_smoke(state: &AppState, actor: &Actor) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if !db::write_barrier(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict(
            "DRILL_REQUIRED",
            "smoke runs while writes stay isolated",
        ));
    }
    let (images_before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (albums_before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM albums")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (token, session) = crate::auth::issue_session(&state.pool, actor, 60_000)
        .await
        .map_err(anyhow::Error::from)?;
    let login_ok = crate::auth::lookup_session(&state.pool, &token)
        .await
        .map_err(anyhow::Error::from)?
        .is_some();
    let sample = synthetic_png().map_err(anyhow::Error::from)?;
    let smoke_id = new_id();
    let key = format!("work/drill/{smoke_id}.bin");
    let thumb_key = format!("work/drill/{smoke_id}-thumb.png");
    let media = state.media().await.map_err(anyhow::Error::from)?;
    let drilled = run_drill_sample(state, actor, &media, &key, &thumb_key, &sample).await;
    let _ = media.delete_key(&key).await;
    let _ = media.delete_key(&thumb_key).await;
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(db::now_ms())
        .bind(&session.id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (images_after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (albums_after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM albums")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if images_after != images_before || albums_after != albums_before {
        return Err(AppError::conflict(
            "SAMPLE_LEFT",
            "the drill sample is still in the library",
        ));
    }
    if !db::write_barrier(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict(
            "DRILL_REQUIRED",
            "smoke runs while writes stay isolated",
        ));
    }
    let mut report = drilled?;
    report["login"] = json!(if login_ok { "ok" } else { "failed" });
    report["writes"] = json!("isolated");
    report["sample_removed"] = json!(media.get_bytes(&key).await.is_err());
    if !login_ok {
        return Err(AppError::conflict("DRILL_LOGIN", "owner session did not open"));
    }
    Ok(report)
}

async fn run_drill_sample(
    state: &AppState,
    actor: &Actor,
    media: &crate::storage::Storage,
    key: &str,
    thumb_key: &str,
    sample: &[u8],
) -> AppResult<serde_json::Value> {
    media
        .put_bytes(key, sample, "image/png")
        .await
        .map_err(anyhow::Error::from)?;
    let original = media.get_bytes(key).await.map_err(anyhow::Error::from)?;
    if original != sample {
        return Err(AppError::conflict("DRILL_ORIGINAL", "synthetic original did not read back"));
    }
    let ranged = media
        .backend()
        .open(key, Some((0, 0)))
        .await
        .map_err(anyhow::Error::from)?;
    if ranged.first().copied() != sample.first().copied() {
        return Err(AppError::conflict("DRILL_DOWNLOAD", "phone range did not match the sample"));
    }
    let thumb = synthetic_thumb(sample).map_err(anyhow::Error::from)?;
    let probed = crate::processing::probe_bytes(&thumb).map_err(anyhow::Error::from)?;
    if probed.width > 4 || probed.height > 4 {
        return Err(AppError::conflict("DRILL_THUMB", "thumbnail is larger than the sample edge"));
    }
    media
        .put_bytes(thumb_key, &thumb, "image/png")
        .await
        .map_err(anyhow::Error::from)?;
    let archived = zip_sample(sample).map_err(anyhow::Error::from)?;
    if !archived.starts_with(b"PK") {
        return Err(AppError::conflict("DRILL_ARCHIVE", "desktop archive is not a zip"));
    }
    let mut tx = db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let job_id = new_id();
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO jobs(id, kind, dedupe_key, payload_json, scope_json, state, progress_done, progress_total, available_at, created_at, updated_at)
         VALUES(?, 'drill_smoke', ?, '{}', '{}', 'running', 0, 1, ?, ?, ?)",
    )
    .bind(&job_id)
    .bind(format!("drill:{job_id}"))
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE jobs SET progress_done = 1, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&job_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let progress: (i64, i64) = sqlx::query_as("SELECT progress_done, progress_total FROM jobs WHERE id = ?")
        .bind(&job_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let album_id = format!("drill-{job_id}");
    sqlx::query("INSERT INTO albums(id, name, created_at) VALUES(?, 'drill', ?)")
        .bind(&album_id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO album_grants(album_id, actor_id, browse, edit) VALUES(?, ?, 1, 0)",
    )
    .bind(&album_id)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let granted: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album_grants WHERE album_id = ?")
        .bind(&album_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("DELETE FROM album_grants WHERE album_id = ? AND actor_id = ?")
        .bind(&album_id)
        .bind(&actor.id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let left: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM album_grants WHERE album_id = ?")
        .bind(&album_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.rollback().await.map_err(anyhow::Error::from)?;
    if granted.0 != 1 || left.0 != 0 || progress != (1, 1) {
        return Err(AppError::conflict("DRILL_GRANT", "grant revoke or queue progress did not complete"));
    }
    Ok(json!({
        "upload": "ok",
        "thumbnail": "ok",
        "original": "ok",
        "archive": "ok",
        "phone_download": "ok",
        "queue_progress": {"done": progress.0, "total": progress.1},
        "grant_revoked": true
    }))
}

fn synthetic_png() -> Result<Vec<u8>> {
    let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([12, 80, 160])));
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|err| anyhow!(err))?;
    Ok(bytes)
}

fn synthetic_thumb(sample: &[u8]) -> Result<Vec<u8>> {
    let decoded = image::load_from_memory(sample)?;
    let thumb = image::imageops::resize(&decoded, 4, 4, image::imageops::FilterType::Triangle);
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(thumb)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|err| anyhow!(err))?;
    Ok(bytes)
}

fn zip_sample(sample: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("sample.png", options)?;
        zip.write_all(sample)?;
        zip.finish()?;
    }
    Ok(cursor.into_inner())
}

pub async fn rebuild_restored_runtime(
    pool: &SqlitePool,
    restore_id: &str,
    backup_id: &str,
) -> Result<serde_json::Value> {
    crate::versions::ensure_default_profile(pool).await?;
    let now = db::now_ms();
    let mut tx = db::begin_immediate(pool).await?;
    let revoked = sqlx::query("UPDATE sessions SET revoked_at = ? WHERE revoked_at IS NULL")
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query(
        "UPDATE storage_route SET routing_epoch = routing_epoch + 1, write_barrier = 1 WHERE id = 1",
    )
    .execute(&mut *tx)
    .await?;
    let policy: Option<(String,)> =
        sqlx::query_as("SELECT value FROM schema_app WHERE key = 'policy_epoch'")
            .fetch_optional(&mut *tx)
            .await?;
    let next_policy = policy
        .and_then(|(value,)| value.parse::<i64>().ok())
        .unwrap_or(1)
        + 1;
    db::kv_set(&mut tx, "policy_epoch", &next_policy.to_string()).await?;
    sqlx::query(
        "UPDATE archive_packages SET archive_state = 'stale'
         WHERE archive_state IN ('queued', 'building', 'ready')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE jobs SET state = 'failed', failure_code = 'NEEDS_REVIEW', lease_token = NULL, lease_until = NULL, updated_at = ?
         WHERE kind IN ('backup', 'restore', 'storage_migrate_copy', 'storage_retire', 'purge_media', 'agent_run')
           AND state IN ('queued', 'running', 'retry_wait')",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE jobs SET state = 'queued', lease_token = NULL, lease_until = NULL, available_at = ?, updated_at = ?
         WHERE state = 'running'",
    )
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let leases = sqlx::query(
        "UPDATE jobs SET lease_token = NULL, lease_until = NULL, updated_at = ?
         WHERE state IN ('queued', 'retry_wait', 'cancel_requested')
           AND (lease_token IS NOT NULL OR lease_until IS NOT NULL)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let uploads = sqlx::query(
        "UPDATE upload_items SET state = 'canceled', failure_code = 'RESTORED'
         WHERE state IN ('waiting', 'receiving', 'validating')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE upload_batches SET state = 'canceled', updated_at = ?
         WHERE state IN ('waiting', 'running')",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let claims = sqlx::query("UPDATE download_claims SET expires_at = ? WHERE expires_at > ?")
        .bind(now - 1)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let variants = crate::versions::queue_missing_variants(&mut tx).await?;
    let search_payload = crate::jobs::JobPayload {
        kind: "rebuild_search".into(),
        payload_version: 1,
        scope: json!({"restore_id": restore_id}),
        target: json!({}),
        actor_id: None,
    };
    let search_job = crate::jobs::enqueue_protected(
        &mut tx,
        "rebuild_search",
        &format!("rebuild_search:restore:{restore_id}"),
        &search_payload,
        None,
        20,
    )
    .await?;
    db::kv_set(&mut tx, "search_index_ready", "0").await?;
    let sessions = sqlx::query(
        "UPDATE agent_sessions SET state = 'needs_reconcile', revision = revision + 1, updated_at = ?
         WHERE state IN ('idle', 'active', 'stopping')",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let turns = sqlx::query(
        "UPDATE agent_turns SET state = 'needs_reconcile', revision = revision + 1
         WHERE state IN ('running', 'stop_requested', 'queued', 'active')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE agent_actions SET state = 'needs_reconcile'
         WHERE state IN ('proposed', 'approved', 'dispatching')",
    )
    .execute(&mut *tx)
    .await?;
    let (routing_epoch,): (i64,) =
        sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    let (images,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images")
        .fetch_one(&mut *tx)
        .await?;
    let (albums,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM albums WHERE deleted_at IS NULL")
        .fetch_one(&mut *tx)
        .await?;
    let (trashed,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM images WHERE state = 'trashed'")
        .fetch_one(&mut *tx)
        .await?;
    let instance_id = sqlx::query_as::<_, (String,)>("SELECT value FROM schema_app WHERE key = 'instance_id'")
        .fetch_optional(&mut *tx)
        .await?
        .map(|(value,)| value);
    tx.commit().await?;
    Ok(json!({
        "restore_id": restore_id,
        "backup_id": backup_id,
        "instance_id": instance_id,
        "integrity": "ok",
        "foreign_keys": "ok",
        "images": images,
        "albums": albums,
        "trashed": trashed,
        "writes": "isolated",
        "sessions_revoked": revoked,
        "routing_epoch": routing_epoch,
        "policy_epoch": next_policy,
        "uploads_canceled": uploads,
        "claims_expired": claims,
        "leases_cleared": leases,
        "variants_queued": variants,
        "search_job": search_job,
        "sessions_reviewed": sessions,
        "turns_reviewed": turns
    }))
}

pub async fn restore_gap(
    pool: &SqlitePool,
    desired_at: i64,
    backup_id: Option<&str>,
) -> AppResult<serde_json::Value> {
    let chosen = if let Some(id) = backup_id {
        sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>)>(
            "SELECT id, snapshot_at, catalog_revision, routing_epoch FROM backup_runs
             WHERE id = ? AND state = 'succeeded' AND snapshot_at IS NOT NULL",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?
    } else {
        sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>)>(
            "SELECT id, snapshot_at, catalog_revision, routing_epoch FROM backup_runs
             WHERE state = 'succeeded' AND snapshot_at IS NOT NULL
             ORDER BY snapshot_at DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?
    };
    let Some((backup_id, snapshot_at, backup_catalog, backup_epoch)) = chosen else {
        if backup_id.is_some() {
            return Err(AppError::not_found());
        }
        let (unbacked,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM images WHERE state IN ('active', 'trashed')",
        )
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
        return Ok(json!({
            "backup_id": null,
            "snapshot_at": null,
            "desired_at": desired_at,
            "gap_ms": null,
            "unbacked_images": unbacked
        }));
    };
    let lost = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, created_at FROM images
         WHERE state IN ('active', 'trashed') AND created_at > ?
         ORDER BY id",
    )
    .bind(snapshot_at)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let loss_sha = crate::ids::sha256_hex(
        lost.iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    let (current_catalog,): (i64,) =
        sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
            .fetch_one(pool)
            .await
            .map_err(anyhow::Error::from)?;
    let (current_epoch,): (i64,) =
        sqlx::query_as("SELECT routing_epoch FROM storage_route WHERE id = 1")
            .fetch_one(pool)
            .await
            .map_err(anyhow::Error::from)?;
    let shown: Vec<_> = lost
        .iter()
        .take(20)
        .map(|(id, created_at)| json!({"id": id, "created_at": created_at}))
        .collect();
    Ok(json!({
        "backup_id": backup_id,
        "snapshot_at": snapshot_at,
        "desired_at": desired_at,
        "gap_ms": (desired_at - snapshot_at).max(0),
        "unbacked_images": lost.len() as i64,
        "lost_images": shown,
        "loss_sha": loss_sha,
        "backup_catalog_revision": backup_catalog,
        "current_catalog_revision": current_catalog,
        "backup_routing_epoch": backup_epoch,
        "current_routing_epoch": current_epoch
    }))
}

pub async fn restore_plan(
    pool: &SqlitePool,
    actor: &Actor,
    backup_id: &str,
    branch_id: Option<&str>,
    confirm_loss: Option<&str>,
    dry_run: bool,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let gap = restore_gap(pool, db::now_ms(), Some(backup_id)).await?;
    if dry_run {
        require_loss_ack(&gap, confirm_loss)?;
    }
    let branch = select_restore_branch(pool, backup_id, branch_id).await?;
    Ok(json!({
        "mode": if dry_run { "dry_run" } else { "preview" },
        "backup_id": backup_id,
        "parent_backup_id": branch["parent_backup_id"],
        "branch_id": branch["branch_id"],
        "branched": branch["branched"],
        "snapshot_at": gap["snapshot_at"],
        "gap_ms": gap["gap_ms"],
        "unbacked_images": gap["unbacked_images"],
        "lost_images": gap["lost_images"],
        "loss_sha": gap["loss_sha"],
        "backup_catalog_revision": gap["backup_catalog_revision"],
        "current_catalog_revision": gap["current_catalog_revision"],
        "backup_routing_epoch": gap["backup_routing_epoch"],
        "current_routing_epoch": gap["current_routing_epoch"]
    }))
}

pub async fn assess_early_snapshot(
    pool: &SqlitePool,
    actor: &Actor,
    backup_root: &std::path::Path,
    salvage_root: &std::path::Path,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let migration = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, source_backend_id, target_backend_id, cutover_at
         FROM storage_migrations
         WHERE cutover_at IS NOT NULL
         ORDER BY cutover_at DESC
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((migration_id, source_id, _target_id, cutover_at)) = migration else {
        return Ok(json!({
            "situation": "no_cutover",
            "confirm_required": false,
            "restore_started": false
        }));
    };
    let backups = sqlx::query_as::<_, (String, i64, Option<i64>, Option<i64>)>(
        "SELECT id, snapshot_at, catalog_revision, routing_epoch
         FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL
         ORDER BY snapshot_at DESC, id DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some(later) = backups.iter().find(|row| row.1 >= cutover_at) {
        return Ok(json!({
            "situation": "post_cutover_backup",
            "confirm_required": false,
            "restore_started": false,
            "migration_id": migration_id,
            "cutover_at": cutover_at,
            "restore_point": {
                "backup_id": later.0,
                "snapshot_at": later.1,
                "catalog_revision": later.2,
                "routing_epoch": later.3
            }
        }));
    }
    let Some(early) = backups.first() else {
        return Ok(json!({
            "situation": "no_backup",
            "confirm_required": false,
            "restore_started": false,
            "migration_id": migration_id,
            "cutover_at": cutover_at
        }));
    };
    let (backup_id, snapshot_at, backup_catalog, backup_epoch) = early.clone();
    let route = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT active_backend_id, routing_epoch, catalog_revision FROM storage_route WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let current_id = route.0;
    let current_readable = backend_root_readable(pool, &current_id).await?;
    let source_readable = backend_root_readable(pool, &source_id).await?;
    let lost = sqlx::query_as::<_, (String, i64, String, String)>(
        "SELECT i.id, i.created_at, i.original_blob_id, b.sha256
         FROM images i JOIN media_blobs b ON b.id = i.original_blob_id
         WHERE i.state IN ('active', 'trashed') AND i.created_at > ?
         ORDER BY i.id",
    )
    .bind(snapshot_at)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let loss_sha = sha256_hex(
        lost.iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    let mut copied = Vec::new();
    let mut blocked = Vec::new();
    for (image_id, created_at, blob_id, sha) in &lost {
        match salvage_one(
            pool,
            salvage_root,
            &backup_id,
            &current_id,
            &source_id,
            current_readable,
            source_readable,
            blob_id,
            sha,
        )
        .await?
        {
            Salvage::Copied(from) => copied.push(json!({
                "id": image_id,
                "created_at": created_at,
                "blob_id": blob_id,
                "sha256": sha,
                "from": from
            })),
            Salvage::Blocked(reason) => blocked.push(json!({
                "id": image_id,
                "created_at": created_at,
                "blob_id": blob_id,
                "reason": reason
            })),
        }
    }
    let source_sample = sample_source(pool, &source_id, source_readable, snapshot_at).await?;
    let package = backup_package(backup_root, &backup_id);
    let shown: Vec<_> = lost
        .iter()
        .take(20)
        .map(|row| json!({"id": row.0, "created_at": row.1}))
        .collect();
    Ok(json!({
        "situation": "pre_cutover_only",
        "confirm_required": !lost.is_empty(),
        "restore_started": false,
        "migration_id": migration_id,
        "cutover_at": cutover_at,
        "restore_point": {
            "backup_id": backup_id,
            "snapshot_at": snapshot_at,
            "catalog_revision": backup_catalog,
            "routing_epoch": backup_epoch
        },
        "gap_ms": (db::now_ms() - snapshot_at).max(0),
        "unbacked_images": lost.len() as i64,
        "lost_images": shown,
        "loss_sha": loss_sha,
        "current_readable": current_readable,
        "source_checkable": source_sample["checkable"],
        "source_sample": source_sample["sample"],
        "backup_checkable": package.0,
        "backup_manifest_sha": package.1,
        "current_routing_epoch": route.1,
        "current_catalog_revision": route.2,
        "salvage_copied": copied,
        "salvage_blocked": blocked
    }))
}

enum Salvage {
    Copied(&'static str),
    Blocked(&'static str),
}

async fn salvage_one(
    pool: &SqlitePool,
    salvage_root: &std::path::Path,
    backup_id: &str,
    current_id: &str,
    source_id: &str,
    current_readable: bool,
    source_readable: bool,
    blob_id: &str,
    sha: &str,
) -> AppResult<Salvage> {
    let locations = sqlx::query_as::<_, (String,)>(
        "SELECT backend_id FROM media_locations
         WHERE blob_id = ? AND state = 'verified'
         ORDER BY backend_id",
    )
    .bind(blob_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut ordered: Vec<(&str, &'static str)> = Vec::new();
    if current_readable && locations.iter().any(|row| row.0 == current_id) {
        ordered.push((current_id, "current"));
    }
    if source_readable && source_id != current_id && locations.iter().any(|row| row.0 == source_id) {
        ordered.push((source_id, "source"));
    }
    for row in locations
        .iter()
        .filter(|row| row.0 != current_id && row.0 != source_id)
    {
        if backend_root_readable(pool, &row.0).await? {
            ordered.push((row.0.as_str(), "other"));
        }
    }
    let mut saw_hash = false;
    for (backend_id, from) in ordered {
        match read_verified(pool, backend_id, blob_id, sha).await {
            Ok(Some(bytes)) => {
                write_salvage(salvage_root, backup_id, blob_id, &bytes)?;
                return Ok(Salvage::Copied(from));
            }
            Ok(None) => {}
            Err(err) if err.to_string().contains("hash mismatch") => saw_hash = true,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(Salvage::Blocked(if saw_hash {
        "hash"
    } else {
        "endpoint unreadable"
    }))
}

async fn sample_source(
    pool: &SqlitePool,
    source_id: &str,
    source_readable: bool,
    snapshot_at: i64,
) -> AppResult<serde_json::Value> {
    if !source_readable {
        return Ok(json!({"checkable": false, "sample": null}));
    }
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT i.original_blob_id, b.sha256
         FROM images i
         JOIN media_blobs b ON b.id = i.original_blob_id
         JOIN media_locations l ON l.blob_id = b.id AND l.backend_id = ? AND l.state = 'verified'
         WHERE i.state IN ('active', 'trashed') AND i.created_at <= ?
         ORDER BY i.id
         LIMIT 1",
    )
    .bind(source_id)
    .bind(snapshot_at)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((blob_id, sha)) = row else {
        return Ok(json!({"checkable": true, "sample": null}));
    };
    let readable = match read_verified(pool, source_id, &blob_id, &sha).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) if err.to_string().contains("hash mismatch") => false,
        Err(err) => return Err(err.into()),
    };
    Ok(json!({
        "checkable": readable,
        "sample": if readable {
            json!({"blob_id": blob_id, "sha256": sha})
        } else {
            serde_json::Value::Null
        }
    }))
}

fn backup_package(backup_root: &std::path::Path, backup_id: &str) -> (bool, Option<String>) {
    if !safe_id(backup_id) {
        return (false, None);
    }
    let dir = backup_root.join(backup_id);
    let manifest = std::fs::read(dir.join("manifest.json")).ok().filter(|bytes| !bytes.is_empty());
    let sqlite = std::fs::read(dir.join("gallery.sqlite.age"))
        .ok()
        .filter(|bytes| !bytes.is_empty());
    let sha = manifest.as_ref().map(|bytes| sha256_hex(bytes));
    (manifest.is_some() || sqlite.is_some(), sha)
}

fn write_salvage(
    salvage_root: &std::path::Path,
    backup_id: &str,
    blob_id: &str,
    bytes: &[u8],
) -> AppResult<()> {
    if !safe_id(backup_id) || !safe_id(blob_id) {
        return Err(AppError::conflict("SALVAGE_ID", "backup and blob ids must be plain names"));
    }
    let dir = salvage_root.join(backup_id);
    std::fs::create_dir_all(&dir).map_err(|err| anyhow!(err))?;
    let path = dir.join(blob_id);
    if path.is_file() && std::fs::read(&path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let tmp = dir.join(format!("{blob_id}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|err| anyhow!(err))?;
    std::fs::rename(&tmp, &path).map_err(|err| anyhow!(err))?;
    Ok(())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && id != "."
}

async fn backend_root_readable(pool: &SqlitePool, backend_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT kind, config_json FROM storage_backends WHERE id = ?",
    )
    .bind(backend_id)
    .fetch_optional(pool)
    .await?;
    let Some((kind, json)) = row else {
        return Ok(false);
    };
    if kind == "local" {
        let cfg: crate::storage::BackendConfig = serde_json::from_str(&json)?;
        let crate::storage::BackendConfig::Local { root } = cfg else {
            return Ok(false);
        };
        return Ok(root.is_dir());
    }
    match crate::storage::open_by_id(pool, backend_id).await {
        Ok(storage) => Ok(storage.backend().probe().await.is_ok()),
        Err(_) => Ok(false),
    }
}

async fn read_verified(
    pool: &SqlitePool,
    backend_id: &str,
    blob_id: &str,
    sha: &str,
) -> Result<Option<Vec<u8>>> {
    let located = match crate::storage::location_for(pool, blob_id, backend_id).await {
        Ok(row) => row,
        Err(_) => return Ok(None),
    };
    let storage = crate::storage::open_by_id(pool, backend_id).await?;
    let bytes = match storage.get_bytes(&located.0).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if bytes.len() as i64 != located.1 || located.2 != sha || crate::storage::sha_of(&bytes) != sha {
        anyhow::bail!("backup source hash mismatch");
    }
    Ok(Some(bytes))
}

pub fn require_loss_ack(gap: &serde_json::Value, confirm: Option<&str>) -> AppResult<()> {
    if gap["unbacked_images"].as_i64().unwrap_or(0) > 0 && confirm != gap["loss_sha"].as_str() {
        return Err(AppError::conflict(
            "RESTORE_LOSS",
            "confirm the writes this restore point would drop",
        ));
    }
    Ok(())
}

const RPO_MS: i64 = 24 * 3600 * 1000;
const DRILL_MS: i64 = 30 * 24 * 3600 * 1000;
const PIN_HOLD_MS: i64 = 7 * 24 * 3600 * 1000;

pub fn backup_capacity_ok(free: Option<u64>, originals: u64) -> bool {
    free.is_none_or(|free| free >= originals)
}

pub async fn backup_health(
    pool: &SqlitePool,
    now: i64,
    free: Option<u64>,
) -> AppResult<serde_json::Value> {
    let latest = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, snapshot_at FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL
         ORDER BY snapshot_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM backup_runs WHERE state = 'succeeded'),
            (SELECT COUNT(*) FROM backup_runs WHERE state = 'failed' OR state = 'abandoned')",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let attempts = counts.0 + counts.1;
    let success_rate = if attempts == 0 {
        1.0
    } else {
        counts.0 as f64 / attempts as f64
    };
    let age = latest.as_ref().map(|row| now.saturating_sub(row.1));
    let rpo_exceeded = age.map(|age| age > RPO_MS).unwrap_or(true);
    let (pins,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM backup_pins p
         JOIN backup_runs b ON b.id = p.backup_id AND b.state = 'succeeded'
         WHERE b.snapshot_at IS NOT NULL AND b.snapshot_at <= ?",
    )
    .bind(now - PIN_HOLD_MS)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let drill: (Option<i64>,) = sqlx::query_as(
        "SELECT MAX(created_at) FROM restore_runs WHERE state = 'succeeded'",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let drill_at = drill.0;
    let drill_overdue = drill_at.map(|at| now.saturating_sub(at) > DRILL_MS).unwrap_or(true);
    let mut alerts = Vec::new();
    if rpo_exceeded {
        alerts.push("snapshot older than 24h");
    }
    if counts.1 > 0 {
        alerts.push("backup failures");
    }
    if pins > 0 {
        alerts.push("backup pins held");
    }
    if drill_overdue {
        alerts.push("restore drill overdue");
    }
    if free.is_some_and(|free| free < 1) {
        alerts.push("backup capacity");
    }
    Ok(json!({
        "snapshot_id": latest.as_ref().map(|row| &row.0),
        "snapshot_at": latest.as_ref().map(|row| row.1),
        "snapshot_age_ms": age,
        "rpo_ms": RPO_MS,
        "rpo_exceeded": rpo_exceeded,
        "success_rate": success_rate,
        "failures": counts.1,
        "pins_held": pins,
        "last_drill_at": drill_at,
        "free_bytes": free,
        "alerts": alerts
    }))
}

fn backup_phase(state: &str, snapshot_at: Option<i64>, verified_at: Option<i64>, ready: bool) -> &'static str {
    match state {
        "succeeded" if verified_at.is_some() && ready => "ready",
        "succeeded" if verified_at.is_some() => "verified",
        "succeeded" => "complete",
        "unavailable" => "unavailable",
        _ if snapshot_at.is_some() => "copy",
        _ => "snapshot",
    }
}

async fn schema_version(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_as::<_, (i64,)>("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .map(|row| row.0)
}

pub async fn list_backups(
    pool: &SqlitePool,
    actor: &Actor,
    free: Option<u64>,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let rows = sqlx::query_as::<_, (String, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64, i64, i64, i64)>(
        "SELECT id, state, snapshot_at, catalog_revision, routing_epoch, verified_at, updated_at,
                bytes_written, bytes_read_back, bytes_verified
         FROM backup_runs ORDER BY snapshot_at IS NULL, snapshot_at DESC, created_at DESC LIMIT 50",
    )
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let ready_id = rows.iter().find(|row| row.1 == "succeeded" && row.5.is_some()).map(|row| row.0.clone());
    let schema = schema_version(pool).await.unwrap_or(0);
    let now = db::now_ms();
    Ok(json!({
        "items": rows.into_iter().map(|(id, state, at, catalog, epoch, verified_at, updated_at, written, read_back, verified)| {
            let ready = ready_id.as_deref() == Some(id.as_str());
            let phase = backup_phase(&state, at, verified_at, ready);
            json!({
                "id": id,
                "state": state,
                "phase": phase,
                "format_version": 1,
                "schema_version": schema,
                "snapshot_at": at,
                "completed_at": if state == "succeeded" || state == "unavailable" { Some(updated_at) } else { None },
                "catalog_revision": catalog,
                "routing_epoch": epoch,
                "verified_at": verified_at,
                "bytes_written": written,
                "bytes_read_back": read_back,
                "bytes_verified": verified
            })
        }).collect::<Vec<_>>(),
        "restore_gap": restore_gap(pool, now, None).await?,
        "health": backup_health(pool, now, free).await?,
        "next_cursor": null
    }))
}

pub async fn get_backup(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64, i64, i64, i64)>(
        "SELECT id, state, job_id, snapshot_at, catalog_revision, routing_epoch, verified_at, updated_at,
                bytes_written, bytes_read_back, bytes_verified
         FROM backup_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let head: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM backup_runs
         WHERE state = 'succeeded' AND verified_at IS NOT NULL AND snapshot_at IS NOT NULL
         ORDER BY snapshot_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let ready = head.as_ref().map(|item| item.0.as_str()) == Some(row.0.as_str());
    let schema = schema_version(pool).await.unwrap_or(0);
    Ok(json!({
        "id": row.0,
        "state": row.1,
        "phase": backup_phase(&row.1, row.3, row.6, ready),
        "job_id": row.2,
        "format_version": 1,
        "schema_version": schema,
        "snapshot_at": row.3,
        "completed_at": if row.1 == "succeeded" || row.1 == "unavailable" { Some(row.7) } else { None },
        "catalog_revision": row.4,
        "routing_epoch": row.5,
        "verified_at": row.6,
        "bytes_written": row.8,
        "bytes_read_back": row.9,
        "bytes_verified": row.10
    }))
}

pub async fn verify_backup(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT state FROM backup_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "succeeded" && row.0 != "unavailable" {
        return Err(AppError::conflict("BACKUP_STATE", "only a finished backup can be verified"));
    }
    let dest = state.config.backup_root().join(id);
    let mut exceptions = Vec::new();
    let manifest_bytes = match std::fs::read(dest.join("manifest.json")) {
        Ok(bytes) => bytes,
        Err(_) => {
            exceptions.push(json!({"code": "manifest"}));
            Vec::new()
        }
    };
    let digest = sha256_hex(&manifest_bytes);
    match std::fs::read_to_string(dest.join("COMPLETE")) {
        Ok(complete) => {
            if verify_complete(&state.config.keys_root(), id, &digest, &complete).is_err() {
                exceptions.push(json!({"code": "signature"}));
            }
        }
        Err(_) => exceptions.push(json!({"code": "signature"})),
    }
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap_or(json!({}));
    let temp = state.config.work_root().join("verify").join(format!("{id}.sqlite"));
    if let Some(parent) = temp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::read(dest.join("gallery.sqlite.age")) {
        Ok(ciphertext) => match age_decrypt_bytes(&state.config.keys_root(), &ciphertext) {
            Ok(plain) => {
                if std::fs::write(&temp, &plain).is_err() {
                    exceptions.push(json!({"code": "sqlite"}));
                } else if let Ok(pool) = crate::db::connect(&temp).await {
                    let integrity: Result<(String,), _> =
                        sqlx::query_as("PRAGMA integrity_check").fetch_one(&pool).await;
                    if integrity.map(|row| row.0).unwrap_or_default() != "ok" {
                        exceptions.push(json!({"code": "sqlite"}));
                    }
                    let foreign = sqlx::query("PRAGMA foreign_key_check").fetch_all(&pool).await;
                    if foreign.map(|rows| !rows.is_empty()).unwrap_or(true) {
                        exceptions.push(json!({"code": "foreign_keys"}));
                    }
                } else {
                    exceptions.push(json!({"code": "sqlite"}));
                }
            }
            Err(_) => exceptions.push(json!({"code": "decrypt"})),
        },
        Err(_) => exceptions.push(json!({"code": "sqlite"})),
    }
    let _ = std::fs::remove_file(&temp);
    let mut objects = 0_i64;
    let mut verified = 0_i64;
    for item in manifest["items"].as_array().cloned().unwrap_or_default() {
        objects += 1;
        let Some(blob_id) = item["blob_id"].as_str() else {
            exceptions.push(json!({"code": "manifest"}));
            continue;
        };
        let object = item["object"].as_str().unwrap_or("").to_string();
        let name = if object.is_empty() {
            sha256_hex(blob_id.as_bytes())
        } else {
            object
        };
        let ciphertext = match std::fs::read(dest.join(&name)) {
            Ok(bytes) => bytes,
            Err(_) => {
                exceptions.push(json!({"code": "object_missing", "blob_id": blob_id}));
                continue;
            }
        };
        let plain = match age_decrypt_bytes(&state.config.keys_root(), &ciphertext) {
            Ok(bytes) => bytes,
            Err(_) => {
                exceptions.push(json!({"code": "decrypt", "blob_id": blob_id}));
                continue;
            }
        };
        let size = item["size"].as_i64().unwrap_or(-1);
        let sha = item["sha256"].as_str().unwrap_or("");
        if plain.len() as i64 != size || sha256_hex(&plain) != sha {
            exceptions.push(json!({"code": "object_hash", "blob_id": blob_id}));
        } else {
            verified += 1;
        }
    }
    let now = db::now_ms();
    let ok = exceptions.is_empty();
    if ok {
        sqlx::query("UPDATE backup_runs SET state = 'succeeded', verified_at = ?, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(now)
            .bind(id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    } else {
        sqlx::query(
            "UPDATE backup_runs SET state = 'unavailable', verified_at = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(json!({
        "id": id,
        "ok": ok,
        "state": if ok { "succeeded" } else { "unavailable" },
        "verified_at": if ok { Some(now) } else { None::<i64> },
        "objects": objects,
        "objects_verified": verified,
        "exceptions": exceptions
    }))
}

pub fn write_complete_marker(dir: &std::path::Path, digest: &str) -> Result<()> {
    let mut f = std::fs::File::create(dir.join("COMPLETE"))?;
    f.write_all(digest.as_bytes())?;
    Ok(())
}

fn verify_complete(keys: &PathBuf, backup_id: &str, digest: &str, complete: &str) -> Result<()> {
    let mut lines = complete.lines();
    let signed_digest = lines.next().unwrap_or("");
    let sig_hex = lines.next().unwrap_or("");
    if signed_digest != digest {
        anyhow::bail!("backup digest mismatch");
    }
    let bytes = std::fs::read(keys.join("backup-ed25519.key"))?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| anyhow!("backup signing key"))?;
    let signing = SigningKey::from_bytes(&arr);
    let sig_bytes = hex::decode(sig_hex).map_err(|_| anyhow!("backup signature"))?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| anyhow!("backup signature"))?;
    signing
        .verifying_key()
        .verify(format!("{backup_id}:{digest}").as_bytes(), &signature)
        .map_err(|_| anyhow!("backup signature mismatch"))?;
    Ok(())
}

fn sign_complete(keys: &PathBuf, backup_id: &str, digest: &str) -> Result<String> {
    std::fs::create_dir_all(keys)?;
    let key_path = keys.join("backup-ed25519.key");
    let signing = if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        let arr: [u8; 32] = bytes.as_slice().try_into().unwrap_or([0u8; 32]);
        SigningKey::from_bytes(&arr)
    } else {
        let mut rng = rand::rngs::OsRng;
        let key = SigningKey::generate(&mut rng);
        std::fs::write(&key_path, key.to_bytes())?;
        key
    };
    let msg = format!("{backup_id}:{digest}");
    let sig = signing.sign(msg.as_bytes());
    Ok(format!("{digest}\n{}", hex::encode(sig.to_bytes())))
}

fn age_identity(keys: &PathBuf) -> Result<age::x25519::Identity> {
    std::fs::create_dir_all(keys)?;
    let path = keys.join("backup-age.key");
    if path.exists() {
        let s = std::fs::read_to_string(&path)?;
        s.trim()
            .parse::<age::x25519::Identity>()
            .map_err(|e| anyhow!(e.to_string()))
    } else {
        let id = age::x25519::Identity::generate();
        std::fs::write(&path, id.to_string().expose_secret())?;
        Ok(id)
    }
}

pub fn age_encrypt_bytes(keys: &PathBuf, plaintext: &[u8]) -> Result<Vec<u8>> {
    let id = age_identity(keys)?;
    Ok(age::encrypt(&id.to_public(), plaintext)?)
}

pub fn age_decrypt_bytes(keys: &PathBuf, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let id = age_identity(keys)?;
    Ok(age::decrypt(&id, ciphertext)?)
}
