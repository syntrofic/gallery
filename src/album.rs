use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::{album_capabilities, Actor, CapabilitySet};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::new_id;

#[derive(Clone, Debug, Serialize)]
pub struct AlbumView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub cover_image_id: Option<String>,
    pub visibility: String,
    pub archive_enabled: bool,
    pub revision: i64,
    pub content_revision: i64,
    pub acl_revision: i64,
    pub capabilities: CapabilitySet,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAlbum {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PatchAlbum {
    pub name: Option<String>,
    pub description: Option<String>,
    pub cover_image_id: Option<String>,
    pub expected_revision: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessPatch {
    pub visibility: Option<String>,
    pub archive_enabled: Option<bool>,
    pub grants: Option<Vec<GrantIn>>,
    pub expected_revision: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantIn {
    pub actor_id: String,
    pub browse: Option<bool>,
    pub edit: Option<bool>,
    pub upload: Option<bool>,
    pub view_original: Option<bool>,
    pub download: Option<bool>,
}

const SORT_GAP: i64 = 1024;

pub fn validate_name(name: &str) -> AppResult<()> {
    let n = name.chars().count();
    if n < 1 || n > 120 {
        return Err(AppError::unprocessable("INVALID_NAME", "album name length"));
    }
    Ok(())
}

pub async fn create(pool: &SqlitePool, actor: &Actor, body: CreateAlbum) -> AppResult<AlbumView> {
    validate_name(&body.name)?;
    let desc = body.description.unwrap_or_default();
    if desc.chars().count() > 4000 {
        return Err(AppError::unprocessable("INVALID_DESCRIPTION", "too long"));
    }
    let id = new_id();
    let now = db::now_ms();
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO albums(id, name, description, created_at) VALUES(?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&body.name)
    .bind(&desc)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    crate::search::sync_album(&mut tx, &id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    crate::events::emit(
        pool,
        "album.created",
        "album",
        Some(&id),
        Some(&id),
        Some(1),
        Some(&actor.id),
        serde_json::json!({}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    get(pool, Some(actor), &id).await
}

pub async fn get(pool: &SqlitePool, actor: Option<&Actor>, id: &str) -> AppResult<AlbumView> {
    let caps = album_capabilities(pool, actor, id).await?;
    if !caps.browse {
        return Err(AppError::not_found());
    }
    let row = sqlx::query_as::<_, (String, String, Option<String>, String, i64, i64, i64, i64)>(
        "SELECT name, description, cover_image_id, visibility, archive_enabled, revision, content_revision, acl_revision
         FROM albums WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    Ok(AlbumView {
        id: id.to_string(),
        name: row.0,
        description: row.1,
        cover_image_id: displayed_cover(pool, id, row.2).await?,
        visibility: row.3,
        archive_enabled: row.4 == 1,
        revision: row.5,
        content_revision: row.6,
        acl_revision: row.7,
        capabilities: caps,
    })
}

pub async fn list(pool: &SqlitePool, actor: Option<&Actor>) -> AppResult<Vec<AlbumView>> {
    let ids: Vec<(String,)> = if let Some(actor) = actor {
        if actor.role == "owner" {
            sqlx::query_as("SELECT id FROM albums WHERE deleted_at IS NULL ORDER BY created_at DESC")
                .fetch_all(pool)
                .await
                .map_err(anyhow::Error::from)?
        } else {
            sqlx::query_as(
                "SELECT a.id FROM albums a
                 LEFT JOIN album_grants g ON g.album_id = a.id AND g.actor_id = ?
                 WHERE a.deleted_at IS NULL AND (a.visibility = 'public' OR g.browse = 1)
                 ORDER BY a.created_at DESC",
            )
            .bind(&actor.id)
            .fetch_all(pool)
            .await
            .map_err(anyhow::Error::from)?
        }
    } else {
        sqlx::query_as("SELECT id FROM albums WHERE deleted_at IS NULL AND visibility = 'public' ORDER BY created_at DESC")
            .fetch_all(pool)
            .await
            .map_err(anyhow::Error::from)?
    };
    let mut out = Vec::new();
    for (id,) in ids {
        if let Ok(v) = get(pool, actor, &id).await {
            out.push(v);
        }
    }
    Ok(out)
}

pub async fn refuse_stopped_turn(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
) -> AppResult<()> {
    let turn = sqlx::query_as::<_, (String,)>(
        "SELECT state FROM agent_turns WHERE session_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(anyhow::Error::from)?;
    if matches!(
        turn.as_ref().map(|row| row.0.as_str()),
        Some("stop_requested" | "interrupted")
    ) {
        return Err(AppError::conflict("TURN_STOPPED", "the current turn was stopped"));
    }
    Ok(())
}

pub async fn patch(
    pool: &SqlitePool,
    actor: &Actor,
    id: &str,
    body: PatchAlbum,
    turn_session: Option<&str>,
) -> AppResult<AlbumView> {
    let current = get(pool, Some(actor), id).await?;
    if !current.capabilities.edit {
        return Err(AppError::not_found());
    }
    if let Some(expected) = body.expected_revision {
        if expected != current.revision {
            return Err(AppError::precondition("REVISION_CONFLICT", "album revision"));
        }
    } else {
        return Err(AppError::new(
            axum::http::StatusCode::PRECONDITION_REQUIRED,
            "REVISION_REQUIRED",
            "Precondition Required",
            "expected_revision required",
        ));
    }
    if let Some(name) = &body.name {
        validate_name(name)?;
    }
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    if let Some(session_id) = turn_session {
        refuse_stopped_turn(&mut tx, session_id).await?;
    }
    sqlx::query(
        "UPDATE albums SET
            name = COALESCE(?, name),
            description = COALESCE(?, description),
            cover_image_id = COALESCE(?, cover_image_id),
            revision = revision + 1
         WHERE id = ?",
    )
    .bind(body.name.as_deref())
    .bind(body.description.as_deref())
    .bind(body.cover_image_id.as_deref())
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    crate::search::sync_album(&mut tx, id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    get(pool, Some(actor), id).await
}

pub async fn soft_delete(pool: &SqlitePool, actor: &Actor, id: &str, expected: i64) -> AppResult<()> {
    if actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let row = sqlx::query_as::<_, (i64, Option<i64>)>(
        "SELECT revision, deleted_at FROM albums WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    check_rev(Some(expected), row.0)?;
    if row.1.is_some() {
        return Ok(());
    }
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let revision = sqlx::query_as::<_, (i64,)>(
        "UPDATE albums SET deleted_at = ?, revision = revision + 1, content_revision = content_revision + 1
         WHERE id = ? AND revision = ? AND deleted_at IS NULL
         RETURNING revision",
    )
    .bind(db::now_ms())
    .bind(id)
    .bind(expected)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((revision,)) = revision else {
        return Err(AppError::precondition("REVISION_CONFLICT", "album revision"));
    };
    sqlx::query(
        "UPDATE archive_packages SET archive_state = 'stale'
         WHERE album_id = ? AND archive_state IN ('queued', 'building', 'ready')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    record_album_change(&mut tx, actor, id, revision, "album.delete", "album.deleted").await?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}

pub async fn restore_album(
    pool: &SqlitePool,
    actor: &Actor,
    id: &str,
    expected: i64,
) -> AppResult<AlbumView> {
    if actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let row = sqlx::query_as::<_, (i64, Option<i64>)>(
        "SELECT revision, deleted_at FROM albums WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    check_rev(Some(expected), row.0)?;
    if row.1.is_some() {
        let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
        let revision = sqlx::query_as::<_, (i64,)>(
            "UPDATE albums SET deleted_at = NULL, revision = revision + 1, content_revision = content_revision + 1
             WHERE id = ? AND revision = ? AND deleted_at IS NOT NULL
             RETURNING revision",
        )
        .bind(id)
        .bind(expected)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let Some((revision,)) = revision else {
            return Err(AppError::precondition("REVISION_CONFLICT", "album revision"));
        };
        record_album_change(&mut tx, actor, id, revision, "album.restore", "album.restored").await?;
        tx.commit().await.map_err(anyhow::Error::from)?;
    }
    get(pool, Some(actor), id).await
}

async fn record_album_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &Actor,
    album_id: &str,
    revision: i64,
    audit_action: &str,
    event_type: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO audit_log(request_id, actor_id, action, target, result, detail, occurred_at)
         VALUES('album', ?, ?, ?, 'ok', NULL, ?)",
    )
    .bind(&actor.id)
    .bind(audit_action)
    .bind(album_id)
    .bind(db::now_ms())
    .execute(&mut **tx)
    .await
    .map_err(anyhow::Error::from)?;
    crate::events::emit_tx(
        tx,
        event_type,
        "album",
        Some(album_id),
        Some(album_id),
        Some(revision),
        Some(&actor.id),
        serde_json::json!({"revision": revision}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    crate::search::sync_album(tx, album_id)
        .await
        .map_err(anyhow::Error::from)?;
    let image_ids = sqlx::query_as::<_, (String,)>("SELECT image_id FROM album_images WHERE album_id = ?")
        .bind(album_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(anyhow::Error::from)?;
    for (image_id,) in image_ids {
        crate::search::sync_image(tx, &image_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

async fn image_visible_to(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &Actor,
    image_id: &str,
) -> AppResult<bool> {
    if actor.role == "owner" {
        let row = sqlx::query_as::<_, (String,)>("SELECT id FROM images WHERE id = ? AND state = 'active'")
            .bind(image_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(anyhow::Error::from)?;
        return Ok(row.is_some());
    }
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM images i
         WHERE i.id = ? AND i.state = 'active'
           AND EXISTS (
             SELECT 1 FROM album_images ai
             JOIN albums a ON a.id = ai.album_id
             LEFT JOIN album_grants g ON g.album_id = a.id AND g.actor_id = ?
             WHERE ai.image_id = i.id AND a.deleted_at IS NULL
               AND (a.visibility = 'public' OR g.browse = 1)
           )",
    )
    .bind(image_id)
    .bind(&actor.id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(row.is_some())
}

pub async fn next_sort_key(pool: &SqlitePool, album_id: &str) -> Result<i64> {
    let row: (Option<i64>,) =
        sqlx::query_as("SELECT MAX(sort_key) FROM album_images WHERE album_id = ?")
            .bind(album_id)
            .fetch_one(pool)
            .await?;
    Ok(row.0.unwrap_or(0) + SORT_GAP)
}

pub async fn add_images(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    image_ids: &[String],
    expected_revision: Option<i64>,
) -> AppResult<serde_json::Value> {
    let current = get(pool, Some(actor), album_id).await?;
    if !current.capabilities.edit && !current.capabilities.upload {
        return Err(AppError::not_found());
    }
    check_rev(expected_revision, current.revision)?;
    let mut results = Vec::new();
    let mut inserted = 0_i64;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    for image_id in image_ids {
        if !image_visible_to(&mut tx, actor, image_id).await? {
            results.push(serde_json::json!({"image_id": image_id, "ok": false, "code": "NOT_FOUND"}));
            continue;
        }
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT sort_key FROM album_images WHERE album_id = ? AND image_id = ?",
        )
        .bind(album_id)
        .bind(image_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        if existing.is_some() {
            results.push(serde_json::json!({"image_id": image_id, "ok": false, "code": "ALREADY_EXISTS"}));
            continue;
        }
        let sort = {
            let row: (Option<i64>,) =
                sqlx::query_as("SELECT MAX(sort_key) FROM album_images WHERE album_id = ?")
                    .bind(album_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(anyhow::Error::from)?;
            row.0.unwrap_or(0) + SORT_GAP
        };
        sqlx::query(
            "INSERT INTO album_images(album_id, image_id, sort_key, added_at, display_name)
             VALUES(?, ?, ?, ?, NULL)",
        )
        .bind(album_id)
        .bind(image_id)
        .bind(sort)
        .bind(db::now_ms())
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        inserted += 1;
        crate::search::sync_image(&mut tx, image_id)
            .await
            .map_err(anyhow::Error::from)?;
        results.push(serde_json::json!({"image_id": image_id, "ok": true}));
    }
    if inserted > 0 {
        sqlx::query("UPDATE albums SET revision = revision + 1, content_revision = content_revision + 1 WHERE id = ?")
            .bind(album_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    if inserted > 0 {
        let _ = invalidate_archives(pool, album_id).await;
    }
    let album = get(pool, Some(actor), album_id).await?;
    Ok(serde_json::json!({
        "revision": album.revision,
        "content_revision": album.content_revision,
        "items": results
    }))
}

pub async fn rename_display(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    image_id: &str,
    display_name: &str,
    expected_revision: i64,
) -> AppResult<serde_json::Value> {
    let current = get(pool, Some(actor), album_id).await?;
    if !current.capabilities.edit {
        return Err(AppError::not_found());
    }
    check_rev(Some(expected_revision), current.revision)?;
    let trimmed = display_name.trim();
    let stored = if trimmed.is_empty() {
        None
    } else {
        if trimmed.chars().count() > 120 {
            return Err(AppError::unprocessable("INVALID_NAME", "display name length"));
        }
        Some(trimmed.to_string())
    };
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let member: Option<(String,)> = sqlx::query_as(
        "SELECT image_id FROM album_images WHERE album_id = ? AND image_id = ?",
    )
    .bind(album_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if member.is_none() {
        return Err(AppError::not_found());
    }
    sqlx::query("UPDATE album_images SET display_name = ? WHERE album_id = ? AND image_id = ?")
        .bind(stored.as_deref())
        .bind(album_id)
        .bind(image_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let updated = sqlx::query(
        "UPDATE albums SET revision = revision + 1 WHERE id = ? AND revision = ?",
    )
    .bind(album_id)
    .bind(expected_revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if updated.rows_affected() != 1 {
        return Err(AppError::precondition("REVISION_CONFLICT", "album revision"));
    }
    crate::search::sync_image(&mut tx, image_id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    let album = get(pool, Some(actor), album_id).await?;
    Ok(serde_json::json!({
        "album_id": album_id,
        "image_id": image_id,
        "display_name": stored,
        "revision": album.revision
    }))
}

pub async fn remove_images(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    image_ids: &[String],
    expected_revision: Option<i64>,
) -> AppResult<serde_json::Value> {
    let current = get(pool, Some(actor), album_id).await?;
    if !current.capabilities.edit {
        return Err(AppError::not_found());
    }
    check_rev(expected_revision, current.revision)?;
    let mut results = Vec::new();
    for image_id in image_ids {
        let res = sqlx::query("DELETE FROM album_images WHERE album_id = ? AND image_id = ?")
            .bind(album_id)
            .bind(image_id)
            .execute(pool)
            .await
            .map_err(anyhow::Error::from)?;
        results.push(serde_json::json!({"image_id": image_id, "ok": res.rows_affected() > 0}));
    }
    sqlx::query(
        "UPDATE albums SET revision = revision + 1, content_revision = content_revision + 1,
         cover_image_id = CASE WHEN cover_image_id IN (SELECT value FROM json_each(?)) THEN NULL ELSE cover_image_id END
         WHERE id = ?",
    )
    .bind(serde_json::to_string(image_ids).unwrap_or_else(|_| "[]".into()))
    .bind(album_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    for image_id in image_ids {
        crate::search::sync_image(&mut tx, image_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    let _ = invalidate_archives(pool, album_id).await;
    let album = get(pool, Some(actor), album_id).await?;
    Ok(serde_json::json!({
        "revision": album.revision,
        "content_revision": album.content_revision,
        "items": results
    }))
}

pub async fn reorder(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    image_id: &str,
    before_id: Option<&str>,
    after_id: Option<&str>,
    expected_revision: Option<i64>,
    turn_session: Option<&str>,
) -> AppResult<serde_json::Value> {
    let current = get(pool, Some(actor), album_id).await?;
    if !current.capabilities.edit {
        return Err(AppError::not_found());
    }
    check_rev(expected_revision, current.revision)?;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    if let Some(session_id) = turn_session {
        refuse_stopped_turn(&mut tx, session_id).await?;
    }
    let mut before_key = member_sort_key(&mut tx, album_id, before_id).await?;
    let mut after_key = member_sort_key(&mut tx, album_id, after_id).await?;
    if !sort_gap_fits(after_key, before_key) {
        rebalance_sort_keys(&mut tx, album_id).await?;
        before_key = member_sort_key(&mut tx, album_id, before_id).await?;
        after_key = member_sort_key(&mut tx, album_id, after_id).await?;
    }
    let new_key = match (after_key, before_key) {
        (Some(a), Some(b)) if b > a + 1 => a + (b - a) / 2,
        (Some(a), None) => a + SORT_GAP,
        (None, Some(b)) => b - SORT_GAP,
        (None, None) => SORT_GAP,
        _ => {
            return Err(AppError::conflict("SORT_EXHAUSTED", "album order could not be spaced"));
        }
    };
    sqlx::query("UPDATE album_images SET sort_key = ? WHERE album_id = ? AND image_id = ?")
        .bind(new_key)
        .bind(album_id)
        .bind(image_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE albums SET revision = revision + 1, content_revision = content_revision + 1 WHERE id = ?")
        .bind(album_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    let _ = invalidate_archives(pool, album_id).await;
    let album = get(pool, Some(actor), album_id).await?;
    Ok(serde_json::json!({
        "revision": album.revision,
        "content_revision": album.content_revision
    }))
}

async fn member_sort_key(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    album_id: &str,
    image_id: Option<&str>,
) -> AppResult<Option<i64>> {
    let Some(image_id) = image_id else {
        return Ok(None);
    };
    let key = sqlx::query_as::<_, (i64,)>("SELECT sort_key FROM album_images WHERE album_id = ? AND image_id = ?")
        .bind(album_id)
        .bind(image_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(anyhow::Error::from)?
        .map(|row| row.0);
    if key.is_none() {
        return Err(AppError::unprocessable("REORDER_ANCHOR", "anchor is not in this album"));
    }
    Ok(key)
}

fn sort_gap_fits(after_key: Option<i64>, before_key: Option<i64>) -> bool {
    match (after_key, before_key) {
        (Some(after), Some(before)) => before > after + 1,
        (Some(after), None) => after.checked_add(SORT_GAP).is_some(),
        (None, Some(before)) => before.checked_sub(SORT_GAP).is_some(),
        (None, None) => true,
    }
}

async fn rebalance_sort_keys(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    album_id: &str,
) -> AppResult<()> {
    let ids = sqlx::query_as::<_, (String,)>(
        "SELECT image_id FROM album_images WHERE album_id = ? ORDER BY sort_key, image_id",
    )
    .bind(album_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(anyhow::Error::from)?;
    for (index, (image_id,)) in ids.iter().enumerate() {
        sqlx::query("UPDATE album_images SET sort_key = ? WHERE album_id = ? AND image_id = ?")
            .bind((index as i64 + 1) * SORT_GAP)
            .bind(album_id)
            .bind(image_id)
            .execute(&mut **tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

pub async fn move_images(
    pool: &SqlitePool,
    actor: &Actor,
    source_id: &str,
    target_id: &str,
    image_ids: &[String],
    expected_revision: Option<i64>,
    target_expected_revision: Option<i64>,
    turn_session: Option<&str>,
) -> AppResult<serde_json::Value> {
    let source = get(pool, Some(actor), source_id).await?;
    let target = get(pool, Some(actor), target_id).await?;
    if !source.capabilities.edit || !target.capabilities.edit {
        return Err(AppError::not_found());
    }
    check_rev(expected_revision, source.revision)?;
    check_rev(target_expected_revision, target.revision)?;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    if let Some(session_id) = turn_session {
        refuse_stopped_turn(&mut tx, session_id).await?;
    }
    for image_id in image_ids {
        let member: Option<(String,)> = sqlx::query_as(
            "SELECT i.id FROM album_images ai
             JOIN images i ON i.id = ai.image_id
             WHERE ai.album_id = ? AND ai.image_id = ? AND i.state = 'active'",
        )
        .bind(source_id)
        .bind(image_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        if member.is_none() {
            return Err(AppError::unprocessable(
                "MOVE_INCOMPLETE",
                "every image must be an active member of the source album",
            ));
        }
    }
    for image_id in image_ids {
        let sort: (Option<i64>,) =
            sqlx::query_as("SELECT MAX(sort_key) FROM album_images WHERE album_id = ?")
                .bind(target_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        sqlx::query(
            "INSERT OR IGNORE INTO album_images(album_id, image_id, sort_key, added_at, display_name)
             VALUES(?, ?, ?, ?, NULL)",
        )
        .bind(target_id)
        .bind(image_id)
        .bind(sort.0.unwrap_or(0) + SORT_GAP)
        .bind(db::now_ms())
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        sqlx::query("DELETE FROM album_images WHERE album_id = ? AND image_id = ?")
            .bind(source_id)
            .bind(image_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let ids_json = serde_json::to_string(image_ids).unwrap_or_else(|_| "[]".into());
    let source_updated = sqlx::query(
        "UPDATE albums SET revision = revision + 1, content_revision = content_revision + 1,
            cover_image_id = CASE WHEN cover_image_id IN (SELECT value FROM json_each(?)) THEN NULL ELSE cover_image_id END
         WHERE id = ? AND revision = ?",
    )
    .bind(&ids_json)
    .bind(source_id)
    .bind(source.revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let target_updated = sqlx::query(
        "UPDATE albums SET revision = revision + 1, content_revision = content_revision + 1
         WHERE id = ? AND revision = ?",
    )
    .bind(target_id)
    .bind(target.revision)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if source_updated.rows_affected() != 1 || target_updated.rows_affected() != 1 {
        return Err(AppError::precondition("REVISION_CONFLICT", "album revision"));
    }
    for image_id in image_ids {
        crate::search::sync_image(&mut tx, image_id)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    let _ = invalidate_archives(pool, source_id).await;
    let _ = invalidate_archives(pool, target_id).await;
    let source = get(pool, Some(actor), source_id).await?;
    let target = get(pool, Some(actor), target_id).await?;
    Ok(serde_json::json!({
        "source_revision": source.revision,
        "target_revision": target.revision,
        "image_ids": image_ids
    }))
}

pub async fn patch_access(
    pool: &SqlitePool,
    actor: &Actor,
    album_id: &str,
    body: AccessPatch,
) -> AppResult<AlbumView> {
    crate::auth::require_owner(actor)?;
    let current = get(pool, Some(actor), album_id).await?;
    check_rev(body.expected_revision, current.revision)?;
    if body.visibility.is_none() && body.archive_enabled.is_none() && body.grants.is_none() {
        return Ok(current);
    }
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let mut fields = Vec::new();
    if let Some(vis) = &body.visibility {
        sqlx::query("UPDATE albums SET visibility = ? WHERE id = ?")
            .bind(vis)
            .bind(album_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        fields.push("visibility");
    }
    if let Some(enabled) = body.archive_enabled {
        sqlx::query("UPDATE albums SET archive_enabled = ? WHERE id = ?")
            .bind(enabled as i64)
            .bind(album_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        fields.push("archive_enabled");
    }
    if let Some(grants) = body.grants {
        for g in grants {
            sqlx::query(
                "INSERT INTO album_grants(album_id, actor_id, browse, edit, upload, view_original, download)
                 VALUES(?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(album_id, actor_id) DO UPDATE SET
                    browse = excluded.browse, edit = excluded.edit, upload = excluded.upload,
                    view_original = excluded.view_original, download = excluded.download",
            )
            .bind(album_id)
            .bind(&g.actor_id)
            .bind(g.browse.unwrap_or(true) as i64)
            .bind(g.edit.unwrap_or(false) as i64)
            .bind(g.upload.unwrap_or(false) as i64)
            .bind(g.view_original.unwrap_or(false) as i64)
            .bind(g.download.unwrap_or(false) as i64)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        }
        fields.push("grants");
    }
    let revision = sqlx::query_as::<_, (i64,)>(
        "UPDATE albums SET acl_revision = acl_revision + 1, revision = revision + 1
         WHERE id = ? AND revision = ?
         RETURNING revision",
    )
    .bind(album_id)
    .bind(current.revision)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((revision,)) = revision else {
        return Err(AppError::precondition("REVISION_CONFLICT", "revision mismatch"));
    };
    let detail = serde_json::json!({
        "fields": fields,
        "revision_before": current.revision,
        "revision_after": revision
    })
    .to_string();
    sqlx::query(
        "INSERT INTO audit_log(request_id, actor_id, action, target, result, detail, occurred_at)
         VALUES(?, ?, 'album.access', ?, 'ok', ?, ?)",
    )
    .bind(crate::http_util::current_request_id())
    .bind(&actor.id)
    .bind(album_id)
    .bind(&detail)
    .bind(db::now_ms())
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    crate::events::emit_tx(
        &mut tx,
        "album.access",
        "album",
        Some(album_id),
        Some(album_id),
        Some(revision),
        Some(&actor.id),
        serde_json::json!({"fields": fields, "revision": revision}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    let _ = invalidate_archives(pool, album_id).await;
    get(pool, Some(actor), album_id).await
}

fn check_rev(expected: Option<i64>, current: i64) -> AppResult<()> {
    match expected {
        None => Err(AppError::new(
            axum::http::StatusCode::PRECONDITION_REQUIRED,
            "REVISION_REQUIRED",
            "Precondition Required",
            "expected_revision required",
        )),
        Some(v) if v != current => Err(AppError::precondition("REVISION_CONFLICT", "revision mismatch")),
        Some(_) => Ok(()),
    }
}

#[derive(Serialize)]
pub struct ImageListItem {
    pub id: String,
    pub display_name: String,
    pub width: i64,
    pub height: i64,
    pub created_at: String,
    pub variants: Vec<crate::versions::PublishedVariant>,
}

pub async fn list_images(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    album_id: &str,
    sort: &str,
    q: Option<&str>,
    limit: i64,
    cursor: Option<&str>,
) -> AppResult<serde_json::Value> {
    let album = get(pool, actor, album_id).await?;
    let limit = limit.clamp(1, 200);
    let order = match sort {
        "created_desc" => "i.created_at DESC, i.id DESC",
        "created_asc" => "i.created_at ASC, i.id ASC",
        "name_asc" => "COALESCE(ai.display_name, i.original_name) ASC, i.id ASC",
        _ => "ai.sort_key ASC, i.id ASC",
    };
    let sort_name = if ["created_desc", "created_asc", "name_asc"].contains(&sort) {
        sort
    } else {
        "manual"
    };
    let cursor_id = match cursor {
        None => None,
        Some(raw) => {
            let decoded = decode_cursor(raw).ok_or_else(|| {
                AppError::conflict("CURSOR_STALE", "cursor does not match this list")
            })?;
            if decoded.0 != album.content_revision || decoded.1 != sort_name {
                return Err(AppError::conflict(
                    "CURSOR_STALE",
                    "cursor does not match this list",
                ));
            }
            Some(decoded.2)
        }
    };
    let predicate = match sort_name {
        "created_desc" => "AND (? IS NULL OR i.created_at < (SELECT created_at FROM images WHERE id = ?) OR (i.created_at = (SELECT created_at FROM images WHERE id = ?) AND i.id < ?))",
        "created_asc" => "AND (? IS NULL OR i.created_at > (SELECT created_at FROM images WHERE id = ?) OR (i.created_at = (SELECT created_at FROM images WHERE id = ?) AND i.id > ?))",
        "name_asc" => "AND (? IS NULL OR COALESCE(ai.display_name, i.original_name) > (SELECT COALESCE(ai2.display_name, i2.original_name) FROM album_images ai2 JOIN images i2 ON i2.id = ai2.image_id WHERE ai2.album_id = ai.album_id AND i2.id = ?) OR (COALESCE(ai.display_name, i.original_name) = (SELECT COALESCE(ai2.display_name, i2.original_name) FROM album_images ai2 JOIN images i2 ON i2.id = ai2.image_id WHERE ai2.album_id = ai.album_id AND i2.id = ?) AND i.id > ?))",
        _ => "AND (? IS NULL OR ai.sort_key > (SELECT sort_key FROM album_images WHERE album_id = ai.album_id AND image_id = ?) OR (ai.sort_key = (SELECT sort_key FROM album_images WHERE album_id = ai.album_id AND image_id = ?) AND i.id > ?))",
    };
    let sql = format!(
        "SELECT i.id, COALESCE(ai.display_name, i.original_name), i.width, i.height, i.created_at
         FROM album_images ai JOIN images i ON i.id = ai.image_id
         WHERE ai.album_id = ? AND i.state = 'active'
           AND (? IS NULL OR i.original_name LIKE '%' || ? || '%' )
           {predicate}
         ORDER BY {order} LIMIT ?",
    );
    let like = q;
    let rows = sqlx::query_as::<_, (String, String, i64, i64, i64)>(&sql)
        .bind(album_id)
        .bind(like)
        .bind(like)
        .bind(cursor_id.as_deref())
        .bind(cursor_id.as_deref())
        .bind(cursor_id.as_deref())
        .bind(cursor_id.as_deref())
        .bind(limit + 1)
        .fetch_all(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let more = rows.len() as i64 > limit;
    let take = if more { limit as usize } else { rows.len() };
    let mut items = Vec::new();
    for (id, name, w, h, created) in rows.into_iter().take(take) {
        let variants = crate::versions::published_variants(pool, &id)
            .await
            .unwrap_or_default();
        items.push(ImageListItem {
            id,
            display_name: name,
            width: w,
            height: h,
            created_at: db::rfc3339(created),
            variants,
        });
    }
    let next_cursor = if more {
        items
            .last()
            .map(|i| encode_cursor(album.content_revision, sort_name, &i.id))
    } else {
        None
    };
    Ok(serde_json::json!({
        "items": items,
        "next_cursor": next_cursor,
        "content_revision": album.content_revision,
        "sort": sort_name
    }))
}

fn encode_cursor(revision: i64, sort: &str, image_id: &str) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        format!("{revision}|{sort}|{image_id}"),
    )
}

fn decode_cursor(raw: &str) -> Option<(i64, String, String)> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.splitn(3, '|');
    let rev = parts.next()?.parse().ok()?;
    let sort = parts.next()?.to_string();
    let id = parts.next()?.to_string();
    if id.is_empty() {
        return None;
    }
    Some((rev, sort, id))
}

pub async fn invalidate_archives(pool: &SqlitePool, album_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE archive_packages SET archive_state = 'stale'
         WHERE album_id = ? AND archive_state IN ('queued', 'building', 'ready')
           AND (
             content_revision != (SELECT content_revision FROM albums WHERE id = ?)
             OR acl_revision != (SELECT acl_revision FROM albums WHERE id = ?)
           )",
    )
    .bind(album_id)
    .bind(album_id)
    .bind(album_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn image_in_album(
    pool: &SqlitePool,
    album_id: &str,
    image_id: &str,
) -> Result<Option<(String, i64, i64, String, i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT i.original_name, i.width, i.height, i.original_mime, i.created_at, i.original_blob_id
         FROM album_images ai JOIN images i ON i.id = ai.image_id
         WHERE ai.album_id = ? AND i.id = ? AND i.state = 'active'",
    )
    .bind(album_id)
    .bind(image_id)
    .fetch_optional(pool)
    .await?)
}

pub const TRASH_RETAIN_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub async fn enqueue_expired_trash(pool: &SqlitePool) -> AppResult<Vec<String>> {
    let cutoff = db::now_ms() - TRASH_RETAIN_MS;
    let due = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, lifecycle_revision FROM images
         WHERE state = 'trashed' AND deleted_at IS NOT NULL AND deleted_at <= ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut queued = Vec::new();
    for (image_id, revision) in due {
        transition_lifecycle(pool, None, &image_id, revision, "purging").await?;
        let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
        let payload = crate::jobs::JobPayload {
            kind: "purge_media".into(),
            payload_version: 1,
            scope: serde_json::json!({"image_id": image_id}),
            target: serde_json::json!({}),
            actor_id: None,
        };
        let job_id = crate::jobs::enqueue(
            &mut tx,
            "purge_media",
            &format!("purge:{image_id}"),
            &payload,
            None,
            5,
        )
        .await
        .map_err(anyhow::Error::from)?;
        sqlx::query(
            "UPDATE jobs SET state = 'queued', available_at = ?, attempt_no = 0, failure_code = NULL,
                lease_token = NULL, lease_until = NULL
             WHERE id = ? AND state IN ('failed', 'canceled')",
        )
        .bind(db::now_ms())
        .bind(&job_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        tx.commit().await.map_err(anyhow::Error::from)?;
        queued.push(image_id);
    }
    Ok(queued)
}

pub async fn transition_lifecycle(
    pool: &SqlitePool,
    actor: Option<&Actor>,
    image_id: &str,
    expected: i64,
    to: &str,
) -> AppResult<(String, i64)> {
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let current = sqlx::query_as::<_, (String, i64)>(
        "SELECT state, lifecycle_revision FROM images WHERE id = ?",
    )
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if current.1 != expected {
        return Err(AppError::precondition(
            "LIFECYCLE_CONFLICT",
            "lifecycle revision mismatch",
        ));
    }
    let allowed = matches!(
        (current.0.as_str(), to),
        ("active", "trashed")
            | ("trashed", "trashed")
            | ("trashed", "active")
            | ("active", "active")
            | ("trashed", "purging")
            | ("purging", "purging")
    );
    if !allowed {
        return Err(AppError::conflict(
            "LIFECYCLE",
            "image cannot move to that state",
        ));
    }
    if current.0 == to {
        return Ok((current.0, current.1));
    }
    let now = db::now_ms();
    let next = sqlx::query_as::<_, (String, i64)>(
        "UPDATE images
         SET state = ?, lifecycle_revision = lifecycle_revision + 1, revision = revision + 1,
             deleted_at = CASE
                WHEN ? = 'trashed' OR ? = 'purging' THEN ?
                WHEN ? = 'active' THEN NULL
                ELSE deleted_at
             END
         WHERE id = ? AND lifecycle_revision = ?
         RETURNING state, lifecycle_revision",
    )
    .bind(to)
    .bind(to)
    .bind(to)
    .bind(now)
    .bind(to)
    .bind(image_id)
    .bind(expected)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(|| AppError::precondition("LIFECYCLE_CONFLICT", "lifecycle revision mismatch"))?;
    if matches!(to, "trashed" | "purging") {
        sqlx::query(
            "UPDATE albums SET cover_image_id = NULL, revision = revision + 1
             WHERE cover_image_id = ? AND deleted_at IS NULL",
        )
        .bind(image_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    }
    let detail = serde_json::json!({
        "from": current.0,
        "to": to,
        "lifecycle_before": expected,
        "lifecycle_after": next.1
    })
    .to_string();
    sqlx::query(
        "INSERT INTO audit_log(request_id, actor_id, action, target, result, detail, occurred_at)
         VALUES(?, ?, 'image.lifecycle', ?, 'ok', ?, ?)",
    )
    .bind(crate::http_util::current_request_id())
    .bind(actor.map(|person| person.id.as_str()))
    .bind(image_id)
    .bind(&detail)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    crate::events::emit_tx(
        &mut tx,
        "image.lifecycle",
        "image",
        Some(image_id),
        Some(image_id),
        Some(next.1),
        actor.map(|person| person.id.as_str()),
        serde_json::json!({"from": current.0, "to": to, "lifecycle_revision": next.1}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    crate::search::sync_image(&mut tx, image_id)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    if to == "active" {
        repair_missing_variants(pool, image_id, next.1).await?;
    }
    Ok(next)
}

async fn displayed_cover(
    pool: &SqlitePool,
    album_id: &str,
    stored: Option<String>,
) -> AppResult<Option<String>> {
    if let Some(image_id) = &stored {
        let member: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM album_images ai
             JOIN images i ON i.id = ai.image_id
             WHERE ai.album_id = ? AND ai.image_id = ? AND i.state = 'active'",
        )
        .bind(album_id)
        .bind(image_id)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if member.0 > 0 {
            return Ok(Some(image_id.clone()));
        }
    }
    let first: Option<(String,)> = sqlx::query_as(
        "SELECT i.id FROM album_images ai
         JOIN images i ON i.id = ai.image_id
         WHERE ai.album_id = ? AND i.state = 'active'
         ORDER BY ai.sort_key ASC, i.id ASC
         LIMIT 1",
    )
    .bind(album_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(first.map(|row| row.0))
}

async fn repair_missing_variants(pool: &SqlitePool, image_id: &str, lifecycle: i64) -> AppResult<()> {
    let (revision, specs) = crate::versions::current_profile(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    for spec in specs {
        let published: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM variant_heads h
             JOIN image_variants v ON v.id = h.variant_id
             WHERE h.image_id = ? AND h.spec_id = ? AND v.state = 'published'",
        )
        .bind(image_id)
        .bind(&spec.spec_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        if published.0 > 0 {
            continue;
        }
        let payload = crate::jobs::JobPayload {
            kind: "generate_variant".into(),
            payload_version: 1,
            scope: serde_json::json!({"image_id": image_id}),
            target: serde_json::json!({
                "profile_revision": revision,
                "spec_id": spec.spec_id
            }),
            actor_id: None,
        };
        let priority = if spec.spec_id == "thumb" { 100 } else { 10 };
        crate::jobs::enqueue(
            &mut tx,
            "generate_variant",
            &format!("restore_variant:{image_id}:{lifecycle}:{}", spec.spec_id),
            &payload,
            None,
            priority,
        )
        .await
        .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}
