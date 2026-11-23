use anyhow::{anyhow, Result};
use serde_json::json;
use sqlx::SqlitePool;

use crate::app::AppState;
use crate::auth::Actor;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::new_id;
use crate::storage::{open_backend, BackendConfig};

pub async fn plan(
    state: &AppState,
    actor: &Actor,
    target: BackendConfig,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    ensure_migration_slot(&state.pool).await?;
    let source_id = state.media().await.map_err(anyhow::Error::from)?.backend_id;
    if let (BackendConfig::Local { root: target_root }, BackendConfig::Local { root: source_root }) = (
        &target,
        open_config(&state.pool, &source_id).await.map_err(anyhow::Error::from)?,
    ) {
        let target_root = std::fs::canonicalize(target_root).unwrap_or_else(|_| target_root.clone());
        let source_root = std::fs::canonicalize(&source_root).unwrap_or(source_root);
        if target_root == source_root
            || target_root.starts_with(&source_root)
            || source_root.starts_with(&target_root)
        {
            return Err(AppError::conflict(
                "STORAGE_OVERLAP",
                "source and target media roots overlap",
            ));
        }
    }
    let target_id = new_id();
    let probe_backend = open_backend(target_id.clone(), target.clone()).map_err(anyhow::Error::from)?;
    probe_backend
        .backend()
        .probe()
        .await
        .map_err(anyhow::Error::from)?;
    let now = db::now_ms();
    let kind = match &target {
        BackendConfig::Local { .. } => "local",
        BackendConfig::S3 { .. } => "s3",
        BackendConfig::WebDav { .. } => "webdav",
    };
    sqlx::query(
        "INSERT INTO storage_backends(id, kind, config_json, created_at) VALUES(?, ?, ?, ?)",
    )
    .bind(&target_id)
    .bind(kind)
    .bind(serde_json::to_string(&target).map_err(anyhow::Error::from)?)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let blobs = required_blobs(&state.pool).await?;
    let byte_count: i64 = blobs.iter().map(|blob| blob.2).sum();
    let (catalog,): (i64,) =
        sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
            .fetch_one(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    let migration_id = new_id();
    let (source_rev,): (i64,) =
        sqlx::query_as("SELECT config_revision FROM storage_backends WHERE id = ?")
            .bind(&source_id)
            .fetch_one(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    let (target_rev,): (i64,) =
        sqlx::query_as("SELECT config_revision FROM storage_backends WHERE id = ?")
            .bind(&target_id)
            .fetch_one(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO storage_migrations(
            id, source_backend_id, target_backend_id, direction, stage, state,
            snapshot_catalog_revision, source_config_revision, target_config_revision,
            created_at, updated_at
         ) VALUES(?, ?, ?, 'forward', 'planned', 'planned', ?, ?, ?, ?, ?)",
    )
    .bind(&migration_id)
    .bind(&source_id)
    .bind(&target_id)
    .bind(catalog)
    .bind(source_rev)
    .bind(target_rev)
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let _ = returned(&state.pool, actor, &migration_id).await;
    Ok(json!({
        "id": migration_id,
        "stage": "planned",
        "state": "planned",
        "revision": 1,
        "source_backend_id": source_id,
        "target_backend_id": target_id,
        "object_count": blobs.len() as i64,
        "byte_count": byte_count,
        "catalog_revision": catalog,
        "next_action": "start"
    }))
}

pub async fn begin(
    state: &AppState,
    actor: &Actor,
    id: &str,
    expected_revision: i64,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, i64, String, Option<i64>)>(
        "SELECT stage, revision, target_backend_id, snapshot_catalog_revision
         FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "planned" {
        return Err(AppError::conflict("MIGRATION_STAGE", "start begins from a plan"));
    }
    if row.1 != expected_revision {
        return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
    }
    if db::write_barrier(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict("MIGRATION_LOCKED", "maintenance lock held"));
    }
    let (catalog,): (i64,) =
        sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
            .fetch_one(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    if row.3 != Some(catalog) {
        return Err(AppError::conflict("ROUTE_CHANGED", "catalog changed since the plan"));
    }
    ensure_pinned_config(&state.pool, id).await?;
    let blobs = required_blobs(&state.pool).await?;
    let bytes = blobs.iter().map(|blob| blob.2.max(0) as u64).fold(0u64, u64::saturating_add);
    let target_cfg = open_config(&state.pool, &row.2).await.map_err(anyhow::Error::from)?;
    let free = match &target_cfg {
        BackendConfig::Local { root } => crate::download::free_bytes(root),
        BackendConfig::S3 { .. } | BackendConfig::WebDav { .. } => None,
    };
    let acked = crate::db::kv_get(&state.pool, &format!("migration_capacity_ack:{id}"))
        .await
        .map_err(anyhow::Error::from)?
        .as_deref()
        == Some("1");
    match capacity_decision(free, bytes, acked) {
        CapacityDecision::Ok => {}
        CapacityDecision::Short => {
            return Err(AppError::conflict(
                "DEST_CAPACITY",
                "target does not have the copy size plus a 10 percent margin",
            ))
        }
        CapacityDecision::NeedsAck => {
            return Err(AppError::conflict(
                "DEST_CAPACITY",
                "target free space is unknown; record the quota before starting",
            ))
        }
    }
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = ? WHERE id = 1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let mut job_ids = Vec::new();
    for (blob_id, sha, size) in &blobs {
        sqlx::query(
            "INSERT INTO storage_migration_items(migration_id, blob_id, expected_sha256, expected_size, state)
             VALUES(?, ?, ?, ?, 'pending')",
        )
        .bind(id)
        .bind(blob_id)
        .bind(sha)
        .bind(size)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let payload = crate::jobs::JobPayload {
            kind: "storage_migrate_copy".into(),
            payload_version: 1,
            scope: json!({"migration_id": id, "blob_id": blob_id}),
            target: json!({"target_backend_id": row.2}),
            actor_id: Some(actor.id.clone()),
        };
        let job_id = crate::jobs::enqueue(
            &mut tx,
            "storage_migrate_copy",
            &format!("storage_migrate_copy:{id}:{blob_id}"),
            &payload,
            Some(&actor.id),
            20,
        )
        .await
        .map_err(anyhow::Error::from)?;
        job_ids.push(job_id);
    }
    sqlx::query(
        "UPDATE storage_migrations
         SET stage = 'copying', state = 'running', revision = revision + 1, updated_at = ?
         WHERE id = ? AND stage = 'planned' AND revision = ?",
    )
    .bind(db::now_ms())
    .bind(id)
    .bind(expected_revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    if blobs.is_empty() {
        mark_ready(&state.pool, id).await.map_err(anyhow::Error::from)?;
    }
    state.wake_jobs();
    let mut view = get(&state.pool, actor, id).await?;
    view["job_ids"] = json!(job_ids);
    emit_migration(&state.pool, &actor.id, &view).await;
    Ok(view)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityDecision {
    Ok,
    Short,
    NeedsAck,
}

pub fn capacity_decision(free: Option<u64>, bytes: u64, acked: bool) -> CapacityDecision {
    let need = bytes.saturating_add(bytes / 10);
    match free {
        Some(free) if free >= need => CapacityDecision::Ok,
        Some(_) => CapacityDecision::Short,
        None if acked => CapacityDecision::Ok,
        None => CapacityDecision::NeedsAck,
    }
}

pub async fn start(
    state: &AppState,
    actor: &Actor,
    target: BackendConfig,
) -> AppResult<serde_json::Value> {
    let planned = plan(state, actor, target).await?;
    begin(
        state,
        actor,
        planned["id"].as_str().unwrap_or(""),
        planned["revision"].as_i64().unwrap_or(1),
    )
    .await
}

async fn ensure_migration_slot(pool: &SqlitePool) -> AppResult<()> {
    if db::write_barrier(pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::conflict("MIGRATION_LOCKED", "maintenance lock held"));
    }
    let (open,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM storage_migrations
         WHERE stage IN ('planned', 'copying', 'ready', 'paused')",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if open > 0 {
        return Err(AppError::conflict("MIGRATION_LOCKED", "another migration is open"));
    }
    Ok(())
}

async fn required_blobs(pool: &SqlitePool) -> AppResult<Vec<(String, String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT b.id, b.sha256, b.size FROM media_blobs b
         WHERE b.state = 'verified' AND b.kind IN ('original', 'variant')
           AND (
             EXISTS (
               SELECT 1 FROM images i
               WHERE i.original_blob_id = b.id AND i.state IN ('active', 'trashed')
             )
             OR EXISTS (
               SELECT 1 FROM image_variants v
               WHERE v.blob_id = b.id AND v.state = 'published'
             )
           )",
    )
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?)
}

async fn open_config(pool: &SqlitePool, backend_id: &str) -> Result<BackendConfig> {
    let (json,): (String,) = sqlx::query_as("SELECT config_json FROM storage_backends WHERE id = ?")
        .bind(backend_id)
        .fetch_one(pool)
        .await?;
    Ok(serde_json::from_str(&json)?)
}

pub async fn copy_item(state: &AppState, payload_json: &str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let migration_id = payload["scope"]["migration_id"]
        .as_str()
        .ok_or_else(|| anyhow!("migration_id"))?;
    let blob_id = payload["scope"]["blob_id"]
        .as_str()
        .ok_or_else(|| anyhow!("blob_id"))?;
    let target_backend_id = payload["target"]["target_backend_id"]
        .as_str()
        .ok_or_else(|| anyhow!("target"))?;
    let (source_id,): (String,) =
        sqlx::query_as("SELECT source_backend_id FROM storage_migrations WHERE id = ?")
            .bind(migration_id)
            .fetch_one(&state.pool)
            .await?;
    ensure_pinned_config(&state.pool, migration_id)
        .await
        .map_err(|err| anyhow!(err.to_string()))?;
    let source = crate::storage::open_by_id(&state.pool, &source_id).await?;
    let target = crate::storage::open_by_id(&state.pool, target_backend_id).await?;
    let (key, size, sha, mime) =
        crate::storage::location_for(&state.pool, blob_id, &source_id).await?;
    if target_matches(&target, &key, size, &sha).await? {
        clear_attempt_partials(&target, migration_id, blob_id);
        record_verified(&state.pool, blob_id, target_backend_id, &key, size, &sha).await?;
        mark_item_verified(&state.pool, migration_id, blob_id).await?;
        mark_ready(&state.pool, migration_id).await?;
        return Ok(());
    }
    if let (Some(source_path), Some(stable)) = (
        source.backend().local_file(&key),
        target.backend().local_file(&key),
    ) {
        let attempt = next_attempt(&state.pool, migration_id, blob_id).await?;
        clear_attempt_partials(&target, migration_id, blob_id);
        let partial_key = format!("work/migrate/{migration_id}/{blob_id}-{attempt}.partial");
        let partial = target
            .backend()
            .local_file(&partial_key)
            .ok_or_else(|| anyhow!("partial key"))?;
        let streamed = crate::storage::local::LocalFs::stream_copy(&source_path, &partial);
        let (copied, digest) = match streamed {
            Ok(row) => row,
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                return Err(err);
            }
        };
        if digest != sha || copied as i64 != size {
            let _ = std::fs::remove_file(&partial);
            anyhow::bail!("source object mismatch");
        }
        if let Err(err) = crate::storage::local::LocalFs::publish_local(&partial, &stable) {
            let _ = std::fs::remove_file(&partial);
            return Err(err);
        }
    } else {
        stream_object_copy(&source, &target, &key, size, &sha, &mime).await?;
    }
    record_verified(&state.pool, blob_id, target_backend_id, &key, size, &sha).await?;
    mark_item_verified(&state.pool, migration_id, blob_id).await?;
    mark_ready(&state.pool, migration_id).await?;
    Ok(())
}

async fn target_matches(target: &crate::storage::Storage, key: &str, size: i64, sha: &str) -> Result<bool> {
    if let Some(path) = target.backend().local_file(key) {
        if !path.is_file() {
            return Ok(false);
        }
        let (len, digest) = crate::storage::local::hash_file(&path)?;
        return Ok(len as i64 == size && digest == sha);
    }
    match ranged_sha(target, key, size).await {
        Ok(Some(digest)) => Ok(digest == sha),
        Ok(None) => Ok(false),
        Err(err) => Err(err),
    }
}

const COPY_CHUNK: u64 = 256 * 1024;

pub async fn stream_object_copy(
    source: &crate::storage::Storage,
    target: &crate::storage::Storage,
    key: &str,
    size: i64,
    sha: &str,
    mime: &str,
) -> Result<()> {
    let temp = std::env::temp_dir().join(format!(
        "gallery-copy-{}-{}",
        new_id(),
        crate::ids::sha256_hex(key.as_bytes())
    ));
    let streamed = if let Some(path) = source.backend().local_file(key) {
        crate::storage::local::LocalFs::stream_copy(&path, &temp)
    } else {
        write_ranged(source, key, size, &temp).await
    };
    let (copied, digest) = match streamed {
        Ok(row) => row,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
    };
    if digest != sha || copied as i64 != size {
        let _ = std::fs::remove_file(&temp);
        anyhow::bail!("source object mismatch");
    }
    let published = target.backend().put_file(key, &temp, mime).await;
    let _ = std::fs::remove_file(&temp);
    published?;
    Ok(())
}

async fn write_ranged(
    source: &crate::storage::Storage,
    key: &str,
    size: i64,
    dest: &std::path::Path,
) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut output = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let total = size.max(0) as u64;
    let mut offset = 0u64;
    while offset < total {
        let end = (offset + COPY_CHUNK - 1).min(total - 1);
        let chunk = source.backend().open(key, Some((offset, end))).await.map_err(|err| {
            if crate::storage::is_missing(&err) {
                anyhow!("storage missing")
            } else {
                anyhow!("storage unavailable")
            }
        })?;
        if chunk.len() as u64 != end - offset + 1 {
            anyhow::bail!("source object mismatch");
        }
        hasher.update(&chunk);
        output.write_all(&chunk)?;
        offset = end + 1;
    }
    output.sync_all()?;
    Ok((total, hex::encode(hasher.finalize())))
}

async fn ranged_sha(storage: &crate::storage::Storage, key: &str, size: i64) -> Result<Option<String>> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let total = size.max(0) as u64;
    let mut offset = 0u64;
    while offset < total {
        let end = (offset + COPY_CHUNK - 1).min(total - 1);
        let chunk = match storage.backend().open(key, Some((offset, end))).await {
            Ok(chunk) => chunk,
            Err(err) if crate::storage::is_missing(&err) => return Ok(None),
            Err(_) => anyhow::bail!("storage unavailable"),
        };
        if chunk.len() as u64 != end - offset + 1 {
            return Ok(None);
        }
        hasher.update(&chunk);
        offset = end + 1;
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

fn clear_attempt_partials(target: &crate::storage::Storage, migration_id: &str, blob_id: &str) {
    let Some(marker) = target.backend().local_file(&format!("work/migrate/{migration_id}/keep")) else {
        return;
    };
    let Some(dir) = marker.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{blob_id}-");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && name.ends_with(".partial") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

async fn next_attempt(pool: &SqlitePool, migration_id: &str, blob_id: &str) -> Result<i64> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE storage_migration_items
         SET attempt_no = attempt_no + 1, state = 'copying'
         WHERE migration_id = ? AND blob_id = ?
         RETURNING attempt_no",
    )
    .bind(migration_id)
    .bind(blob_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|item| item.0).unwrap_or(1))
}

async fn record_verified(
    pool: &SqlitePool,
    blob_id: &str,
    backend_id: &str,
    key: &str,
    size: i64,
    sha: &str,
) -> Result<()> {
    let now = db::now_ms();
    sqlx::query(
        "INSERT INTO media_locations(blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at)
         VALUES(?, ?, ?, 1, ?, ?, 'verified', ?)
         ON CONFLICT(blob_id, backend_id) DO UPDATE SET
            object_key = excluded.object_key, size = excluded.size, sha256 = excluded.sha256,
            state = 'verified', verified_at = excluded.verified_at",
    )
    .bind(blob_id)
    .bind(backend_id)
    .bind(key)
    .bind(size)
    .bind(sha)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_item_verified(pool: &SqlitePool, migration_id: &str, blob_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE storage_migration_items SET state = 'verified', verified_at = ?, last_error = NULL
         WHERE migration_id = ? AND blob_id = ?",
    )
    .bind(db::now_ms())
    .bind(migration_id)
    .bind(blob_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_pinned_config(pool: &SqlitePool, migration_id: &str) -> AppResult<()> {
    let row = sqlx::query_as::<_, (Option<i64>, Option<i64>, i64, i64)>(
        "SELECT m.source_config_revision, m.target_config_revision, s.config_revision, t.config_revision
         FROM storage_migrations m
         JOIN storage_backends s ON s.id = m.source_backend_id
         JOIN storage_backends t ON t.id = m.target_backend_id
         WHERE m.id = ?",
    )
    .bind(migration_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0.is_some_and(|pinned| pinned != row.2) || row.1.is_some_and(|pinned| pinned != row.3) {
        return Err(AppError::conflict(
            "CONFIG_CHANGED",
            "storage endpoint revision changed; precheck again",
        ));
    }
    Ok(())
}

async fn recheck_verified(state: &AppState, migration_id: &str) -> AppResult<()> {
    let (target_id,): (String,) = sqlx::query_as(
        "SELECT target_backend_id FROM storage_migrations WHERE id = ?",
    )
    .bind(migration_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let target = crate::storage::open_by_id(&state.pool, &target_id)
        .await
        .map_err(anyhow::Error::from)?;
    let items = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT blob_id, expected_sha256, expected_size FROM storage_migration_items
         WHERE migration_id = ? AND state = 'verified'",
    )
    .bind(migration_id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let now = db::now_ms();
    for (blob_id, sha, size) in items {
        let located = crate::storage::location_for(&state.pool, &blob_id, &target_id).await;
        let still_good = match located {
            Ok((key, _, _, _)) => target_matches(&target, &key, size, &sha)
                .await
                .map_err(anyhow::Error::from)?,
            Err(_) => false,
        };
        if still_good {
            continue;
        }
        sqlx::query("DELETE FROM media_locations WHERE blob_id = ? AND backend_id = ?")
            .bind(&blob_id)
            .bind(&target_id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
        sqlx::query(
            "UPDATE storage_migration_items SET state = 'pending', verified_at = NULL, last_error = 'target drifted'
             WHERE migration_id = ? AND blob_id = ?",
        )
        .bind(migration_id)
        .bind(&blob_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        sqlx::query(
            "UPDATE jobs SET state = 'queued', available_at = ?, failure_code = NULL,
                lease_token = NULL, lease_until = NULL, updated_at = ?
             WHERE dedupe_key = ? AND state IN ('succeeded', 'failed', 'retry_wait', 'canceled')",
        )
        .bind(now)
        .bind(now)
        .bind(format!("storage_migrate_copy:{migration_id}:{blob_id}"))
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

async fn mark_ready(pool: &SqlitePool, migration_id: &str) -> Result<()> {
    let pending: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM storage_migration_items WHERE migration_id = ? AND state != 'verified'",
    )
    .bind(migration_id)
    .fetch_one(pool)
    .await?;
    if pending.0 != 0 {
        return Ok(());
    }
    let seal = item_seal(pool, migration_id).await?;
    sqlx::query(
        "UPDATE storage_migrations
         SET stage = 'ready', state = 'ready', seal_json = ?, revision = revision + 1, updated_at = ?
         WHERE id = ? AND stage = 'copying'",
    )
    .bind(&seal)
    .bind(db::now_ms())
    .bind(migration_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn item_seal(pool: &SqlitePool, migration_id: &str) -> Result<String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT blob_id, expected_sha256 FROM storage_migration_items
         WHERE migration_id = ? ORDER BY blob_id",
    )
    .bind(migration_id)
    .fetch_all(pool)
    .await?;
    let mut raw = String::new();
    for (blob_id, sha) in rows {
        raw.push_str(&blob_id);
        raw.push('\n');
        raw.push_str(&sha);
        raw.push('\n');
    }
    Ok(crate::ids::sha256_hex(raw.as_bytes()))
}

const PROTECT_MS: i64 = 14 * 24 * 3600 * 1000;
const ROLLBACK_MAX_ATTEMPTS: i64 = 3;

pub async fn cutover(state: &AppState, actor: &Actor, id: &str, expected: i64) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, i64, Option<String>, Option<i64>)>(
        "SELECT stage, target_backend_id, revision, seal_json, snapshot_catalog_revision
         FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "ready" && row.0 != "opening" {
        return Err(AppError::conflict("MIGRATION_STAGE", "cutover starts from ready"));
    }
    if row.2 != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
    }
    let seal = item_seal(&state.pool, id).await.map_err(anyhow::Error::from)?;
    if row.3.as_deref() != Some(seal.as_str()) {
        return Err(AppError::conflict("CHECKSUM_MISMATCH", "migration seal changed"));
    }
    let (catalog,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if row.4 != Some(catalog) {
        return Err(AppError::conflict("ROUTE_CHANGED", "catalog changed during copy"));
    }
    let pending: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM storage_migration_items WHERE migration_id = ? AND state != 'verified'",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if pending.0 != 0 {
        return Err(AppError::conflict("MIGRATION_STAGE", "objects are still unverified"));
    }
    let sample: Option<(String, String)> = sqlx::query_as(
        "SELECT blob_id, expected_sha256 FROM storage_migration_items WHERE migration_id = ? LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some((blob_id, sha)) = sample {
        let target = crate::storage::open_by_id(&state.pool, &row.1)
            .await
            .map_err(anyhow::Error::from)?;
        let (key, size, _, _) = crate::storage::location_for(&state.pool, &blob_id, &row.1)
            .await
            .map_err(anyhow::Error::from)?;
        if !target.hash_object(&key, size).await.is_ok_and(|(digest, _)| digest == sha) {
            return Err(AppError::conflict("CHECKSUM_MISMATCH", "target object hash mismatch"));
        }
    }
    let opened_revision = if row.0 == "ready" {
        let now = db::now_ms();
        let mut tx = crate::db::begin_immediate(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
        let switched = sqlx::query(
            "UPDATE storage_route SET active_backend_id = ?, routing_epoch = routing_epoch + 1,
                write_barrier = 1, current_migration_id = ? WHERE id = 1",
        )
        .bind(&row.1)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let marked = sqlx::query(
            "UPDATE storage_migrations
             SET stage = 'opening', state = 'opening', revision = revision + 1, updated_at = ?
             WHERE id = ? AND stage = 'ready' AND revision = ?",
        )
        .bind(now)
        .bind(id)
        .bind(expected)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        if switched.rows_affected() != 1 || marked.rows_affected() != 1 {
            return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
        }
        tx.commit().await.map_err(anyhow::Error::from)?;
        expected + 1
    } else if row.0 == "opening" {
        if row.2 != expected {
            return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
        }
        let (active, barrier): (String, i64) = sqlx::query_as(
            "SELECT active_backend_id, write_barrier FROM storage_route WHERE id = 1",
        )
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        if active != row.1 || barrier != 1 {
            return Err(AppError::conflict("ROUTE_CHANGED", "opening route is not the frozen target"));
        }
        expected
    } else {
        return Err(AppError::conflict("MIGRATION_STAGE", "cutover starts from ready"));
    };
    if let Err(err) = smoke_target(&state.pool, id, &row.1).await {
        return Err(err);
    }
    let now = db::now_ms();
    let mut tx = crate::db::begin_immediate(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let opened = sqlx::query(
        "UPDATE storage_route SET write_barrier = 0, current_migration_id = NULL WHERE id = 1 AND write_barrier = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let marked = sqlx::query(
        "UPDATE storage_migrations
         SET stage = 'protected', state = 'protected', protected_until = ?, cutover_at = ?, revision = revision + 1, updated_at = ?
         WHERE id = ? AND stage = 'opening' AND revision = ?",
    )
    .bind(now + PROTECT_MS)
    .bind(now)
    .bind(now)
    .bind(id)
    .bind(opened_revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if opened.rows_affected() != 1 || marked.rows_affected() != 1 {
        return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    returned(&state.pool, actor, id).await
}

pub async fn recover_opening(pool: &SqlitePool) -> Result<()> {
    let opening: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, target_backend_id FROM storage_migrations WHERE stage = 'opening'",
    )
    .fetch_all(pool)
    .await?;
    for (id, target_id) in opening {
        sqlx::query(
            "UPDATE storage_route SET write_barrier = 1, current_migration_id = ?
             WHERE id = 1 AND active_backend_id = ?",
        )
        .bind(&id)
        .bind(&target_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn smoke_target(pool: &SqlitePool, migration_id: &str, target_id: &str) -> AppResult<()> {
    let (catalog_before,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let sample: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT blob_id, expected_sha256, expected_size FROM storage_migration_items
         WHERE migration_id = ? ORDER BY blob_id LIMIT 1",
    )
    .bind(migration_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let target = crate::storage::open_by_id(pool, target_id)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some((blob_id, sha, size)) = sample {
        let (key, _, located_sha, _) = crate::storage::location_for(pool, &blob_id, target_id)
            .await
            .map_err(|_| AppError::conflict("CHECKSUM_MISMATCH", "target object is not published"))?;
        if located_sha != sha {
            return Err(AppError::conflict("CHECKSUM_MISMATCH", "target object hash mismatch"));
        }
        let (digest, first) = target.hash_object(&key, size).await.map_err(|_| {
            AppError::conflict("CHECKSUM_MISMATCH", "target object hash mismatch")
        })?;
        if digest != sha {
            return Err(AppError::conflict("CHECKSUM_MISMATCH", "target object hash mismatch"));
        }
        let caps = target.backend().probe().await.map_err(|_| {
            AppError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "STORAGE_UNAVAILABLE",
                "Unavailable",
                "target smoke probe failed",
            )
        })?;
        if let Some(first) = first.filter(|_| caps.range) {
            let ranged = target.backend().open(&key, Some((0, 0))).await.map_err(|_| {
                AppError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "Unavailable",
                    "target range smoke failed",
                )
            })?;
            if ranged.as_slice() != [first] {
                return Err(AppError::conflict("CHECKSUM_MISMATCH", "target range smoke mismatch"));
            }
        }
    }
    let smoke_key = format!("work/migrate-smoke/{migration_id}.bin");
    target
        .put_bytes(&smoke_key, b"gallery-smoke", "application/octet-stream")
        .await
        .map_err(|_| {
            AppError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "STORAGE_UNAVAILABLE",
                "Unavailable",
                "target smoke write failed",
            )
        })?;
    let read_back = target.get_bytes(&smoke_key).await.map_err(|_| {
        AppError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "Unavailable",
            "target smoke read failed",
        )
    })?;
    if read_back != b"gallery-smoke" {
        let _ = target.backend().delete(&smoke_key).await;
        return Err(AppError::conflict("CHECKSUM_MISMATCH", "target smoke object mismatch"));
    }
    if target.backend().delete(&smoke_key).await.is_err() {
        return Err(AppError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "Unavailable",
            "target smoke cleanup failed",
        ));
    }
    let (catalog_after,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    if catalog_after != catalog_before {
        return Err(AppError::conflict("ROUTE_CHANGED", "smoke changed the media catalog"));
    }
    Ok(())
}

pub async fn pause(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let (stage,): (String,) = migration_stage(&state.pool, id).await?;
    if stage != "copying" {
        return Err(AppError::conflict("MIGRATION_STAGE", "pause applies while copying"));
    }
    let now = db::now_ms();
    sqlx::query("UPDATE storage_migrations SET state = 'paused', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE jobs SET state = 'retry_wait', available_at = ? WHERE dedupe_key LIKE ? AND state IN ('queued', 'retry_wait')",
    )
    .bind(now + 365 * 24 * 3600 * 1000)
    .bind(format!("storage_migrate_copy:{id}:%"))
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    returned(&state.pool, actor, id).await
}

pub async fn resume(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    ensure_pinned_config(&state.pool, id).await?;
    recheck_verified(state, id).await?;
    let now = db::now_ms();
    sqlx::query(
        "UPDATE jobs SET state = 'queued', available_at = ? WHERE dedupe_key LIKE ? AND state = 'retry_wait'",
    )
    .bind(now)
    .bind(format!("storage_migrate_copy:{id}:%"))
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE storage_migrations SET state = 'running', stage = 'copying', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    state.wake_jobs();
    returned(&state.pool, actor, id).await
}

pub async fn abort(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let (stage,): (String,) = migration_stage(&state.pool, id).await?;
    if matches!(
        stage.as_str(),
        "done" | "opening" | "protected" | "retiring" | "retired" | "rolled_back"
    ) {
        return Err(AppError::conflict("MIGRATION_STAGE", "use rollback after cutover"));
    }
    let (source_id, target_id): (String, String) = sqlx::query_as(
        "SELECT source_backend_id, target_backend_id FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let source = crate::storage::open_by_id(&state.pool, &source_id)
        .await
        .map_err(anyhow::Error::from)?;
    if source.backend().probe().await.is_err() {
        return Err(AppError::conflict("SOURCE_UNREADABLE", "source is not readable"));
    }
    let target = crate::storage::open_by_id(&state.pool, &target_id)
        .await
        .map_err(anyhow::Error::from)?;
    let copied: Vec<(String, String)> = sqlx::query_as(
        "SELECT l.blob_id, l.object_key FROM media_locations l
         JOIN storage_migration_items i ON i.blob_id = l.blob_id AND i.migration_id = ?
         WHERE l.backend_id = ?",
    )
    .bind(id)
    .bind(&target_id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut cleaned = Vec::new();
    let mut cleanup_errors = Vec::new();
    for (blob_id, key) in copied {
        if target.backend().delete(&key).await.is_err() {
            cleanup_errors.push(json!({"key": key, "error": "delete failed"}));
            continue;
        }
        sqlx::query("DELETE FROM media_locations WHERE blob_id = ? AND backend_id = ?")
            .bind(&blob_id)
            .bind(&target_id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
        cleaned.push(key);
    }
    let (partials, partial_errors) = clear_migration_partials(&target, id);
    cleaned.extend(partials);
    cleanup_errors.extend(partial_errors);
    let now = db::now_ms();
    sqlx::query("UPDATE jobs SET state = 'canceled', updated_at = ? WHERE dedupe_key LIKE ? AND state IN ('queued', 'retry_wait')")
        .bind(now)
        .bind(format!("storage_migrate_copy:{id}:%"))
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0, current_migration_id = NULL WHERE id = 1 AND current_migration_id = ?
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE storage_migrations SET state = 'aborted', stage = 'aborted', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let _ = crate::events::audit(
        &state.pool,
        "migration",
        Some(&actor.id),
        "migration.abort",
        Some(id),
        "aborted",
        Some(&format!(
            "cleaned={} errors={}",
            cleaned.len(),
            cleanup_errors.len()
        )),
    )
    .await;
    let mut view = get(&state.pool, actor, id).await?;
    view["cleaned"] = json!(cleaned);
    view["cleanup_errors"] = json!(cleanup_errors);
    emit_migration(&state.pool, &actor.id, &view).await;
    Ok(view)
}

fn clear_migration_partials(
    target: &crate::storage::Storage,
    migration_id: &str,
) -> (Vec<String>, Vec<serde_json::Value>) {
    let marker_key = format!("work/migrate/{migration_id}/keep");
    let Some(marker) = target.backend().local_file(&marker_key) else {
        return (Vec::new(), Vec::new());
    };
    let Some(dir) = marker.parent() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut cleaned = Vec::new();
    let mut errors = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".partial") {
            continue;
        }
        let key = format!("work/migrate/{migration_id}/{name}");
        if std::fs::remove_file(entry.path()).is_err() {
            errors.push(json!({"key": key, "error": "delete failed"}));
        } else {
            cleaned.push(key);
        }
    }
    (cleaned, errors)
}

pub async fn rollback(state: &AppState, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT source_backend_id, target_backend_id, stage, direction FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.3 == "rollback" {
        return Err(AppError::conflict(
            "MIGRATION_STAGE",
            "the abandoned backend is already protected",
        ));
    }
    if row.2 == "retired" || row.2 == "retiring" {
        return Err(AppError::conflict("RETIRE_BLOCKED", "retired storage cannot be a rollback target"));
    }
    if row.2 != "protected" && row.2 != "done" && row.2 != "opening" {
        return Err(AppError::conflict("MIGRATION_STAGE", "rollback starts after cutover"));
    }
    prepare_rollback(state, id, &row.0).await?;
    if source_inventory_matches(&state.pool, id, &row.0).await? {
        return finish_rollback(state, actor, id, &row.0).await;
    }
    sync_rollback_items(&state.pool, id).await?;
    let blobs: Vec<(String,)> = sqlx::query_as(
        "SELECT blob_id FROM storage_rollback_items
         WHERE migration_id = ? AND state IN ('pending', 'copying')
         ORDER BY blob_id",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let source = crate::storage::open_by_id(&state.pool, &row.0).await.map_err(anyhow::Error::from)?;
    let target = crate::storage::open_by_id(&state.pool, &row.1).await.map_err(anyhow::Error::from)?;
    for (blob_id,) in blobs {
        if !claim_rollback_item(&state.pool, id, &blob_id).await? {
            continue;
        }
        let current = crate::storage::location_for(&state.pool, &blob_id, &row.1)
            .await
            .map_err(anyhow::Error::from)?;
        let on_target = match ranged_sha(&target, &current.0, current.1).await {
            Ok(Some(digest)) if digest == current.2 => true,
            Ok(Some(_)) => {
                release_rollback_item(&state.pool, id, &blob_id).await?;
                return Err(AppError::conflict(
                    "ROLLBACK_MISSING_MEDIA",
                    "current object hash mismatch",
                ));
            }
            Ok(None) | Err(_) => false,
        };
        let source_ok = match crate::storage::location_for(&state.pool, &blob_id, &row.0).await {
            Ok(loc) => match ranged_sha(&source, &loc.0, current.1).await {
                Ok(Some(digest)) => digest == current.2,
                _ => false,
            },
            Err(_) => false,
        };
        if source_ok {
            mark_rollback_verified(&state.pool, id, &blob_id).await?;
            continue;
        }
        if on_target {
            stream_object_copy(&target, &source, &current.0, current.1, &current.2, &current.3)
                .await
                .map_err(anyhow::Error::from)?;
            let now = db::now_ms();
            sqlx::query(
                "INSERT INTO media_locations(blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at)
                 VALUES(?, ?, ?, 1, ?, ?, 'verified', ?)
                 ON CONFLICT(blob_id, backend_id) DO UPDATE SET state = 'verified', sha256 = excluded.sha256, size = excluded.size, verified_at = excluded.verified_at",
            )
            .bind(&blob_id)
            .bind(&row.0)
            .bind(&current.0)
            .bind(current.1)
            .bind(&current.2)
            .bind(now)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
            mark_rollback_verified(&state.pool, id, &blob_id).await?;
            continue;
        }
        let restored = plaintext_from_backup(state, &blob_id, &current.2).await?;
        let Some(file) = restored else {
            let (kind,): (String,) = sqlx::query_as("SELECT kind FROM media_blobs WHERE id = ?")
                .bind(&blob_id)
                .fetch_one(&state.pool)
                .await
                .map_err(anyhow::Error::from)?;
            if kind == "original" {
                release_rollback_item(&state.pool, id, &blob_id).await?;
                return Err(AppError::conflict(
                    "ROLLBACK_MISSING_MEDIA",
                    "original exists only on the unreadable backend",
                ));
            }
            mark_rollback_verified(&state.pool, id, &blob_id).await?;
            continue;
        };
        source
            .backend().put_file(&current.0, file.path(), &current.3)
            .await
            .map_err(anyhow::Error::from)?;
        let now = db::now_ms();
        sqlx::query(
            "INSERT INTO media_locations(blob_id, backend_id, object_key, backend_revision, size, sha256, state, verified_at)
             VALUES(?, ?, ?, 1, ?, ?, 'verified', ?)
             ON CONFLICT(blob_id, backend_id) DO UPDATE SET state = 'verified', sha256 = excluded.sha256, size = excluded.size, verified_at = excluded.verified_at",
        )
        .bind(&blob_id)
        .bind(&row.0)
        .bind(&current.0)
        .bind(current.1)
        .bind(&current.2)
        .bind(now)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        mark_rollback_verified(&state.pool, id, &blob_id).await?;
    }
    let now = db::now_ms();
    sqlx::query(
        "UPDATE storage_rollback_items
         SET state = 'failed', lease_token = NULL, lease_until = NULL
         WHERE migration_id = ? AND state != 'verified' AND attempt_no >= ?
           AND (lease_until IS NULL OR lease_until < ?)",
    )
    .bind(id)
    .bind(ROLLBACK_MAX_ATTEMPTS)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let (failed, open): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'failed'), 0), COALESCE(SUM(state != 'verified'), 0)
         FROM storage_rollback_items WHERE migration_id = ?",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if failed > 0 {
        return Err(AppError::conflict(
            "ATTEMPTS_EXHAUSTED",
            "rollback item used its attempt limit",
        ));
    }
    let (exceptions,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'exception'), 0) FROM storage_rollback_items WHERE migration_id = ?",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if exceptions > 0 {
        return Err(AppError::conflict(
            "DEGRADE_REQUIRED",
            "missing originals need a confirmed degrade",
        ));
    }
    if open > 0 {
        return Err(AppError::conflict("LEASE_HELD", "a rollback item is still leased"));
    }
    finish_rollback(state, actor, id, &row.0).await
}

async fn sync_rollback_items(pool: &SqlitePool, migration_id: &str) -> AppResult<()> {
    let required = required_blobs(pool).await?;
    for (blob_id, sha, size) in required {
        sqlx::query(
            "INSERT INTO storage_rollback_items(
                migration_id, blob_id, expected_sha256, expected_size, state
             ) VALUES(?, ?, ?, ?, 'pending')
             ON CONFLICT(migration_id, blob_id) DO NOTHING",
        )
        .bind(migration_id)
        .bind(&blob_id)
        .bind(&sha)
        .bind(size)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

async fn claim_rollback_item(pool: &SqlitePool, migration_id: &str, blob_id: &str) -> AppResult<bool> {
    let now = db::now_ms();
    let claimed = sqlx::query(
        "UPDATE storage_rollback_items
         SET state = 'copying', attempt_no = attempt_no + 1, lease_token = ?, lease_until = ?
         WHERE migration_id = ? AND blob_id = ? AND state IN ('pending', 'copying')
           AND attempt_no < ?
           AND (lease_until IS NULL OR lease_until < ?)",
    )
    .bind(new_id())
    .bind(now + 60_000)
    .bind(migration_id)
    .bind(blob_id)
    .bind(ROLLBACK_MAX_ATTEMPTS)
    .bind(now)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(claimed.rows_affected() == 1)
}

async fn mark_rollback_verified(pool: &SqlitePool, migration_id: &str, blob_id: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE storage_rollback_items
         SET state = 'verified', lease_token = NULL, lease_until = NULL
         WHERE migration_id = ? AND blob_id = ?",
    )
    .bind(migration_id)
    .bind(blob_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

async fn release_rollback_item(pool: &SqlitePool, migration_id: &str, blob_id: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE storage_rollback_items
         SET state = 'pending', lease_token = NULL, lease_until = NULL
         WHERE migration_id = ? AND blob_id = ? AND state != 'verified'",
    )
    .bind(migration_id)
    .bind(blob_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

pub async fn degrade(
    state: &AppState,
    actor: &Actor,
    id: &str,
    confirmed: &[String],
    backup_id: &str,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT source_backend_id, target_backend_id, stage FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.2 != "protected" && row.2 != "opening" {
        return Err(AppError::conflict("MIGRATION_STAGE", "degrade starts after cutover"));
    }
    let backup: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM backup_runs WHERE id = ? AND state = 'succeeded'",
    )
    .bind(backup_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if backup.is_none() {
        return Err(AppError::conflict("RESTORE_POINT", "restore point is not a succeeded backup"));
    }
    prepare_rollback(state, id, &row.0).await?;
    sync_rollback_items(&state.pool, id).await?;
    let missing = missing_originals(state, id, &row.0, &row.1).await?;
    let mut confirmed_ids: Vec<String> = confirmed.to_vec();
    confirmed_ids.sort();
    confirmed_ids.dedup();
    if confirmed_ids != missing {
        return Err(AppError::conflict(
            "MISSING_SET",
            "confirmed objects do not match the missing originals",
        ));
    }
    for blob_id in &missing {
        sqlx::query(
            "UPDATE storage_rollback_items
             SET state = 'exception', lease_token = NULL, lease_until = NULL
             WHERE migration_id = ? AND blob_id = ?",
        )
        .bind(id)
        .bind(blob_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    let _ = crate::events::audit(
        &state.pool,
        "migration",
        Some(&actor.id),
        "migration.degrade",
        Some(id),
        "degraded",
        Some(&format!("backup={backup_id} missing={}", missing.join(","))),
    )
    .await;
    let (blocked,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM storage_rollback_items
         WHERE migration_id = ? AND state NOT IN ('verified', 'exception')",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if blocked > 0 {
        return Err(AppError::conflict("LEASE_HELD", "a rollback item is still leased"));
    }
    let mut view = finish_rollback(state, actor, id, &row.0).await?;
    view["exceptions"] = json!(missing);
    view["restore_point"] = json!(backup_id);
    Ok(view)
}

async fn missing_originals(
    state: &AppState,
    migration_id: &str,
    source_id: &str,
    target_id: &str,
) -> AppResult<Vec<String>> {
    let _ = migration_id;
    let required = required_blobs(&state.pool).await?;
    let source = crate::storage::open_by_id(&state.pool, source_id)
        .await
        .map_err(anyhow::Error::from)?;
    let target = crate::storage::open_by_id(&state.pool, target_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut missing = Vec::new();
    for (blob_id, sha, size) in required {
        let (kind,): (String,) = sqlx::query_as("SELECT kind FROM media_blobs WHERE id = ?")
            .bind(&blob_id)
            .fetch_one(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
        if kind != "original" {
            continue;
        }
        let on_source = match crate::storage::location_for(&state.pool, &blob_id, source_id).await {
            Ok((key, _, _, _)) => target_matches(&source, &key, size, &sha)
                .await
                .map_err(anyhow::Error::from)?,
            Err(_) => false,
        };
        if on_source {
            continue;
        }
        let on_target = match crate::storage::location_for(&state.pool, &blob_id, target_id).await {
            Ok((key, size, located_sha, _)) => match target.hash_object(&key, size).await {
                Ok((digest, _)) => digest == located_sha && located_sha == sha,
                Err(_) => false,
            },
            Err(_) => false,
        };
        if on_target {
            continue;
        }
        if plaintext_from_backup(state, &blob_id, &sha).await?.is_some() {
            continue;
        }
        missing.push(blob_id);
    }
    missing.sort();
    Ok(missing)
}

async fn prepare_rollback(state: &AppState, id: &str, source_id: &str) -> AppResult<()> {
    let cfg = open_config(&state.pool, source_id).await.map_err(anyhow::Error::from)?;
    if let BackendConfig::Local { root } = &cfg {
        if root.as_os_str().is_empty() {
            return Err(AppError::conflict("RETIRE_BLOCKED", "source backend was retired"));
        }
    }
    let source = crate::storage::open_by_id(&state.pool, source_id)
        .await
        .map_err(anyhow::Error::from)?;
    if source.backend().probe().await.is_err() {
        return Err(AppError::conflict("SOURCE_UNREADABLE", "source is not readable"));
    }
    sqlx::query("UPDATE storage_route SET write_barrier = 1, current_migration_id = ? WHERE id = 1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (running,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs WHERE state = 'running'
         AND kind IN ('generate_variant', 'convert_image', 'purge_media', 'build_archive', 'caption_image')",
    )
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if running > 0 {
        return Err(AppError::conflict("IN_FLIGHT", "wait for in-flight media jobs"));
    }
    let (catalog,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let (pinned,): (Option<i64>,) = sqlx::query_as(
        "SELECT rollback_catalog_revision FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some(pinned) = pinned {
        if pinned != catalog {
            return Err(AppError::conflict("ROUTE_CHANGED", "catalog changed during rollback"));
        }
    } else {
        sqlx::query("UPDATE storage_migrations SET rollback_catalog_revision = ? WHERE id = ?")
            .bind(catalog)
            .bind(id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let required = required_blobs(&state.pool).await?;
    let mut missing = 0_u64;
    for (blob_id, sha, size) in required {
        let matches = match crate::storage::location_for(&state.pool, &blob_id, source_id).await {
            Ok((key, _, _, _)) => target_matches(&source, &key, size, &sha)
                .await
                .map_err(anyhow::Error::from)?,
            Err(_) => false,
        };
        if !matches {
            missing = missing.saturating_add(size.max(0) as u64);
        }
    }
    let free = match &cfg {
        BackendConfig::Local { root } => crate::download::free_bytes(root),
        BackendConfig::S3 { .. } | BackendConfig::WebDav { .. } => None,
    };
    let acked = crate::db::kv_get(&state.pool, &format!("rollback_capacity_ack:{id}"))
        .await
        .map_err(anyhow::Error::from)?
        .as_deref()
        == Some("1");
    match capacity_decision(free, missing, acked) {
        CapacityDecision::Ok => Ok(()),
        CapacityDecision::Short => Err(AppError::conflict(
            "DEST_CAPACITY",
            "source does not have room for the rollback plus a 10 percent margin",
        )),
        CapacityDecision::NeedsAck => Err(AppError::conflict(
            "DEST_CAPACITY",
            "source free space is unknown; record the quota before rolling back",
        )),
    }
}

async fn plaintext_from_backup(
    state: &AppState,
    blob_id: &str,
    sha: &str,
) -> AppResult<Option<tempfile::NamedTempFile>> {
    let found: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT i.backup_id, i.ciphertext_sha256, i.size, r.format_version FROM backup_items i
         JOIN backup_runs r ON r.id = i.backup_id
         WHERE i.blob_id = ? AND i.plaintext_sha256 = ? AND i.state = 'verified'
           AND r.state = 'succeeded' AND i.ciphertext_sha256 IS NOT NULL
         ORDER BY r.snapshot_at DESC",
    ).bind(blob_id).bind(sha).fetch_all(&state.pool).await.map_err(anyhow::Error::from)?;
    let keys = state.config.keys_root();
    let object = crate::ids::sha256_hex(blob_id.as_bytes());
    for (backup_id, ciphertext_sha, size, version) in found {
        let dir = state.config.backup_root().join(&backup_id);
        if version >= crate::backup_package::FORMAT {
            let Ok((_, manifest)) = crate::backup_package::read_committed(&keys, &dir, &backup_id) else {
                continue;
            };
            if !crate::backup_package::items(&manifest)?.iter().any(|item| {
                item["object"] == object && item["ciphertext_sha256"] == ciphertext_sha
                    && item["size"] == size && item["sha256"] == sha
            }) {
                continue;
            }
        }
        let output = tempfile::NamedTempFile::new_in(state.config.work_root()).map_err(anyhow::Error::from)?;
        if crate::backup_package::decrypt_file(&keys, &dir.join(&object), output.path(),
            Some(&ciphertext_sha), Some((size, sha))).is_ok()
        {
            return Ok(Some(output));
        }
    }
    Ok(None)
}

async fn source_inventory_matches(pool: &SqlitePool, migration_id: &str, source_id: &str) -> AppResult<bool> {
    let required = required_blobs(pool).await?;
    let items: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT blob_id, expected_sha256, expected_size FROM storage_migration_items
         WHERE migration_id = ? AND state = 'verified'",
    )
    .bind(migration_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if required.len() != items.len() {
        return Ok(false);
    }
    let source = crate::storage::open_by_id(pool, source_id)
        .await
        .map_err(anyhow::Error::from)?;
    for (blob_id, sha, size) in required {
        let Some(item) = items.iter().find(|item| item.0 == blob_id) else {
            return Ok(false);
        };
        if item.1 != sha || item.2 != size {
            return Ok(false);
        }
        let Ok((key, _, _, _)) = crate::storage::location_for(pool, &blob_id, source_id).await else {
            return Ok(false);
        };
        if !target_matches(&source, &key, size, &sha).await.map_err(anyhow::Error::from)? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn finish_rollback(
    state: &AppState,
    actor: &Actor,
    id: &str,
    source_id: &str,
) -> AppResult<serde_json::Value> {
    sqlx::query(
        "UPDATE storage_route SET active_backend_id = ?, routing_epoch = routing_epoch + 1,
            write_barrier = 1, current_migration_id = ? WHERE id = 1",
    )
    .bind(source_id)
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    smoke_target(&state.pool, id, source_id).await?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0, current_migration_id = NULL WHERE id = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_migrations
         SET stage = 'protected', state = 'protected', direction = 'rollback',
             protected_until = ?, cutover_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(db::now_ms() + PROTECT_MS)
    .bind(db::now_ms())
    .bind(db::now_ms())
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    returned(&state.pool, actor, id).await
}

async fn migration_stage(pool: &SqlitePool, id: &str) -> AppResult<(String,)> {
    sqlx::query_as("SELECT stage FROM storage_migrations WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(AppError::not_found)
}

pub async fn items(
    pool: &SqlitePool,
    actor: &Actor,
    id: &str,
    limit: i64,
    cursor: Option<&str>,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    migration_stage(pool, id).await?;
    let limit = limit.clamp(1, 200);
    let rows = sqlx::query_as::<_, (String, String, i64, Option<String>, Option<i64>, i64)>(
        "SELECT blob_id, state, attempt_no, last_error, verified_at, expected_size
         FROM storage_migration_items
         WHERE migration_id = ? AND (? IS NULL OR blob_id > ?)
         ORDER BY blob_id LIMIT ?",
    )
    .bind(id)
    .bind(cursor)
    .bind(cursor)
    .bind(limit + 1)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let more = rows.len() > limit as usize;
    let page: Vec<_> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if more { page.last().map(|row| row.0.clone()) } else { None };
    Ok(json!({
        "items": page.into_iter().map(|(blob_id, state, attempt_no, last_error, verified_at, size)| json!({
            "blob_id": blob_id,
            "state": state,
            "attempt_no": attempt_no,
            "last_error": last_error,
            "verified_at": verified_at,
            "size": size
        })).collect::<Vec<_>>(),
        "next_cursor": next_cursor
    }))
}

pub async fn retire(
    state: &AppState,
    actor: &Actor,
    id: &str,
    dry_run: bool,
    expected_revision: Option<i64>,
    preview_sha: Option<&str>,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = load_retire_row(&state.pool, id).await?;
    let preview = retire_preview(state, id, &row).await?;
    if dry_run || row.stage == "retired" {
        return Ok(preview);
    }
    if row.stage != "protected" && row.stage != "retiring" {
        return Err(AppError::conflict("RETIRE_BLOCKED", "migration is not in the protection window"));
    }
    if row.stage == "protected" {
        let blockers = preview["blockers"].as_array().cloned().unwrap_or_default();
        if !blockers.is_empty() {
            return Err(AppError::conflict(
                "RETIRE_BLOCKED",
                blockers
                    .iter()
                    .filter_map(|item| item.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        let expected = expected_revision.ok_or_else(|| {
            AppError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "REVISION_REQUIRED",
                "Precondition Required",
                "expected_revision required",
            )
        })?;
        if expected != row.revision {
            return Err(AppError::precondition("REVISION_CONFLICT", "migration revision"));
        }
        let presented = preview_sha.ok_or_else(|| {
            AppError::new(
                axum::http::StatusCode::PRECONDITION_REQUIRED,
                "RETIRE_PREVIEW",
                "Precondition Required",
                "preview_sha required",
            )
        })?;
        if presented != preview["preview_sha"].as_str().unwrap_or("") {
            return Err(AppError::conflict("RETIRE_PREVIEW", "preview no longer matches"));
        }
        sqlx::query("UPDATE storage_migrations SET stage = 'retiring', state = 'retiring', updated_at = ? WHERE id = ? AND stage = 'protected'")
            .bind(db::now_ms())
            .bind(id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let blobs: Vec<(String,)> = sqlx::query_as(
        "SELECT blob_id FROM storage_migration_items WHERE migration_id = ? AND state = 'verified' ORDER BY blob_id",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let kept: std::collections::HashSet<String> = backup_exception_ids(&state.pool, id)
        .await?
        .into_iter()
        .collect();
    let source = crate::storage::open_by_id(&state.pool, &row.source_backend_id)
        .await
        .map_err(anyhow::Error::from)?;
    revoke_backend_credentials(&state.pool, &row.source_backend_id).await?;
    let doomed: Vec<String> = blobs
        .into_iter()
        .map(|row| row.0)
        .filter(|blob_id| !kept.contains(blob_id))
        .collect();
    let total = doomed.len() as i64;
    sqlx::query(
        "UPDATE storage_migrations SET retire_total = ?, retire_done = 0, retire_errors = 0, updated_at = ? WHERE id = ?",
    )
    .bind(total)
    .bind(db::now_ms())
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut done = 0_i64;
    let mut errors = 0_i64;
    for batch in doomed.chunks(32) {
        for blob_id in batch {
            let located = crate::storage::location_for(&state.pool, blob_id, &row.source_backend_id).await;
            if let Ok((key, _, _, _)) = located {
                if source.backend().delete(&key).await.is_err() {
                    errors += 1;
                    continue;
                }
                sqlx::query("DELETE FROM media_locations WHERE blob_id = ? AND backend_id = ?")
                    .bind(blob_id)
                    .bind(&row.source_backend_id)
                    .execute(&state.pool)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            done += 1;
        }
        sqlx::query(
            "UPDATE storage_migrations SET retire_done = ?, retire_errors = ?, updated_at = ? WHERE id = ?",
        )
        .bind(done)
        .bind(errors)
        .bind(db::now_ms())
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    if errors > 0 {
        return Err(AppError::conflict(
            "RETIRE_BLOCKED",
            "retire finished with storage errors",
        ));
    }
    sqlx::query(
        "UPDATE storage_migrations
         SET stage = 'retired', state = 'retired', revision = revision + 1, updated_at = ?
         WHERE id = ? AND stage = 'retiring'",
    )
    .bind(db::now_ms())
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    returned(&state.pool, actor, id).await
}

struct RetireRow {
    source_backend_id: String,
    stage: String,
    revision: i64,
    protected_until: Option<i64>,
}

async fn load_retire_row(pool: &SqlitePool, id: &str) -> AppResult<RetireRow> {
    let row = sqlx::query_as::<_, (String, String, String, i64, Option<i64>, String)>(
        "SELECT source_backend_id, target_backend_id, stage, revision, protected_until, direction
         FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let backend = if row.5 == "rollback" { row.1 } else { row.0 };
    Ok(RetireRow {
        source_backend_id: backend,
        stage: row.2,
        revision: row.3,
        protected_until: row.4,
    })
}

async fn pre_migration_backup(pool: &SqlitePool, protected_until: Option<i64>) -> AppResult<Option<String>> {
    let Some(until) = protected_until else {
        return Ok(None);
    };
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND snapshot_at < ?
         ORDER BY snapshot_at DESC, COALESCE(catalog_revision, 0) DESC LIMIT 1",
    )
    .bind(until - PROTECT_MS)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(row.map(|item| item.0))
}

async fn drill_backup(
    pool: &SqlitePool,
    migration_id: &str,
    protected_until: Option<i64>,
) -> AppResult<Option<String>> {
    let Some(until) = protected_until else {
        return Ok(None);
    };
    let marker: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT snapshot_catalog_revision, cutover_at FROM storage_migrations WHERE id = ?",
    )
    .bind(migration_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let cutover = marker.1.unwrap_or(until - PROTECT_MS);
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs
         WHERE state = 'succeeded' AND snapshot_at IS NOT NULL AND snapshot_at >= ?
           AND COALESCE(catalog_revision, 0) > ?
         ORDER BY snapshot_at DESC, COALESCE(catalog_revision, 0) DESC LIMIT 1",
    )
    .bind(cutover)
    .bind(marker.0.unwrap_or(0))
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(row.map(|item| item.0))
}

pub fn repository_inside(source: &std::path::Path, backup: &std::path::Path) -> bool {
    if source.as_os_str().is_empty() || backup.as_os_str().is_empty() {
        return false;
    }
    let Some(source) = existing_canonical(source) else {
        return false;
    };
    let Some(backup) = existing_canonical(backup) else {
        return false;
    };
    backup.starts_with(&source) && backup != source
}

fn existing_canonical(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut pending = Vec::new();
    let mut current = path;
    let base = loop {
        if current.exists() {
            break current.canonicalize().ok()?;
        }
        pending.push(current.file_name()?.to_os_string());
        current = current.parent()?;
    };
    let mut joined = base;
    for name in pending.into_iter().rev() {
        joined.push(name);
    }
    Some(joined)
}

async fn backup_repository_inside_backend(
    pool: &SqlitePool,
    backend_id: &str,
    backup_root: &std::path::Path,
) -> AppResult<bool> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT config_json FROM storage_backends WHERE id = ?",
    )
    .bind(backend_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let cfg: crate::storage::BackendConfig =
        serde_json::from_str(&row.0).map_err(anyhow::Error::from)?;
    Ok(match cfg {
        crate::storage::BackendConfig::Local { root } => repository_inside(&root, backup_root),
        _ => false,
    })
}

async fn retire_preview(
    state: &AppState,
    id: &str,
    row: &RetireRow,
) -> AppResult<serde_json::Value> {
    let pool = &state.pool;
    let backup_root = state.config.backup_root();
    let (object_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM storage_migration_items WHERE migration_id = ? AND state = 'verified'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let backup_exceptions = backup_exception_ids(pool, id).await?;
    let mut blockers = Vec::new();
    if row.stage != "protected" && row.stage != "retiring" && row.stage != "retired" {
        blockers.push("migration is not in the protection window");
    }
    if let Some(until) = row.protected_until {
        if db::now_ms() < until {
            blockers.push("protection period");
        }
        let cutover_at = until - PROTECT_MS;
        let (backups,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM backup_runs WHERE state = 'succeeded' AND snapshot_at >= ?",
        )
        .bind(cutover_at)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if backups < 2 {
            blockers.push("post-cutover backups");
        }
        if backups >= 2 && drill_backup(pool, id, row.protected_until).await?.is_none() {
            blockers.push("post-cutover writes");
        }
        let (running,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM backup_runs WHERE state IN ('queued', 'running')",
        )
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if running > 0 {
            blockers.push("backup in progress");
        }
        let (unavailable,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM backup_runs
             WHERE state = 'unavailable' AND snapshot_at IS NOT NULL AND snapshot_at >= ?",
        )
        .bind(cutover_at)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if unavailable > 0 {
            blockers.push("backup unavailable");
        }
        let (requires_replica, drills): (i64, i64) = sqlx::query_as(
            "SELECT
                EXISTS(SELECT 1 FROM backup_runs WHERE state = 'succeeded' AND format_version >= 2 AND snapshot_at >= ?),
                EXISTS(SELECT 1 FROM backup_drills d
                    JOIN backup_runs b ON b.id = d.backup_id AND b.state = 'succeeded'
                    WHERE d.slot = 'cutover:' || ? AND d.completed_at >= ?)
                OR EXISTS(SELECT 1 FROM restore_runs r JOIN backup_runs b ON b.id = r.backup_id
                    WHERE r.state = 'succeeded' AND r.created_at >= ? AND b.snapshot_at >= ?)",
        )
        .bind(cutover_at).bind(id).bind(cutover_at)
        .bind(cutover_at).bind(cutover_at)
        .fetch_one(pool).await.map_err(anyhow::Error::from)?;
        if requires_replica != 0 && !crate::backup_maintenance::verified_replica_since(state, cutover_at).await.unwrap_or(false) {
            blockers.push("verified independent backup replica");
        }
        // Older records did not tie the operator's drill to a particular backup.
        let legacy_drill = requires_replica == 0 && sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM restore_runs WHERE state = 'succeeded' AND created_at >= ?)",
        ).bind(cutover_at).fetch_one(pool).await.map_err(anyhow::Error::from)? != 0;
        if drills == 0 && !legacy_drill {
            blockers.push("restore drill");
        }
        if backup_repository_inside_backend(pool, &row.source_backend_id, &backup_root).await? {
            blockers.push("backup on retired backend");
        }
    } else if row.stage != "retired" {
        blockers.push("protection period");
    }
    let (active_reads,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM media_reads WHERE backend_id = ? AND until_ms > ?",
    )
    .bind(&row.source_backend_id)
    .bind(db::now_ms())
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if active_reads > 0 {
        blockers.push("active reads");
    }
    let exception_list = backup_exceptions.join(",");
    let preview_sha = crate::ids::sha256_hex(
        format!(
            "{id}|{}|{object_count}|{exception_list}|{active_reads}",
            row.source_backend_id
        )
        .as_bytes(),
    );
    Ok(json!({
        "dry_run": true,
        "id": id,
        "stage": row.stage,
        "revision": row.revision,
        "namespace": row.source_backend_id,
        "object_count": object_count,
        "backup_exceptions": backup_exceptions,
        "active_reads": active_reads,
        "protected_until": row.protected_until,
        "pre_migration_backup_id": pre_migration_backup(pool, row.protected_until).await?,
        "drill_backup_id": drill_backup(pool, id, row.protected_until).await?,
        "blockers": blockers,
        "preview_sha": preview_sha
    }))
}

async fn revoke_backend_credentials(pool: &SqlitePool, backend_id: &str) -> AppResult<()> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT config_json FROM storage_backends WHERE id = ?",
    )
    .bind(backend_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let cfg: crate::storage::BackendConfig =
        serde_json::from_str(&row.0).map_err(anyhow::Error::from)?;
    let revoked = match cfg {
        crate::storage::BackendConfig::Local { .. } => {
            crate::storage::BackendConfig::Local { root: std::path::PathBuf::new() }
        }
        crate::storage::BackendConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            ..
        } => crate::storage::BackendConfig::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key: None,
            secret_key: None,
        },
        crate::storage::BackendConfig::WebDav { origin, collection, .. } => {
            crate::storage::BackendConfig::WebDav {
                origin,
                collection,
                username: None,
                password: None,
            }
        }
    };
    let json = serde_json::to_string(&revoked).map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE storage_backends SET config_json = ?, config_revision = config_revision + 1 WHERE id = ?",
    )
    .bind(json)
    .bind(backend_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

async fn backup_exception_ids(pool: &SqlitePool, id: &str) -> AppResult<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT DISTINCT i.blob_id
         FROM storage_migration_items i
         JOIN backup_pins p ON p.blob_id = i.blob_id
         JOIN backup_runs b ON b.id = p.backup_id AND b.state = 'succeeded'
         WHERE i.migration_id = ? AND i.state = 'verified'
         ORDER BY i.blob_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

pub async fn get(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String, String, i64, String, String, Option<i64>, String)>(
        "SELECT id, stage, state, revision, source_backend_id, target_backend_id, protected_until, direction
         FROM storage_migrations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(state = 'verified'), 0) FROM storage_migration_items WHERE migration_id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let byte_sql = if row.7 == "rollback" {
        "SELECT
            COALESCE(SUM(expected_size), 0),
            COALESCE(SUM(CASE WHEN state = 'verified' THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN state IN ('failed', 'exception') THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN state NOT IN ('verified', 'failed', 'exception', 'pending') THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN attempt_no > 1 THEN expected_size ELSE 0 END), 0)
         FROM storage_rollback_items WHERE migration_id = ?"
    } else {
        "SELECT
            COALESCE(SUM(expected_size), 0),
            COALESCE(SUM(CASE WHEN state = 'verified' THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN state IN ('failed', 'exception') THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN state NOT IN ('verified', 'failed', 'exception', 'pending') THEN expected_size ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN attempt_no > 1 THEN expected_size ELSE 0 END), 0)
         FROM storage_migration_items WHERE migration_id = ?"
    };
    let bytes: (i64, i64, i64, i64, i64) = sqlx::query_as(byte_sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let errors = sqlx::query_as::<_, (String,)>(
        "SELECT DISTINCT last_error FROM storage_migration_items
         WHERE migration_id = ? AND last_error IS NOT NULL
         ORDER BY last_error LIMIT 5",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?
    .into_iter()
    .map(|item| item.0)
    .collect::<Vec<_>>();
    let route: (String, i64, i64, Option<String>) = sqlx::query_as(
        "SELECT active_backend_id, routing_epoch, write_barrier, current_migration_id FROM storage_route WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let cleanup = route.3.as_deref() == Some(id)
        && db::kv_get(pool, &format!("maintenance_allow:{id}:purge_media"))
            .await
            .map_err(anyhow::Error::from)?
            .as_deref()
            == Some("1");
    let next_action = if row.2 == "paused" {
        Some("resume")
    } else {
        match row.1.as_str() {
            "planned" => Some("start"),
            "copying" => Some("wait"),
            "ready" => Some("cutover"),
            "opening" => Some("cutover"),
            "protected" => Some("retire"),
            _ => None,
        }
    };
    let reading = if route.0 == row.4 {
        "source"
    } else if route.0 == row.5 {
        "target"
    } else {
        "other"
    };
    let writes_paused = paused_writes(route.2 == 1, &row.2, route.3.is_some(), cleanup);
    Ok(json!({
        "id": row.0,
        "stage": row.1,
        "state": row.2,
        "revision": row.3,
        "source_backend_id": row.4,
        "target_backend_id": row.5,
        "protected_until": row.6,
        "objects_total": counts.0,
        "objects_verified": counts.1,
        "bytes_total": bytes.0,
        "bytes_verified": bytes.1,
        "bytes_failed": bytes.2,
        "bytes_unknown": bytes.3,
        "bytes_retried": bytes.4,
        "failure_summary": {
            "count": errors.len(),
            "errors": errors
        },
        "reading": reading,
        "writes_paused": writes_paused,
        "active_backend_id": route.0,
        "routing_epoch": route.1,
        "write_barrier": route.2 == 1,
        "next_action": next_action
    }))
}

async fn returned(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    let view = get(pool, actor, id).await?;
    emit_migration(pool, &actor.id, &view).await;
    Ok(view)
}

async fn emit_migration(pool: &SqlitePool, actor_id: &str, view: &serde_json::Value) {
    let id = view["id"].as_str().unwrap_or("");
    let _ = crate::events::emit(
        pool,
        "storage.migration.updated",
        "migration",
        Some(id),
        Some(id),
        view["revision"].as_i64(),
        Some(actor_id),
        json!({
            "id": view["id"],
            "stage": view["stage"],
            "state": view["state"],
            "reading": view["reading"],
            "routing_epoch": view["routing_epoch"],
            "write_barrier": view["write_barrier"],
            "next_action": view["next_action"],
            "bytes_verified": view["bytes_verified"],
            "bytes_failed": view["bytes_failed"],
            "objects_verified": view["objects_verified"],
            "objects_total": view["objects_total"]
        }),
    )
    .await;
}

fn paused_writes(barrier: bool, state: &str, diagnostic: bool, cleanup: bool) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if barrier {
        kinds.extend([
            "generate_variant",
            "convert_image",
            "build_archive",
            "caption_image",
            "scan_plan",
            "rebuild_search",
            "backup",
            "restore",
        ]);
        if !diagnostic {
            kinds.push("reconcile");
        }
        if !cleanup {
            kinds.push("purge_media");
        }
    }
    if state == "paused" {
        kinds.push("storage_migrate_copy");
    }
    kinds
}

pub async fn authorize_cleanup(
    pool: &SqlitePool,
    actor: &Actor,
    id: &str,
    kind: &str,
) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    if kind != "purge_media" {
        return Err(AppError::conflict(
            "CLEANUP_KIND",
            "maintenance only authorizes purge cleanup",
        ));
    }
    let route: (i64, Option<String>) = sqlx::query_as(
        "SELECT write_barrier, current_migration_id FROM storage_route WHERE id = 1",
    )
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if route.0 != 1 || route.1.as_deref() != Some(id) {
        return Err(AppError::conflict(
            "MIGRATION_LOCKED",
            "cleanup follows the migration holding the lock",
        ));
    }
    let mut tx = db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    db::kv_set(&mut tx, &format!("maintenance_allow:{id}:purge_media"), "1")
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    get(pool, actor, id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_needs_ten_percent_margin_or_an_owner_ack() {
        assert_eq!(capacity_decision(Some(110), 100, false), CapacityDecision::Ok);
        assert_eq!(capacity_decision(Some(109), 100, false), CapacityDecision::Short);
        assert_eq!(capacity_decision(None, 100, false), CapacityDecision::NeedsAck);
        assert_eq!(capacity_decision(None, 100, true), CapacityDecision::Ok);
        assert_eq!(capacity_decision(Some(0), 0, false), CapacityDecision::Ok);
    }
}
