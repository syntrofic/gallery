use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::Actor;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::{new_id, sha256_hex};
use crate::storage::{detect_mime, original_key, stage_key};

const MAX_ITEM: u64 = 100 * 1024 * 1024;
const MAX_BATCH_ITEMS: usize = 200;
const MAX_BATCH_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBatch {
    pub items: Vec<BatchItemIn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItemIn {
    pub client_item_id: String,
    pub name: String,
    pub size: u64,
}

#[derive(Serialize)]
pub struct BatchCreated {
    pub id: String,
    pub items: Vec<BatchItemMap>,
}

#[derive(Serialize)]
pub struct BatchItemMap {
    pub client_item_id: String,
    pub upload_item_id: String,
}

pub async fn create_batch(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    body: CreateBatch,
) -> AppResult<BatchCreated> {
    let album = crate::album::get(pool, Some(actor), album_id).await?;
    if !album.capabilities.upload {
        return Err(AppError::not_found());
    }
    let max_items = crate::settings::limits(pool)
        .await?
        .max_upload_items
        .clamp(1, MAX_BATCH_ITEMS as i64) as usize;
    if body.items.is_empty() || body.items.len() > max_items {
        return Err(AppError::unprocessable("BATCH_SIZE", format!("1..={max_items} items")));
    }
    let total: u64 = body.items.iter().map(|i| i.size).sum();
    if total > MAX_BATCH_BYTES {
        return Err(AppError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "BATCH_TOO_LARGE",
            "Payload Too Large",
            "declared batch exceeds 10 GiB",
        ));
    }
    let limits = crate::settings::limits(pool).await?;
    let (inflight,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(i.declared_size), 0)
         FROM upload_items i
         JOIN upload_batches b ON b.id = i.batch_id
         WHERE b.actor_id = ? AND i.state IN ('waiting', 'receiving', 'validating')",
    )
    .bind(&actor.id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if inflight.saturating_add(total as i64) > limits.staging_bytes {
        return Err(AppError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "STAGING_QUOTA",
            "Payload Too Large",
            "in-progress uploads exceed the staging allowance",
        ));
    }
    let batch_id = new_id();
    let now = db::now_ms();
    let spec_count = crate::versions::current_profile(pool)
        .await
        .map_err(anyhow::Error::from)?
        .1
        .len() as i64;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO upload_batches(id, album_id, actor_id, state, created_at, updated_at)
         VALUES(?, ?, ?, 'waiting', ?, ?)",
    )
    .bind(&batch_id)
    .bind(album_id)
    .bind(&actor.id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let reserved_slots = spec_count * body.items.len() as i64;
    let mut items = Vec::new();
    for it in body.items {
        let id = new_id();
        sqlx::query(
            "INSERT INTO upload_items(id, batch_id, client_item_id, original_name, declared_size, state)
             VALUES(?, ?, ?, ?, ?, 'waiting')",
        )
        .bind(&id)
        .bind(&batch_id)
        .bind(&it.client_item_id)
        .bind(&it.name)
        .bind(it.size as i64)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        items.push(BatchItemMap {
            client_item_id: it.client_item_id,
            upload_item_id: id,
        });
    }
    if let Err(err) = crate::jobs::reserve_slots(&mut tx, &batch_id, reserved_slots).await {
        if err.to_string().contains("QUEUE_FULL") {
            return Err(AppError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "QUEUE_FULL",
                "Service Unavailable",
                "job queue is full",
            ));
        }
        return Err(anyhow::Error::from(err).into());
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(BatchCreated { id: batch_id, items })
}

#[derive(Serialize)]
pub struct CommitResult {
    pub image_id: String,
    pub original_state: String,
    pub job_ids: Vec<String>,
}

pub async fn begin_receive(
    pool: &SqlitePool,
    actor: &Actor,
    batch_id: &str,
    item_id: &str,
) -> AppResult<()> {
    let batch = sqlx::query_as::<_, (String, String, String)>(
        "SELECT album_id, actor_id, state FROM upload_batches WHERE id = ?",
    )
    .bind(batch_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if batch.1 != actor.id && actor.role != "owner" {
        return Err(AppError::not_found());
    }
    if batch.2 == "canceled" {
        return Err(AppError::conflict("BATCH_CANCELED", "batch canceled"));
    }
    let album = crate::album::get(pool, Some(actor), &batch.0).await?;
    if !album.capabilities.upload {
        return Err(AppError::not_found());
    }
    let item = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
        "SELECT state, staged_key, updated_at FROM upload_items WHERE id = ? AND batch_id = ?",
    )
    .bind(item_id)
    .bind(batch_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if item.0 != "waiting" && item.0 != "receiving" {
        return Ok(());
    }
    if item.0 == "receiving" && item.1.as_ref().is_some_and(|key| !key.is_empty()) {
        let fresh = item.2.unwrap_or(0) > db::now_ms() - UPLOAD_CLAIM_MS;
        if fresh {
            return Err(AppError::conflict(
                "UPLOAD_IN_PROGRESS",
                format!("upload is already in progress; see /api/v1/uploads/{batch_id}"),
            ));
        }
        sqlx::query("UPDATE upload_items SET updated_at = ? WHERE id = ?")
            .bind(db::now_ms())
            .bind(item_id)
            .execute(pool)
            .await
            .map_err(anyhow::Error::from)?;
        return Ok(());
    }
    let staged = stage_key(item_id, &new_id());
    sqlx::query(
        "UPDATE upload_items SET state = 'receiving', staged_key = ?, updated_at = ?
         WHERE id = ? AND batch_id = ? AND state IN ('waiting', 'receiving')",
    )
    .bind(&staged)
    .bind(db::now_ms())
    .bind(item_id)
    .bind(batch_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

pub async fn put_item(
    state: &crate::app::AppState,
    actor: &Actor,
    batch_id: &str,
    item_id: &str,
    bytes: &[u8],
) -> AppResult<(u16, CommitResult)> {
    if bytes.len() as u64 > MAX_ITEM {
        return Err(AppError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "FILE_TOO_LARGE",
            "Payload Too Large",
            "file exceeds 100 MiB",
        ));
    }
    let batch = sqlx::query_as::<_, (String, String, String)>(
        "SELECT album_id, actor_id, state FROM upload_batches WHERE id = ?",
    )
    .bind(batch_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if batch.1 != actor.id && actor.role != "owner" {
        return Err(AppError::not_found());
    }
    if batch.2 == "canceled" {
        return Err(AppError::conflict("BATCH_CANCELED", "batch canceled"));
    }
    let album = crate::album::get(&state.pool, Some(actor), &batch.0).await?;
    if !album.capabilities.upload {
        return Err(AppError::not_found());
    }
    let item = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT state, original_name, result_image_id, computed_sha, staged_key FROM upload_items WHERE id = ? AND batch_id = ?",
    )
    .bind(item_id)
    .bind(batch_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    let mime = detect_mime(bytes).ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_TYPE",
            "Unsupported Media Type",
            "unrecognized image",
        )
    })?;
    let probe = crate::processing::probe_bytes(bytes).map_err(|e| {
        AppError::unprocessable("UNDECODABLE", e.to_string())
    })?;
    let sha = sha256_hex(bytes);
    if item.3.as_ref().is_some_and(|previous| previous != &sha) {
        return Err(AppError::conflict(
            "ITEM_SHA",
            "this item already accepted a different file; create a new item",
        ));
    }
    if item.0 == "committed" {
        let image_id = item.2.ok_or_else(AppError::not_found)?;
        let jobs = sqlx::query_as::<_, (String,)>(
            "SELECT id FROM jobs
             WHERE kind = 'generate_variant' AND json_extract(scope_json, '$.image_id') = ?
             ORDER BY created_at",
        )
        .bind(&image_id)
        .fetch_all(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        return Ok((
            200,
            CommitResult {
                image_id,
                original_state: "verified".into(),
                job_ids: jobs.into_iter().map(|(id,)| id).collect(),
            },
        ));
    }
    sqlx::query("UPDATE upload_items SET state = 'receiving', updated_at = ? WHERE id = ?")
        .bind(db::now_ms())
        .bind(item_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let staged = item
        .4
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| stage_key(item_id, &new_id()));
    let (route_backend, route_epoch) =
        crate::db::current_route(&state.pool).await.map_err(anyhow::Error::from)?;
    let media = crate::storage::open_by_id(&state.pool, &route_backend)
        .await
        .map_err(anyhow::Error::from)?;
    media
        .backend()
        .stage(&staged, bytes)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE upload_items SET state = 'validating', computed_sha = ?, staged_key = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&sha)
    .bind(&staged)
    .bind(db::now_ms())
    .bind(item_id)
        .execute(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;

    if crate::db::write_barrier(&state.pool).await.map_err(anyhow::Error::from)? {
        return Err(AppError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "WRITE_BARRIER",
            "Unavailable",
            "media writes frozen",
        ));
    }

    let mut tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
    let existing: Option<(String, String)> =
        sqlx::query_as("SELECT id, state FROM images WHERE source_sha256 = ?")
            .bind(&sha)
            .fetch_optional(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    let (image_id, is_new) = if let Some((id, image_state)) = existing {
        sqlx::query("DELETE FROM original_reservations WHERE source_sha256 = ?")
            .bind(&sha)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        if image_state != "active" {
            tx.rollback().await.map_err(anyhow::Error::from)?;
            sqlx::query(
                "UPDATE upload_items SET state = 'failed', failure_code = 'IMAGE_UNAVAILABLE' WHERE id = ?",
            )
            .bind(item_id)
            .execute(&state.pool)
            .await
            .map_err(anyhow::Error::from)?;
            return Err(AppError::conflict(
                "IMAGE_UNAVAILABLE",
                "withdrawn image cannot be attached",
            ));
        }
        (id, false)
    } else {
        let (image_id, key) = match claim_original(&mut tx, &sha, mime, item_id).await {
            Ok(reserved) => reserved,
            Err(err) if err.to_string().contains("LEASE_HELD") => {
                tx.rollback().await.map_err(anyhow::Error::from)?;
                return Err(AppError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "LEASE_HELD",
                    "Unavailable",
                    "original hash is reserved",
                ));
            }
            Err(err) => return Err(err.into()),
        };
        tx.commit().await.map_err(anyhow::Error::from)?;
        match media.hash_object(&key, bytes.len() as i64).await {
            Ok((found, _)) if found == sha => {}
            Err(err) if crate::storage::is_missing(&err) => {
                media
                    .put_bytes(&key, bytes, mime)
                    .await
                    .map_err(anyhow::Error::from)?;
            }
            result => {
                if result.is_ok() || result.is_err_and(|err| err.is::<crate::storage::ObjectMismatch>()) {
                    return Err(AppError::conflict(
                        "OBJECT_MISMATCH",
                        "stored original does not match the upload",
                    ));
                }
                return Err(AppError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "Unavailable",
                    "storage read failed",
                ));
            }
        }
        let library_bytes = crate::settings::limits(&state.pool).await?.library_bytes;
        tx = crate::db::begin_immediate(&state.pool).await.map_err(anyhow::Error::from)?;
        let raced: Option<(String, String)> =
            sqlx::query_as("SELECT id, state FROM images WHERE source_sha256 = ?")
                .bind(&sha)
                .fetch_optional(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        if let Some((id, image_state)) = raced {
            sqlx::query("DELETE FROM original_reservations WHERE source_sha256 = ?")
                .bind(&sha)
                .execute(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
            if image_state != "active" {
                tx.rollback().await.map_err(anyhow::Error::from)?;
                return Err(AppError::conflict(
                    "IMAGE_UNAVAILABLE",
                    "withdrawn image cannot be attached",
                ));
            }
            (id, false)
        } else {
        let (used,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(sz), 0) FROM (
                SELECT DISTINCT mb.id, mb.size AS sz
                FROM upload_items i
                JOIN upload_batches b ON b.id = i.batch_id
                JOIN images img ON img.id = i.result_image_id
                JOIN media_blobs mb ON mb.id = img.original_blob_id
                WHERE b.actor_id = ? AND i.state = 'committed'
             )",
        )
        .bind(&actor.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        if used.saturating_add(bytes.len() as i64) > library_bytes {
            tx.rollback().await.map_err(anyhow::Error::from)?;
            return Err(AppError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "LIBRARY_QUOTA",
                "Payload Too Large",
                "stored originals exceed the library allowance",
            ));
        }
        if !crate::db::publish_route_ok(&mut *tx, &media.backend_id, route_epoch)
            .await
            .map_err(anyhow::Error::from)?
        {
            tx.rollback().await.map_err(anyhow::Error::from)?;
            return Err(AppError::conflict(
                "ROUTE_CHANGED",
                "storage route changed before the original was published",
            ));
        }
        let blob_id = crate::storage::record_blob(
            &mut tx,
            &sha,
            bytes.len() as i64,
            mime,
            "original",
            &media.backend_id,
            &key,
        )
        .await
        .map_err(anyhow::Error::from)?;
        let seq: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(created_seq), 0) + 1 FROM images")
            .fetch_one(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        sqlx::query(
            "INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height, orientation, original_mime, created_seq, created_at)
             VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&image_id)
        .bind(&sha)
        .bind(&blob_id)
        .bind(&item.1)
        .bind(probe.width as i64)
        .bind(probe.height as i64)
        .bind(probe.orientation as i64)
        .bind(mime)
        .bind(seq.0)
        .bind(db::now_ms())
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        crate::db::bump_catalog(&mut tx)
            .await
            .map_err(anyhow::Error::from)?;
        sqlx::query("DELETE FROM original_reservations WHERE source_sha256 = ?")
            .bind(&sha)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        (image_id, true)
        }
    };
    let sort = {
        let row: (Option<i64>,) =
            sqlx::query_as("SELECT MAX(sort_key) FROM album_images WHERE album_id = ?")
                .bind(&batch.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        row.0.unwrap_or(0) + 1024
    };
    sqlx::query(
        "INSERT OR IGNORE INTO album_images(album_id, image_id, sort_key, added_at, display_name)
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind(&batch.0)
    .bind(&image_id)
    .bind(sort)
    .bind(db::now_ms())
    .bind(&item.1)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE albums SET content_revision = content_revision + 1, revision = revision + 1 WHERE id = ?")
        .bind(&batch.0)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let job_ids = crate::versions::plan_variants(&mut tx, &image_id, Some(&actor.id), Some(batch_id))
        .await
        .map_err(anyhow::Error::from)?;
    if is_new && state.config.ai_enabled {
        if let Err(err) = crate::ai::enqueue_caption(&mut tx, &actor.id, &image_id, &sha, None, true).await {
            if !err.to_string().contains("QUEUE_FULL") {
                return Err(err.into());
            }
        }
    }
    sqlx::query(
        "UPDATE upload_items SET state = 'committed', result_image_id = ? WHERE id = ?",
    )
    .bind(&image_id)
    .bind(item_id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    refresh_batch(&mut tx, batch_id)
        .await
        .map_err(anyhow::Error::from)?;
    crate::search::sync_image(&mut tx, &image_id)
        .await
        .map_err(anyhow::Error::from)?;
    crate::events::emit_tx(
        &mut tx,
        "upload.committed",
        "album",
        Some(&batch.0),
        Some(&image_id),
        None,
        Some(&actor.id),
        serde_json::json!({"batch_id": batch_id, "item_id": item_id}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    let _ = crate::album::invalidate_archives(&state.pool, &batch.0).await;
    state.wake_jobs();
    let _ = is_new;
    let _ = media.backend().delete(&staged).await;
    Ok((
        201,
        CommitResult {
            image_id,
            original_state: "verified".into(),
            job_ids,
        },
    ))
}

pub const UPLOAD_HOLD_MS: i64 = 24 * 60 * 60 * 1000;
pub const UPLOAD_CLAIM_MS: i64 = 30_000;

pub async fn reclaim_abandoned_uploads(
    pool: &SqlitePool,
    media: &crate::storage::Storage,
) -> anyhow::Result<i64> {
    let cutoff = db::now_ms() - UPLOAD_HOLD_MS;
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT i.id, i.batch_id, i.staged_key
         FROM upload_items i
         WHERE i.state IN ('receiving', 'validating')
           AND i.updated_at IS NOT NULL
           AND i.updated_at < ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut expired = 0i64;
    let mut batches = Vec::new();
    for (item_id, batch_id, staged_key) in rows {
        if let Some(key) = staged_key.as_ref().filter(|key| !key.is_empty()) {
            let referenced: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM media_locations
                 WHERE backend_id = ? AND object_key = ? AND state = 'verified'",
            )
            .bind(&media.backend_id)
            .bind(key)
            .fetch_one(pool)
            .await?;
            if referenced.0 == 0 {
                let _ = media.delete_key(key).await;
            }
        }
        sqlx::query(
            "UPDATE upload_items SET state = 'failed', failure_code = 'UPLOAD_EXPIRED', updated_at = ?
             WHERE id = ? AND state IN ('receiving', 'validating')",
        )
        .bind(db::now_ms())
        .bind(&item_id)
        .execute(pool)
        .await?;
        if !batches.contains(&batch_id) {
            batches.push(batch_id);
        }
        expired += 1;
    }
    for batch_id in batches {
        let mut tx = db::begin_immediate(pool).await?;
        refresh_batch(&mut tx, &batch_id).await?;
        tx.commit().await?;
    }
    Ok(expired)
}

pub async fn reclaim_expired_reservations(
    pool: &SqlitePool,
    media: &crate::storage::Storage,
) -> anyhow::Result<i64> {
    let now = db::now_ms();
    let mut tx = crate::db::begin_immediate(pool).await?;
    let orphan_keys = sqlx::query_as::<_, (String,)>(
        "SELECT r.planned_object_key
         FROM original_reservations r
         WHERE r.lease_until <= ?
           AND r.image_id NOT IN (SELECT id FROM images)
           AND NOT EXISTS (
                SELECT 1 FROM media_locations l
                WHERE l.backend_id = ? AND l.object_key = r.planned_object_key
                  AND l.state = 'verified'
           )",
    )
    .bind(now)
    .bind(&media.backend_id)
    .fetch_all(&mut *tx)
    .await?;
    let removed = sqlx::query(
        "DELETE FROM original_reservations
         WHERE lease_until <= ? AND image_id NOT IN (SELECT id FROM images)",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    for (key,) in &orphan_keys {
        media.delete_key(key).await?;
    }
    Ok(removed.rows_affected() as i64)
}

async fn claim_original(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    sha: &str,
    mime: &str,
    item_id: &str,
) -> anyhow::Result<(String, String)> {
    let now = db::now_ms();
    sqlx::query(
        "DELETE FROM original_reservations
         WHERE lease_until <= ? AND source_sha256 != ?
           AND image_id NOT IN (SELECT id FROM images)",
    )
    .bind(now)
    .bind(sha)
    .execute(&mut **tx)
    .await?;
    let current = sqlx::query_as::<_, (String, String, i64, String)>(
        "SELECT image_id, lease_token, lease_until, planned_object_key
         FROM original_reservations WHERE source_sha256 = ?",
    )
    .bind(sha)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((image_id, token, until, key)) = current {
        if until > now && token != item_id {
            anyhow::bail!("LEASE_HELD");
        }
        sqlx::query(
            "UPDATE original_reservations SET lease_token = ?, lease_until = ? WHERE source_sha256 = ?",
        )
        .bind(item_id)
        .bind(now + 10 * 60 * 1000)
        .bind(sha)
        .execute(&mut **tx)
        .await?;
        return Ok((image_id, key));
    }
    let image_id = new_id();
    let key = original_key(&image_id, sha, mime);
    sqlx::query(
        "INSERT INTO original_reservations(source_sha256, image_id, lease_token, lease_until, planned_object_key)
         VALUES(?, ?, ?, ?, ?)",
    )
    .bind(sha)
    .bind(&image_id)
    .bind(item_id)
    .bind(now + 10 * 60 * 1000)
    .bind(&key)
    .execute(&mut **tx)
    .await?;
    Ok((image_id, key))
}

async fn refresh_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    batch_id: &str,
) -> Result<()> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT state FROM upload_items WHERE batch_id = ?")
            .bind(batch_id)
            .fetch_all(&mut **tx)
            .await?;
    let states: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let state = if states.iter().all(|s| *s == "committed") {
        "succeeded"
    } else if states.iter().all(|s| *s == "failed") {
        "failed"
    } else if states.iter().all(|s| *s == "canceled") {
        "canceled"
    } else if states.iter().any(|s| *s == "committed")
        && states
            .iter()
            .any(|s| *s == "failed" || *s == "canceled")
        && !states
            .iter()
            .any(|s| matches!(*s, "waiting" | "receiving" | "validating"))
    {
        "partial"
    } else {
        "running"
    };
    sqlx::query("UPDATE upload_batches SET state = ?, updated_at = ? WHERE id = ?")
        .bind(state)
        .bind(db::now_ms())
        .bind(batch_id)
        .execute(&mut **tx)
        .await?;
    if matches!(state, "succeeded" | "failed" | "canceled" | "partial") {
        crate::jobs::release_reservation(tx, batch_id).await?;
    }
    Ok(())
}

pub async fn get_batch(pool: &SqlitePool, actor: &Actor, batch_id: &str) -> AppResult<serde_json::Value> {
    let batch = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT album_id, actor_id, state, updated_at FROM upload_batches WHERE id = ?",
    )
    .bind(batch_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if batch.1 != actor.id && actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let items = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>, String, i64)>(
        "SELECT id, client_item_id, state, result_image_id, failure_code, original_name, declared_size FROM upload_items WHERE batch_id = ?",
    )
    .bind(batch_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "id": batch_id,
        "album_id": batch.0,
        "state": batch.2,
        "items": items.into_iter().map(|(id, client, state, image, fail, name, size)| serde_json::json!({
            "id": id,
            "client_item_id": client,
            "state": state,
            "name": name,
            "size": size,
            "image_id": image,
            "failure_code": fail
        })).collect::<Vec<_>>()
    }))
}

pub async fn cancel_batch(
    pool: &SqlitePool,
    actor: &Actor,
    batch_id: &str,
) -> AppResult<(u16, serde_json::Value)> {
    let current = get_batch(pool, actor, batch_id).await?;
    let state = current["state"].as_str().unwrap_or("");
    if matches!(state, "succeeded" | "partial" | "failed" | "canceled") {
        return Ok((200, current));
    }
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE upload_items SET state = 'canceled'
         WHERE batch_id = ? AND state IN ('waiting', 'receiving', 'validating')",
    )
    .bind(batch_id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    refresh_batch(&mut tx, batch_id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok((202, get_batch(pool, actor, batch_id).await?))
}

pub async fn list_batches(pool: &SqlitePool, actor: &Actor) -> AppResult<serde_json::Value> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, album_id, state FROM upload_batches WHERE actor_id = ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&actor.id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "items": rows.into_iter().map(|(id, album_id, state)| serde_json::json!({
            "id": id, "album_id": album_id, "state": state
        })).collect::<Vec<_>>(),
        "next_cursor": null
    }))
}
