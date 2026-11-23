use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::agent_io::AgentStdin;
use crate::auth::Actor;
use crate::error::{AppError, AppResult};
use crate::ids::new_id;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRequest {
    pub scope: serde_json::Value,
}

#[derive(Serialize)]
pub struct ScanView {
    pub id: String,
    pub state: String,
    pub status_url: String,
}

pub async fn start_scan(
    state: &crate::app::AppState,
    actor: &Actor,
    body: ScanRequest,
) -> AppResult<ScanView> {
    crate::auth::require_owner(actor)?;
    if !state.config.ai_enabled {
        return Err(AppError::gated("ai"));
    }
    let id = new_id();
    let (cutoff,): (Option<i64>,) = sqlx::query_as("SELECT MAX(created_seq) FROM images")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let mut scope = if body.scope.is_object() {
        body.scope
    } else {
        serde_json::json!({})
    };
    scope["cutoff_seq"] = serde_json::json!(cutoff.unwrap_or(0));
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO ai_scans(id, actor_id, scope_json, state, cursor_seq, created_at, updated_at)
         VALUES(?, ?, ?, 'queued', 0, ?, ?)",
    )
    .bind(&id)
    .bind(&actor.id)
    .bind(scope.to_string())
    .bind(now)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(anyhow::Error::from)?;
    dispatch_scan_page(state, &id).await.map_err(anyhow::Error::from)?;
    state.wake_jobs();
    let view = get_scan(&state.pool, actor, &id).await?;
    Ok(ScanView {
        status_url: format!("/api/v1/ai/scans/{id}"),
        id,
        state: view["state"].as_str().unwrap_or("queued").to_string(),
    })
}

pub async fn dispatch_scan_page(state: &crate::app::AppState, scan_id: &str) -> anyhow::Result<()> {
    let model = crate::vision::model_key();
    let prompt = crate::vision::PROMPT_REVISION;
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    let row = sqlx::query_as::<_, (String, String, Option<i64>, String)>(
        "SELECT state, scope_json, cursor_seq, actor_id FROM ai_scans WHERE id = ?",
    )
    .bind(scan_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((scan_state, scope_json, cursor, actor_id)) = row else {
        tx.commit().await?;
        return Ok(());
    };
    if scan_state == "succeeded" || scan_state == "canceled" {
        tx.commit().await?;
        return Ok(());
    }
    let scope: serde_json::Value = serde_json::from_str(&scope_json).unwrap_or(serde_json::json!({}));
    let cutoff = scope["cutoff_seq"].as_i64().unwrap_or(0);
    let after = cursor.unwrap_or(0);
    let ids_json = scope
        .get("image_ids")
        .filter(|v| v.is_array())
        .map(|v| v.to_string());
    let album_id = scope
        .get("album_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let rows = next_scan_images(
        &mut *tx,
        after,
        cutoff,
        ids_json.as_deref(),
        album_id.as_deref(),
        &model,
        prompt,
    )
    .await?;
    let mut last_seq = after;
    for (image_id, source_sha, seq) in &rows {
        enqueue_caption(&mut tx, &actor_id, image_id, source_sha, Some(scan_id), false).await?;
        last_seq = *seq;
    }
    let next_state = if rows.is_empty() { "succeeded" } else { "running" };
    sqlx::query(
        "UPDATE ai_scans SET state = ?, total = total + ?, cursor_seq = ?, updated_at = ? WHERE id = ?",
    )
    .bind(next_state)
    .bind(rows.len() as i64)
    .bind(last_seq)
    .bind(crate::db::now_ms())
    .bind(scan_id)
    .execute(&mut *tx)
    .await?;
    if rows.len() == 100 {
        let payload = crate::jobs::JobPayload {
            kind: "scan_plan".into(),
            payload_version: 1,
            scope: serde_json::json!({"scan_id": scan_id}),
            target: serde_json::json!({"model_key": model, "prompt_revision": prompt}),
            actor_id: Some(actor_id.clone()),
        };
        crate::jobs::enqueue(
            &mut tx,
            "scan_plan",
            &format!("scan_plan:{scan_id}:{last_seq}"),
            &payload,
            Some(&actor_id),
            2,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn next_scan_images<'e, E>(
    executor: E,
    after: i64,
    cutoff: i64,
    image_ids_json: Option<&str>,
    album_id: Option<&str>,
    model: &str,
    prompt: &str,
) -> anyhow::Result<Vec<(String, String, i64)>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query_as(
        "SELECT i.id, i.source_sha256, i.created_seq
         FROM images i
         WHERE i.state = 'active'
           AND i.created_seq > ?
           AND i.created_seq <= ?
           AND (? IS NULL OR i.id IN (SELECT value FROM json_each(?)))
           AND (? IS NULL OR EXISTS (
                SELECT 1 FROM album_images ai WHERE ai.image_id = i.id AND ai.album_id = ?
           ))
           AND NOT EXISTS (
                SELECT 1 FROM captions c
                WHERE c.image_id = i.id AND c.source = 'model'
                  AND c.model_key = ? AND c.prompt_revision = ?
                  AND c.source_sha256 = i.source_sha256
           )
         ORDER BY i.created_seq ASC, i.id ASC
         LIMIT 100",
    )
    .bind(after)
    .bind(cutoff)
    .bind(image_ids_json)
    .bind(image_ids_json)
    .bind(album_id)
    .bind(album_id)
    .bind(model)
    .bind(prompt)
    .fetch_all(executor)
    .await
    .map_err(anyhow::Error::from)
}

pub async fn enqueue_caption(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor_id: &str,
    image_id: &str,
    source_sha: &str,
    scan_id: Option<&str>,
    from_upload: bool,
) -> anyhow::Result<String> {
    let model = crate::vision::model_key();
    let prompt = crate::vision::PROMPT_REVISION;
    let dedupe = format!("caption:{image_id}:{source_sha}:{model}:{prompt}");
    let existing = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT id, state, scope_json, payload_json FROM jobs WHERE dedupe_key = ?",
    )
    .bind(&dedupe)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((id, state, scope_json, payload_json)) = existing {
        let mut scope: serde_json::Value =
            serde_json::from_str(&scope_json).unwrap_or_else(|_| serde_json::json!({}));
        attach_caption_interest(&mut scope, scan_id, from_upload);
        let mut payload: crate::jobs::JobPayload = serde_json::from_str(&payload_json).unwrap_or(crate::jobs::JobPayload {
            kind: "caption_image".into(),
            payload_version: 1,
            scope: scope.clone(),
            target: serde_json::json!({"model_key": model, "prompt_revision": prompt}),
            actor_id: Some(actor_id.to_string()),
        });
        payload.scope = scope.clone();
        let now = crate::db::now_ms();
        if matches!(state.as_str(), "failed" | "canceled" | "succeeded") {
            sqlx::query(
                "UPDATE jobs SET state = 'queued', available_at = ?, attempt_no = 0, failure_code = NULL,
                    lease_token = NULL, lease_until = NULL, updated_at = ?
                 WHERE id = ?",
            )
            .bind(now)
            .bind(now)
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query("UPDATE jobs SET scope_json = ?, payload_json = ?, updated_at = ? WHERE id = ?")
            .bind(scope.to_string())
            .bind(serde_json::to_string(&payload)?)
            .bind(now)
            .bind(&id)
            .execute(&mut **tx)
            .await?;
        return Ok(id);
    }
    let mut scope = serde_json::json!({
        "image_id": image_id,
        "source_sha256": source_sha,
        "upload": from_upload,
        "scan_ids": []
    });
    attach_caption_interest(&mut scope, scan_id, from_upload);
    let payload = crate::jobs::JobPayload {
        kind: "caption_image".into(),
        payload_version: 1,
        scope,
        target: serde_json::json!({"model_key": model, "prompt_revision": prompt}),
        actor_id: Some(actor_id.to_string()),
    };
    crate::jobs::enqueue(tx, "caption_image", &dedupe, &payload, Some(actor_id), 1).await
}

pub async fn cancel_scan(pool: &SqlitePool, actor: &Actor, scan_id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM ai_scans WHERE id = ?")
        .bind(scan_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
    if exists.is_none() {
        return Err(AppError::not_found());
    }
    cancel_scan_by_id(pool, scan_id).await.map_err(anyhow::Error::from)?;
    get_scan(pool, actor, scan_id).await
}

pub async fn cancel_scan_parent(pool: &SqlitePool, job_id: &str) -> anyhow::Result<()> {
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT kind, scope_json FROM jobs WHERE id = ?",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?;
    let Some((kind, scope_json)) = row else {
        return Ok(());
    };
    if kind != "scan_plan" {
        return Ok(());
    }
    let scope: serde_json::Value = serde_json::from_str(&scope_json).unwrap_or(serde_json::json!({}));
    let Some(scan_id) = scope["scan_id"].as_str() else {
        return Ok(());
    };
    cancel_scan_by_id(pool, scan_id).await
}

async fn cancel_scan_by_id(pool: &SqlitePool, scan_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE ai_scans SET state = 'canceled', updated_at = ?
         WHERE id = ? AND state NOT IN ('succeeded', 'canceled')",
    )
    .bind(crate::db::now_ms())
    .bind(scan_id)
    .execute(pool)
    .await?;
    let plans = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM jobs
         WHERE kind = 'scan_plan' AND json_extract(scope_json, '$.scan_id') = ?
           AND state IN ('queued', 'running', 'retry_wait')",
    )
    .bind(scan_id)
    .fetch_all(pool)
    .await?;
    for (id,) in plans {
        crate::jobs::cancel(pool, &id).await?;
    }
    let captions = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, state, scope_json FROM jobs
         WHERE kind = 'caption_image'
           AND state IN ('queued', 'running', 'retry_wait', 'cancel_requested')
           AND (
                json_extract(scope_json, '$.scan_id') = ?
                OR EXISTS (
                    SELECT 1 FROM json_each(json_extract(scope_json, '$.scan_ids'))
                    WHERE value = ?
                )
           )",
    )
    .bind(scan_id)
    .bind(scan_id)
    .fetch_all(pool)
    .await?;
    for (id, _state, scope_json) in captions {
        let mut scope: serde_json::Value =
            serde_json::from_str(&scope_json).unwrap_or_else(|_| serde_json::json!({}));
        let shared = detach_scan(&mut scope, scan_id);
        if shared {
            let payload_json: (String,) = sqlx::query_as("SELECT payload_json FROM jobs WHERE id = ?")
                .bind(&id)
                .fetch_one(pool)
                .await?;
            let mut payload: crate::jobs::JobPayload = serde_json::from_str(&payload_json.0).unwrap_or(crate::jobs::JobPayload {
                kind: "caption_image".into(),
                payload_version: 1,
                scope: scope.clone(),
                target: serde_json::json!({}),
                actor_id: None,
            });
            payload.scope = scope.clone();
            sqlx::query("UPDATE jobs SET scope_json = ?, payload_json = ?, updated_at = ? WHERE id = ?")
                .bind(scope.to_string())
                .bind(serde_json::to_string(&payload)?)
                .bind(crate::db::now_ms())
                .bind(&id)
                .execute(pool)
                .await?;
        } else {
            crate::jobs::cancel(pool, &id).await?;
        }
    }
    Ok(())
}

fn scan_ids_of(scope: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = scope["scan_id"].as_str() {
        ids.push(id.to_string());
    }
    if let Some(values) = scope["scan_ids"].as_array() {
        for value in values {
            if let Some(id) = value.as_str() {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

fn attach_caption_interest(scope: &mut serde_json::Value, scan_id: Option<&str>, from_upload: bool) {
    if from_upload {
        scope["upload"] = serde_json::json!(true);
    }
    let Some(scan_id) = scan_id else {
        return;
    };
    let mut ids = scan_ids_of(scope);
    if !ids.iter().any(|id| id == scan_id) {
        ids.push(scan_id.to_string());
    }
    scope["scan_ids"] = serde_json::json!(ids);
    if scope["scan_id"].as_str().is_none() {
        scope["scan_id"] = serde_json::json!(scan_id);
    }
}

fn detach_scan(scope: &mut serde_json::Value, scan_id: &str) -> bool {
    let mut ids = scan_ids_of(scope);
    ids.retain(|id| id != scan_id);
    scope["scan_ids"] = serde_json::json!(ids);
    scope["scan_id"] = ids
        .first()
        .cloned()
        .map(serde_json::Value::String)
        .unwrap_or(serde_json::Value::Null);
    scope["upload"].as_bool().unwrap_or(false) || !ids.is_empty()
}

pub async fn get_scan(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    crate::auth::require_owner(actor)?;
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT id, state FROM ai_scans WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    Ok(serde_json::json!({"id": row.0, "state": row.1}))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    pub scope: Option<serde_json::Value>,
}

pub async fn create_session(
    pool: &SqlitePool,
    actor: &Actor,
    ai_enabled: bool,
    body: CreateSession,
) -> AppResult<serde_json::Value> {
    if !ai_enabled {
        return Err(AppError::gated("agent"));
    }
    let id = new_id();
    let bridge_token = crate::ids::random_token(32);
    let scope = capture_scope(pool, actor, body.scope).await?;
    sqlx::query(
        "INSERT INTO agent_sessions(id, actor_id, scope_json, state, bridge_token, created_at, updated_at)
         VALUES(?, ?, ?, 'idle', ?, ?, ?)",
    )
    .bind(&id)
    .bind(&actor.id)
    .bind(scope.to_string())
    .bind(&bridge_token)
    .bind(crate::db::now_ms())
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({"id": id, "state": "idle", "revision": 1, "scope": scope}))
}

async fn capture_scope(
    pool: &SqlitePool,
    actor: &Actor,
    requested: Option<serde_json::Value>,
) -> AppResult<serde_json::Value> {
    let visible = editable_albums(pool, actor).await?;
    let albums = if let Some(ids) = requested.as_ref().and_then(|scope| scope["albums"].as_array()) {
        ids.iter()
            .filter_map(|id| id.as_str())
            .filter(|id| visible.iter().any(|album| album == id))
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        visible
    };
    Ok(serde_json::json!({"albums": albums}))
}

async fn editable_albums(pool: &SqlitePool, actor: &Actor) -> AppResult<Vec<String>> {
    let rows = if actor.role == "owner" {
        sqlx::query_as::<_, (String,)>("SELECT id FROM albums WHERE deleted_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await
    } else {
        sqlx::query_as::<_, (String,)>(
            "SELECT a.id FROM albums a
             JOIN album_grants g ON g.album_id = a.id
             WHERE a.deleted_at IS NULL AND g.actor_id = ? AND g.edit = 1
             ORDER BY a.id",
        )
        .bind(&actor.id)
        .fetch_all(pool)
        .await
    }
    .map_err(anyhow::Error::from)?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

async fn session_albums(pool: &SqlitePool, session_id: &str) -> AppResult<Option<std::collections::HashSet<String>>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT scope_json FROM agent_sessions WHERE id = ?")
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let Some((scope_json,)) = row else {
        return Ok(None);
    };
    let scope: serde_json::Value = serde_json::from_str(&scope_json).unwrap_or(serde_json::json!({}));
    let Some(albums) = scope["albums"].as_array() else {
        return Ok(None);
    };
    Ok(Some(
        albums
            .iter()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect(),
    ))
}

pub async fn list_sessions(pool: &SqlitePool, actor: &Actor) -> AppResult<serde_json::Value> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, state FROM agent_sessions WHERE actor_id = ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&actor.id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "items": rows.into_iter().map(|(id, state)| serde_json::json!({"id": id, "state": state})).collect::<Vec<_>>(),
        "next_cursor": null
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingMessage {
    pub client_message_id: String,
    pub text: String,
    pub mode: Option<String>,
    pub expected_turn_id: Option<String>,
}

pub async fn list_messages(pool: &SqlitePool, actor: &Actor, session_id: &str) -> AppResult<serde_json::Value> {
    get_session(pool, actor, session_id).await?;
    let rows = sqlx::query_as::<_, (String, i64, String, String, String)>(
        "SELECT id, seq, client_message_id, delivery_state, body FROM agent_messages
         WHERE session_id = ? ORDER BY seq",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "items": rows.into_iter().map(|(id, seq, client_message_id, delivery_state, body)| serde_json::json!({
            "id": id,
            "seq": seq,
            "client_message_id": client_message_id,
            "delivery_state": delivery_state,
            "text": body
        })).collect::<Vec<_>>(),
        "next_cursor": null
    }))
}

pub async fn post_message(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    msg: IncomingMessage,
) -> AppResult<(u16, serde_json::Value)> {
    ensure_writable(pool, actor, session_id).await?;
    if msg.text.chars().count() > 8000 || msg.client_message_id.is_empty() {
        return Err(AppError::unprocessable("MESSAGE", "invalid message"));
    }
    if let Some((id, seq, body, delivery)) = sqlx::query_as::<_, (String, i64, String, String)>(
        "SELECT id, seq, body, delivery_state FROM agent_messages WHERE session_id = ? AND client_message_id = ?",
    )
    .bind(session_id)
    .bind(&msg.client_message_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    {
        if body != msg.text {
            return Err(AppError::conflict("MESSAGE_CONFLICT", "same client id, different text"));
        }
        return Ok((
            200,
            serde_json::json!({
                "id": id,
                "seq": seq,
                "delivery_state": delivery,
                "client_message_id": msg.client_message_id
            }),
        ));
    }
    let queued: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = ? AND delivery_state IN ('queued', 'paused')",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if queued.0 >= 10 {
        return Err(AppError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "MESSAGE_BACKLOG",
            "Too Many Requests",
            "too many unsent messages",
        ));
    }
    let running = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM agent_turns WHERE session_id = ? AND state = 'running' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mode = msg.mode.as_deref().unwrap_or("steer_if_active");
    let delivery = if mode == "steer_if_active"
        && running.as_ref().map(|r| &r.0) == msg.expected_turn_id.as_ref()
        && msg.expected_turn_id.is_some()
    {
        "delivery_unknown"
    } else {
        "queued"
    };
    let seq: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM agent_messages WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let id = new_id();
    sqlx::query(
        "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(session_id)
    .bind(seq.0)
    .bind(&msg.client_message_id)
    .bind(mode)
    .bind(delivery)
    .bind(&msg.text)
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok((
        202,
        serde_json::json!({
            "id": id,
            "seq": seq.0,
            "delivery_state": delivery,
            "client_message_id": msg.client_message_id,
            "turn_id": if delivery == "delivery_unknown" { running.map(|r| r.0) } else { None }
        }),
    ))
}

pub async fn dispatch_queued_message(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
) -> AppResult<serde_json::Value> {
    if crate::config::openrouter_key().is_none() {
        return Err(AppError::gated("OpenRouter credentials"));
    }
    let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let state: String = sqlx::query_scalar(
        "SELECT state FROM agent_sessions WHERE id = ? AND actor_id = ?",
    ).bind(session_id).bind(&actor.id).fetch_optional(&mut *tx).await
        .map_err(anyhow::Error::from)?.ok_or_else(AppError::not_found)?;
    if state == "archived" || state == "needs_reconcile" {
        return Err(AppError::conflict("SESSION_UNAVAILABLE", "the session is closed or needs reconciliation"));
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_turns WHERE session_id = ?
         AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    ).bind(session_id).fetch_one(&mut *tx).await.map_err(anyhow::Error::from)?;
    if count != 0 {
        return Err(AppError::conflict("TURN_ACTIVE", "a turn is already running"));
    }
    let message: (String, String) = sqlx::query_as(
        "SELECT id, body FROM agent_messages WHERE session_id = ? AND delivery_state = 'queued' ORDER BY seq LIMIT 1",
    ).bind(session_id).fetch_optional(&mut *tx).await.map_err(anyhow::Error::from)?
        .ok_or_else(|| AppError::conflict("NO_QUEUED_MESSAGE", "there is no queued message to run"))?;
    let turn = new_id();
    let now = crate::db::now_ms();
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, state, created_at) VALUES(?, ?, 'starting', ?)",
    ).bind(&turn).bind(session_id).bind(now).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE agent_messages SET delivery_state = 'dispatching' WHERE id = ?")
        .bind(&message.0).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;

    let (pool, actor, session_id, turn_id) = (pool.clone(), actor.clone(), session_id.to_owned(), turn.clone());
    tokio::spawn(async move {
        let result = spawn_codex_turn(&pool, &actor, &session_id, &turn_id, &message.1, None).await;
        if let Err(err) = &result {
            // Only the state is sent to clients. Provider diagnostics may contain private data.
            let code = match err { AppError::Problem { code, .. } => *code, _ => "INTERNAL" };
            tracing::warn!(turn_id = %turn_id, code, "Gallery Agent turn failed");
            let _ = mark_turn(&pool, &turn_id, "failed").await;
        }
        let delivery = if result.is_ok() { "accepted" } else { "delivery_unknown" };
        let _ = sqlx::query("UPDATE agent_messages SET delivery_state = ? WHERE id = ? AND delivery_state = 'dispatching'")
            .bind(delivery).bind(&message.0).execute(&pool).await;
    });
    Ok(serde_json::json!({"id": turn, "state": "starting"}))
}

pub async fn remove_message(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    message_id: &str,
) -> AppResult<serde_json::Value> {
    get_session(pool, actor, session_id).await?;
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT delivery_state FROM agent_messages WHERE id = ? AND session_id = ?",
    )
    .bind(message_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.0 != "queued" && row.0 != "paused" {
        return Err(AppError::conflict("MESSAGE_SENT", "sent messages stay in the transcript"));
    }
    sqlx::query("UPDATE agent_messages SET delivery_state = 'removed' WHERE id = ?")
        .bind(message_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({"id": message_id, "delivery_state": "removed"}))
}

pub async fn stop_turn(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    turn_id: &str,
) -> AppResult<serde_json::Value> {
    get_session(pool, actor, session_id).await?;
    let current: Option<(String,)> = sqlx::query_as(
        "SELECT state FROM agent_turns WHERE id = ? AND session_id = ?",
    )
    .bind(turn_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((state,)) = current else {
        return Err(AppError::not_found());
    };
    if matches!(
        state.as_str(),
        "completed" | "interrupted" | "failed" | "unknown"
    ) {
        return Ok(serde_json::json!({"id": turn_id, "state": state}));
    }
    sqlx::query(
        "UPDATE agent_turns SET state = 'stop_requested', revision = revision + 1
         WHERE id = ? AND session_id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    )
    .bind(turn_id)
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE agent_messages SET delivery_state = 'paused'
         WHERE session_id = ? AND delivery_state = 'queued'",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE agent_sessions
         SET state = 'stopping', revision = revision + 1, run_epoch = run_epoch + 1, updated_at = ?
         WHERE id = ? AND state != 'archived'",
    )
    .bind(crate::db::now_ms())
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    signal_live_stop(turn_id);
    Ok(serde_json::json!({"id": turn_id, "state": "stop_requested"}))
}

pub async fn record_turn_terminal(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    turn_id: &str,
    status: &str,
) -> AppResult<serde_json::Value> {
    get_session(pool, actor, session_id).await?;
    let state = match status {
        "interrupted" => "interrupted",
        "completed" => "completed",
        "failed" => "failed",
        _ => return Err(AppError::bad_request("TURN_STATUS", "unsupported terminal status")),
    };
    let changed = sqlx::query(
        "UPDATE agent_turns SET state = ?, revision = revision + 1
         WHERE id = ? AND session_id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    )
    .bind(state)
    .bind(turn_id)
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if changed.rows_affected() == 0 {
        let current: Option<(String,)> = sqlx::query_as(
            "SELECT state FROM agent_turns WHERE id = ? AND session_id = ?",
        )
        .bind(turn_id)
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
        let Some((current,)) = current else {
            return Err(AppError::not_found());
        };
        return Ok(serde_json::json!({"id": turn_id, "state": current}));
    }
    if !newer_turn_exists(pool, turn_id).await? {
        sqlx::query(
            "UPDATE agent_sessions SET state = 'idle', revision = revision + 1, updated_at = ?
             WHERE id = ? AND state = 'stopping'",
        )
        .bind(crate::db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(serde_json::json!({"id": turn_id, "state": state}))
}

async fn newer_turn_exists(pool: &SqlitePool, turn_id: &str) -> AppResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM agent_turns newer
         JOIN agent_turns current ON current.id = ?
         WHERE newer.session_id = current.session_id
           AND newer.id != current.id
           AND newer.created_at > current.created_at",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(row.is_some())
}

pub async fn get_session(pool: &SqlitePool, actor: &Actor, id: &str) -> AppResult<serde_json::Value> {
    let row = sqlx::query_as::<_, (String, String, String, i64, String)>(
        "SELECT id, actor_id, state, revision, scope_json FROM agent_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.1 != actor.id && actor.role != "owner" {
        return Err(AppError::not_found());
    }
    let actions = sqlx::query_as::<_, (String, Option<String>, String, Option<String>, i64)>(
        "SELECT id, tool_name, state, result_ref, plan_revision FROM agent_actions
         WHERE session_id = ? AND state IN ('proposed', 'committed') ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let jobs = sqlx::query_as::<_, (String, String, String)>(
        "SELECT j.id, j.kind, j.state
         FROM agent_turn_jobs tj
         JOIN agent_turns t ON t.id = tj.turn_id
         JOIN jobs j ON j.id = tj.job_id
         WHERE t.session_id = ? AND j.state IN ('queued', 'running', 'retry_wait', 'cancel_requested')
         ORDER BY tj.submitted_at",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let turn: Option<(String, String)> = sqlx::query_as(
        "SELECT id, state FROM agent_turns WHERE session_id = ? ORDER BY created_at DESC, id DESC LIMIT 1",
    ).bind(id).fetch_optional(pool).await.map_err(anyhow::Error::from)?;
    let scope: serde_json::Value = serde_json::from_str(&row.4).map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "id": row.0,
        "state": row.2,
        "revision": row.3,
        "scope": scope,
        "turn_id": turn.as_ref().map(|turn| &turn.0),
        "turn_state": turn.as_ref().map(|turn| &turn.1),
        "actions": actions.into_iter().map(|(id, tool, state, result, revision)| serde_json::json!({
            "id": id,
            "tool_name": tool,
            "state": state,
            "revision": revision,
            "result_ref": result
        })).collect::<Vec<_>>(),
        "running_jobs": jobs.into_iter().map(|(id, kind, state)| serde_json::json!({
            "id": id,
            "kind": kind,
            "state": state
        })).collect::<Vec<_>>()
    }))
}

const BULK_MOVE: usize = 10;
const ACTION_TTL_MS: i64 = 5 * 60 * 1000;

async fn ensure_writable(pool: &SqlitePool, actor: &Actor, session_id: &str) -> AppResult<()> {
    let view = get_session(pool, actor, session_id).await?;
    if view["state"] == "archived" {
        return Err(AppError::conflict("SESSION_ARCHIVED", "session is archived"));
    }
    Ok(())
}

async fn latest_turn_state(pool: &SqlitePool, session_id: &str) -> AppResult<Option<String>> {
    let turn: Option<(String,)> = sqlx::query_as(
        "SELECT state FROM agent_turns WHERE session_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(turn.map(|(state,)| state))
}

fn turn_is_stopped(state: Option<&str>) -> bool {
    matches!(state, Some("interrupted" | "stop_requested"))
}

async fn move_or_propose(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    args: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let ids = args["image_ids"]
        .as_array()
        .ok_or_else(|| AppError::bad_request("TOOL_ARGS", "image_ids"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    if ids.len() > BULK_MOVE {
        return propose_action(pool, session_id, "album.move_images", args, ids.len()).await;
    }
    crate::album::move_images(
        pool,
        actor,
        tool_str(args, "album_id")?,
        tool_str(args, "target_album_id")?,
        &ids,
        args["expected_revision"].as_i64(),
        args["target_expected_revision"].as_i64(),
        Some(session_id),
    )
    .await
}

async fn propose_action(
    pool: &SqlitePool,
    session_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    image_count: usize,
) -> AppResult<serde_json::Value> {
    let id = new_id();
    let now = crate::db::now_ms();
    let expires_at = now + ACTION_TTL_MS;
    sqlx::query(
        "INSERT INTO agent_actions(id, session_id, plan_revision, state, tool_name, args_json, expires_at, created_at)
         VALUES(?, ?, 1, 'proposed', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(session_id)
    .bind(tool_name)
    .bind(args.to_string())
    .bind(expires_at)
    .bind(now)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "action_id": id,
        "state": "proposed",
        "plan_revision": 1,
        "expires_at": expires_at,
        "summary": {
            "tool": tool_name,
            "image_count": image_count,
            "album_id": args["album_id"],
            "target_album_id": args["target_album_id"]
        }
    }))
}

pub async fn approve_action(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    action_id: &str,
    expected_revision: i64,
) -> AppResult<serde_json::Value> {
    ensure_writable(pool, actor, session_id).await?;
    if turn_is_stopped(latest_turn_state(pool, session_id).await?.as_deref()) {
        return Err(AppError::conflict("TURN_STOPPED", "the current turn was stopped"));
    }
    let row = load_action(pool, session_id, action_id).await?;
    if row.state == "committed" {
        return Ok(stored_action_result(&row));
    }
    if row.state != "proposed" {
        return Err(AppError::conflict("ACTION_CLOSED", "action is no longer proposed"));
    }
    if row.expires_at.is_some_and(|exp| exp < crate::db::now_ms()) {
        return Err(AppError::conflict("ACTION_EXPIRED", "action plan expired"));
    }
    if row.plan_revision != expected_revision {
        return Err(AppError::precondition("ACTION_REVISION", "action revision changed"));
    }
    let args: serde_json::Value = serde_json::from_str(&row.args_json.unwrap_or_else(|| "{}".into()))
        .map_err(|_| AppError::unprocessable("ACTION_PLAN", "stored plan is unreadable"))?;
    let ids = args["image_ids"]
        .as_array()
        .ok_or_else(|| AppError::unprocessable("ACTION_PLAN", "image_ids"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let result = match row.tool_name.as_deref() {
        Some("album.move_images") => crate::album::move_images(
            pool,
            actor,
            tool_str(&args, "album_id")?,
            tool_str(&args, "target_album_id")?,
            &ids,
            args["expected_revision"].as_i64(),
            args["target_expected_revision"].as_i64(),
            Some(session_id),
        )
        .await?,
        _ => return Err(AppError::unprocessable("TOOL_UNAVAILABLE", "action has no executor")),
    };
    sqlx::query(
        "UPDATE agent_actions SET state = 'committed', result_ref = ? WHERE id = ? AND state = 'proposed'",
    )
    .bind(result.to_string())
    .bind(action_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let _ = crate::events::emit(
        pool,
        "agent.action.updated",
        "agent",
        Some(session_id),
        Some(action_id),
        Some(row.plan_revision),
        Some(&actor.id),
        serde_json::json!({"state": "committed"}),
    )
    .await;
    Ok(serde_json::json!({"action_id": action_id, "state": "committed", "result": result}))
}

pub async fn reject_action(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    action_id: &str,
    expected_revision: i64,
) -> AppResult<serde_json::Value> {
    ensure_writable(pool, actor, session_id).await?;
    let row = load_action(pool, session_id, action_id).await?;
    if row.state == "rejected" {
        return Ok(serde_json::json!({"action_id": action_id, "state": "rejected"}));
    }
    if row.state != "proposed" {
        return Err(AppError::conflict("ACTION_CLOSED", "action is no longer proposed"));
    }
    if row.plan_revision != expected_revision {
        return Err(AppError::precondition("ACTION_REVISION", "action revision changed"));
    }
    sqlx::query("UPDATE agent_actions SET state = 'rejected' WHERE id = ? AND state = 'proposed'")
        .bind(action_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({"action_id": action_id, "state": "rejected"}))
}

struct ActionRow {
    state: String,
    plan_revision: i64,
    tool_name: Option<String>,
    args_json: Option<String>,
    result_ref: Option<String>,
    expires_at: Option<i64>,
}

async fn load_action(pool: &SqlitePool, session_id: &str, action_id: &str) -> AppResult<ActionRow> {
    let row = sqlx::query_as::<_, (String, i64, Option<String>, Option<String>, Option<String>, Option<i64>)>(
        "SELECT state, plan_revision, tool_name, args_json, result_ref, expires_at
         FROM agent_actions WHERE id = ? AND session_id = ?",
    )
    .bind(action_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    Ok(ActionRow {
        state: row.0,
        plan_revision: row.1,
        tool_name: row.2,
        args_json: row.3,
        result_ref: row.4,
        expires_at: row.5,
    })
}

fn stored_action_result(row: &ActionRow) -> serde_json::Value {
    let result = row
        .result_ref
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .unwrap_or(serde_json::json!({}));
    serde_json::json!({"state": "committed", "result": result})
}

pub async fn resend_unknown(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    message_id: &str,
    client_message_id: &str,
) -> AppResult<serde_json::Value> {
    ensure_writable(pool, actor, session_id).await?;
    if client_message_id.is_empty() {
        return Err(AppError::unprocessable("MESSAGE", "client_message_id required"));
    }
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT body, delivery_state, client_message_id FROM agent_messages WHERE id = ? AND session_id = ?",
    )
    .bind(message_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(AppError::not_found)?;
    if row.1 != "delivery_unknown" {
        return Err(AppError::conflict("MESSAGE_STATE", "only an unknown delivery can be resent"));
    }
    if row.2 == client_message_id {
        return Err(AppError::conflict(
            "MESSAGE_CONFLICT",
            "resend needs a new client message id",
        ));
    }
    if let Some((id, seq, body, delivery)) = sqlx::query_as::<_, (String, i64, String, String)>(
        "SELECT id, seq, body, delivery_state FROM agent_messages WHERE session_id = ? AND client_message_id = ?",
    )
    .bind(session_id)
    .bind(client_message_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?
    {
        if body != row.0 {
            return Err(AppError::conflict("MESSAGE_CONFLICT", "same client id, different text"));
        }
        return Ok(serde_json::json!({
            "id": id,
            "seq": seq,
            "delivery_state": delivery,
            "client_message_id": client_message_id,
            "resent_from": message_id
        }));
    }
    let seq: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM agent_messages WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let id = new_id();
    sqlx::query(
        "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
         VALUES(?, ?, ?, ?, 'resend', 'queued', ?, ?)",
    )
    .bind(&id)
    .bind(session_id)
    .bind(seq.0)
    .bind(client_message_id)
    .bind(&row.0)
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "id": id,
        "seq": seq.0,
        "delivery_state": "queued",
        "client_message_id": client_message_id,
        "resent_from": message_id
    }))
}

pub async fn resume_pending(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
) -> AppResult<serde_json::Value> {
    ensure_writable(pool, actor, session_id).await?;
    let resumed = sqlx::query(
        "UPDATE agent_messages SET delivery_state = 'queued'
         WHERE session_id = ? AND delivery_state = 'paused'",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({"resumed": resumed.rows_affected()}))
}

pub async fn archive_session(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    expected_revision: Option<i64>,
) -> AppResult<serde_json::Value> {
    let view = get_session(pool, actor, session_id).await?;
    let (revision,): (i64,) = sqlx::query_as("SELECT revision FROM agent_sessions WHERE id = ?")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)?;
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(AppError::precondition("SESSION_REVISION", "session revision changed"));
    }
    if let Some(state) = latest_turn_state(pool, session_id).await? {
        if matches!(state.as_str(), "running" | "starting" | "stop_requested") {
            return Err(AppError::conflict("AGENT_BUSY", "stop the current turn before archiving"));
        }
    }
    if view["state"] != "archived" {
        sqlx::query(
            "UPDATE agent_sessions SET state = 'archived', revision = revision + 1, updated_at = ? WHERE id = ?",
        )
        .bind(crate::db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(serde_json::json!({"id": session_id, "state": "archived"}))
}

pub async fn dispatch_tool(
    state: &crate::app::AppState,
    actor: &Actor,
    session_id: &str,
    name: &str,
    args: serde_json::Value,
) -> AppResult<serde_json::Value> {
    let pool = &state.pool;
    ensure_writable(pool, actor, session_id).await?;
    let scope = session_albums(pool, session_id).await?;
    if let Some(allowed) = &scope {
        for key in ["album_id", "target_album_id"] {
            if let Some(album_id) = args[key].as_str() {
                if !allowed.contains(album_id) {
                    return Err(AppError::not_found());
                }
            }
        }
    }
    if !allowed_tools().contains(&name) {
        return Err(AppError::forbidden());
    }
    if matches!(
        name,
        "album.update"
            | "album.reorder"
            | "album.move_images"
            | "job.repair_variants"
            | "scans.request"
            | "archive.request"
    ) {
        let turn: Option<(String,)> = sqlx::query_as(
            "SELECT state FROM agent_turns WHERE session_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
        if let Some((state,)) = turn {
            if state == "interrupted" || state == "stop_requested" {
                return Err(AppError::conflict(
                    "TURN_STOPPED",
                    "the current turn was stopped",
                ));
            }
        }
        let epoch = session_run_epoch(pool, session_id).await?;
        if let Some(bound) = args.get("run_epoch").and_then(|value| value.as_i64()) {
            if bound != epoch {
                return Err(AppError::conflict(
                    "EPOCH",
                    "this bridge epoch can no longer change the gallery",
                ));
            }
        }
        if let Some(prior) = replay_committed(pool, session_id, name, &args).await? {
            return Ok(prior);
        }
    }
    let result = match name {
        "album.list" => {
            let mut listed = serde_json::to_value(crate::album::list(pool, Some(actor)).await?)
                .map_err(anyhow::Error::from)?;
            if let Some(allowed) = &scope {
                if let Some(items) = listed.as_array_mut() {
                    items.retain(|item| {
                        item["id"]
                            .as_str()
                            .is_some_and(|id| allowed.contains(id))
                    });
                }
            }
            listed
        }
        "album.read" => serde_json::to_value(
            crate::album::get(pool, Some(actor), tool_str(&args, "album_id")?).await?,
        )
        .map_err(anyhow::Error::from)?,
        "album.update" => serde_json::to_value(
            crate::album::patch(
                pool,
                actor,
                tool_str(&args, "album_id")?,
                crate::album::PatchAlbum {
                    name: args["name"].as_str().map(str::to_string),
                    description: args["description"].as_str().map(str::to_string),
                    cover_image_id: args["cover_image_id"].as_str().map(str::to_string),
                    expected_revision: args["expected_revision"].as_i64(),
                },
                Some(session_id),
            )
            .await?,
        )
        .map_err(anyhow::Error::from)?,
        "album.reorder" => crate::album::reorder(
            pool,
            actor,
            tool_str(&args, "album_id")?,
            tool_str(&args, "image_id")?,
            args["before_id"].as_str(),
            args["after_id"].as_str(),
            args["expected_revision"].as_i64(),
            Some(session_id),
        )
        .await?,
        "album.move_images" => move_or_propose(pool, actor, session_id, &args).await?,
        "image.list" => crate::album::list_images(
            pool,
            Some(actor),
            tool_str(&args, "album_id")?,
            args["sort"].as_str().unwrap_or("manual"),
            args["q"].as_str(),
            args["limit"].as_i64().unwrap_or(50),
            args["cursor"].as_str(),
        )
        .await?,
        "job.status" => {
            let job = crate::jobs::get(pool, tool_str(&args, "job_id")?)
                .await
                .map_err(anyhow::Error::from)?
                .ok_or_else(AppError::not_found)?;
            serde_json::to_value(job).map_err(anyhow::Error::from)?
        }
        "job.audit_variants" => serde_json::to_value(
            crate::versions::published_variants(pool, tool_str(&args, "image_id")?)
                .await
                .map_err(anyhow::Error::from)?,
        )
        .map_err(anyhow::Error::from)?,
        "job.repair_variants" => {
            let image_id = tool_str(&args, "image_id")?;
            let mut tx = crate::db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
            crate::album::refuse_stopped_turn(&mut tx, session_id).await?;
            let jobs = crate::versions::plan_variants(&mut tx, image_id, Some(&actor.id), None)
                .await
                .map_err(anyhow::Error::from)?;
            tx.commit().await.map_err(anyhow::Error::from)?;
            serde_json::json!({"job_ids": jobs})
        }
        "upload.status" => {
            crate::upload::get_batch(pool, actor, tool_str(&args, "batch_id")?).await?
        }
        "download.status" => {
            crate::download::archive_status(pool, actor, tool_str(&args, "archive_id")?).await?
        }
        "scans.request" => {
            let scope = args.get("scope").cloned().unwrap_or(args.clone());
            let view = start_scan(state, actor, ScanRequest { scope }).await?;
            let jobs = sqlx::query_as::<_, (String,)>(
                "SELECT id FROM jobs WHERE json_extract(scope_json, '$.scan_id') = ?",
            )
            .bind(&view.id)
            .fetch_all(pool)
            .await
            .map_err(anyhow::Error::from)?;
            let job_ids: Vec<String> = jobs.into_iter().map(|(id,)| id).collect();
            link_turn_jobs(pool, session_id, &job_ids).await?;
            serde_json::json!({
                "scan_id": view.id,
                "state": view.state,
                "status_url": view.status_url,
                "job_ids": job_ids
            })
        }
        "archive.request" => {
            let (_status, body) =
                crate::download::request_archive(state, actor, tool_str(&args, "album_id")?).await?;
            if let Some(job_id) = body["job_id"].as_str() {
                link_turn_jobs(pool, session_id, &[job_id.to_string()]).await?;
            }
            body
        }
        "storage.check" => {
            let row = sqlx::query_as::<_, (String, i64, i64)>(
                "SELECT b.kind, r.write_barrier, r.routing_epoch
                 FROM storage_route r JOIN storage_backends b ON b.id = r.active_backend_id
                 WHERE r.id = 1",
            )
            .fetch_one(pool)
            .await
            .map_err(anyhow::Error::from)?;
            serde_json::json!({
                "kind": row.0,
                "write_barrier": row.1 == 1,
                "routing_epoch": row.2
            })
        }
        other => {
            return Err(AppError::unprocessable(
                "TOOL_UNAVAILABLE",
                format!("{other} is not wired to a local executor"),
            ))
        }
    };
    sqlx::query(
        "INSERT INTO agent_actions(id, session_id, plan_revision, state, tool_name, args_json, result_ref, created_at)
         VALUES(?, ?, 1, 'committed', ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(session_id)
    .bind(name)
    .bind(args.to_string())
    .bind(result.to_string())
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(result)
}

async fn replay_committed(
    pool: &SqlitePool,
    session_id: &str,
    name: &str,
    args: &serde_json::Value,
) -> AppResult<Option<serde_json::Value>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT result_ref FROM agent_actions
         WHERE session_id = ? AND tool_name = ? AND state = 'committed' AND args_json = ?
           AND result_ref IS NOT NULL
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .bind(name)
    .bind(args.to_string())
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(row.and_then(|row| serde_json::from_str(&row.0).ok()))
}

fn prepare_codex_home(home: &std::path::Path, mcp: &str) -> AppResult<()> {
    std::fs::create_dir_all(home).map_err(anyhow::Error::from)?;
    let model = std::env::var("GALLERY_CODEX_MODEL")
        .unwrap_or_else(|_| "deepseek/deepseek-v4.1-flash".into());
    let mut config = format!(
        r#"model_provider = "openrouter"
model = "{model}"
model_reasoning_effort = "low"
suppress_unstable_features_warning = true

[model_providers.openrouter]
name = "openrouter"
base_url = "https://openrouter.ai/api/v1"
wire_api = "responses"
env_key = "OPENROUTER_API_KEY"
"#
    );
    config.push_str(mcp);
    std::fs::write(home.join("config.toml"), config).map_err(anyhow::Error::from)?;
    Ok(())
}

async fn codex_mcp_config(pool: &SqlitePool, session_id: &str) -> String {
    let Some(bin) = gallery_bin() else {
        return String::new();
    };
    let socket = crate::db::kv_get(pool, "mcp_socket").await.ok().flatten();
    let row = sqlx::query_as::<_, (Option<String>, i64)>(
        "SELECT bridge_token, run_epoch FROM agent_sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let (Some(socket), Some((Some(token), epoch))) = (socket, row) else {
        return String::new();
    };
    format!(
        "\n[mcp_servers.gallery]\ncommand = \"{}\"\nargs = [\"mcp\"]\n\n[mcp_servers.gallery.env]\nGALLERY_MCP_SOCKET = \"{}\"\nGALLERY_MCP_SESSION = \"{}\"\nGALLERY_MCP_TOKEN = \"{}\"\nGALLERY_MCP_EPOCH = \"{}\"\n",
        toml_basic(&bin.to_string_lossy()),
        toml_basic(&socket),
        toml_basic(session_id),
        toml_basic(&token),
        epoch
    )
}

async fn session_run_epoch(pool: &SqlitePool, session_id: &str) -> AppResult<i64> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT run_epoch FROM agent_sessions WHERE id = ?")
        .bind(session_id)
        .fetch_optional(pool)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(row.map(|row| row.0).unwrap_or(1))
}

fn gallery_bin() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("GALLERY_BIN") {
        return Some(std::path::PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    (exe.file_stem()?.to_str()? == "gallery").then_some(exe)
}

fn toml_basic(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn openrouter_key_from_file() -> Option<String> {
    crate::config::openrouter_key()
}

fn upstream_timeout() -> AppError {
    AppError::new(
        axum::http::StatusCode::GATEWAY_TIMEOUT,
        "UPSTREAM_TIMEOUT",
        "Gateway Timeout",
        "agent upstream timed out",
    )
}

async fn recv_within(
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    budget: std::time::Duration,
) -> AppResult<String> {
    match tokio::time::timeout(budget, lines.recv()).await {
        Ok(Some(line)) => Ok(line),
        Ok(None) => Err(AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        )),
        Err(_) => Err(upstream_timeout()),
    }
}

async fn read_response(
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    id: i64,
    budget: std::time::Duration,
) -> AppResult<serde_json::Value> {
    let deadline = std::time::Instant::now() + budget;
    for _ in 0..40 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(upstream_timeout());
        }
        let line = recv_within(lines, left).await?;
        let msg: serde_json::Value = serde_json::from_str(line.trim()).map_err(|_| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "upstream line was not json")
        })?;
        if msg.get("id").and_then(|value| value.as_i64()) == Some(id) {
            if let Some(error) = msg.get("error") {
                return Err(AppError::unprocessable(
                    "UPSTREAM_PROTOCOL",
                    error["message"].as_str().unwrap_or("app-server request failed"),
                ));
            }
            return Ok(msg);
        }
    }
    Err(AppError::unprocessable(
        "UPSTREAM_PROTOCOL",
        "app-server response id was not seen",
    ))
}

struct LiveSlot {
    generation: u64,
    sender: tokio::sync::mpsc::Sender<()>,
}

fn live_turns() -> &'static std::sync::Mutex<std::collections::HashMap<String, LiveSlot>> {
    static LIVE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, LiveSlot>>,
    > = std::sync::OnceLock::new();
    LIVE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

struct LiveStop {
    id: String,
    generation: u64,
    receiver: tokio::sync::mpsc::Receiver<()>,
}

impl Drop for LiveStop {
    fn drop(&mut self) {
        if let Ok(mut live) = live_turns().lock() {
            if live.get(&self.id).map(|slot| slot.generation) == Some(self.generation) {
                live.remove(&self.id);
            }
        }
    }
}

fn watch_live_stop(turn_id: &str) -> LiveStop {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    if let Ok(mut live) = live_turns().lock() {
        live.insert(turn_id.to_string(), LiveSlot { generation, sender });
    }
    LiveStop {
        id: turn_id.to_string(),
        generation,
        receiver,
    }
}

fn signal_live_stop(turn_id: &str) {
    if let Ok(live) = live_turns().lock() {
        if let Some(slot) = live.get(turn_id) {
            let _ = slot.sender.try_send(());
        }
    }
}

const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
pub const HANDSHAKE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
pub const CONTROL_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
pub const TURN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10 * 60);
pub const REPLY_COALESCE: std::time::Duration = std::time::Duration::from_millis(100);

pub struct ReplyCoalescer {
    dirty: bool,
    last_flush: std::time::Instant,
}

impl ReplyCoalescer {
    pub fn new() -> Self {
        Self {
            dirty: false,
            last_flush: std::time::Instant::now(),
        }
    }

    pub fn mark_dirty(&mut self, now: std::time::Instant) {
        if !self.dirty {
            self.last_flush = now;
        }
        self.dirty = true;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn due(&self, now: std::time::Instant) -> bool {
        self.dirty && now.saturating_duration_since(self.last_flush) >= REPLY_COALESCE
    }

    pub fn remaining(&self, now: std::time::Instant) -> std::time::Duration {
        REPLY_COALESCE.saturating_sub(now.saturating_duration_since(self.last_flush))
    }

    pub fn mark_flushed(&mut self, now: std::time::Instant) {
        self.dirty = false;
        self.last_flush = now;
    }
}

pub async fn drive_live_turn<W>(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    thread_id: &str,
    text: &str,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    writer: &mut W,
    grace: std::time::Duration,
) -> AppResult<String>
where
    W: crate::agent_io::AgentStdin + Unpin,
{
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at)
         VALUES(?, ?, 1, 'running', ?)
         ON CONFLICT(id) DO UPDATE SET state = CASE
            WHEN agent_turns.state = 'stop_requested' THEN 'stop_requested'
            ELSE 'running'
         END",
    )
    .bind(turn_id)
    .bind(session_id)
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut live = watch_live_stop(turn_id);
    let mut stop_armed = turn_stop_requested(pool, turn_id).await?;
    if stop_armed {
        write_turn_interrupt(writer, thread_id, turn_id).await?;
    }
    writer
        .write_normal(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": text}],
                "approvalPolicy": "never",
                "sandbox": "read-only"
            }
        }))
        .await?;
    let mut reply = String::new();
    let mut coalescer = ReplyCoalescer::new();
    let mut terminal = Some("completed");
    let started = std::time::Instant::now();
    let mut grace_deadline = stop_armed.then(|| std::time::Instant::now() + grace);
    loop {
        if started.elapsed() > TURN_BUDGET {
            mark_turn(pool, turn_id, "failed").await?;
            return Err(upstream_timeout());
        }
        if grace_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            mark_turn(pool, turn_id, "unknown").await?;
            return Err(AppError::conflict("NEEDS_RECONCILE", "stop grace elapsed"));
        }
        let wait = grace_deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(TURN_BUDGET.saturating_sub(started.elapsed()));
        enum Incoming {
            Stop,
            Line(Option<String>),
            Flush,
            Poll,
            Tick,
        }
        let flush_wait = coalescer.remaining(std::time::Instant::now());
        let flush_due = coalescer.dirty();
        let incoming = tokio::select! {
            biased;
            _ = live.receiver.recv(), if !stop_armed => Incoming::Stop,
            incoming = lines.recv() => Incoming::Line(incoming),
            _ = tokio::time::sleep(flush_wait), if flush_due => Incoming::Flush,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)), if !stop_armed => Incoming::Poll,
            _ = tokio::time::sleep(wait) => Incoming::Tick,
        };
        let line = match incoming {
            Incoming::Flush => {
                coalescer.mark_flushed(std::time::Instant::now());
                if !reply.is_empty() {
                    let _ = emit_reply_progress(pool, session_id, turn_id, &reply).await;
                }
                continue;
            }
            Incoming::Stop => {
                stop_armed = true;
                write_turn_interrupt(writer, thread_id, turn_id).await?;
                grace_deadline = Some(std::time::Instant::now() + grace);
                continue;
            }
            Incoming::Poll => {
                if turn_stop_requested(pool, turn_id).await? {
                    stop_armed = true;
                    write_turn_interrupt(writer, thread_id, turn_id).await?;
                    grace_deadline = Some(std::time::Instant::now() + grace);
                }
                continue;
            }
            Incoming::Tick => {
                if grace_deadline.is_some() {
                    mark_turn(pool, turn_id, "unknown").await?;
                    return Err(AppError::conflict("NEEDS_RECONCILE", "stop grace elapsed"));
                }
                mark_turn(pool, turn_id, "failed").await?;
                return Err(upstream_timeout());
            }
            Incoming::Line(None) => {
                return Err(AppError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "UPSTREAM_CLOSED",
                    "Bad Gateway",
                    "agent upstream closed",
                ));
            }
            Incoming::Line(Some(line)) => line,
        };
        let msg: serde_json::Value = serde_json::from_str(line.trim()).map_err(|_| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "upstream line was not json")
        })?;
        if msg.get("id").and_then(|value| value.as_i64()) == Some(3) {
            if let Some(error) = msg.get("error") {
                return Err(AppError::unprocessable(
                    "UPSTREAM_TURN",
                    error["message"].as_str().unwrap_or("turn/start failed"),
                ));
            }
        }
        match msg["method"].as_str() {
            Some("item/agentMessage/delta") => {
                if let Some(delta) = msg["params"]["delta"].as_str() {
                    if !delta.is_empty() {
                        reply.push_str(delta);
                        coalescer.mark_dirty(std::time::Instant::now());
                    }
                }
            }
            Some("item/completed") => {
                let item = &msg["params"]["item"];
                if item["type"].as_str() == Some("agentMessage") {
                    if let Some(text) = item["text"].as_str() {
                        reply = text.to_string();
                    }
                }
            }
            Some("turn/completed") => {
                if reply.is_empty() {
                    if let Some(text) = msg["params"]["text"].as_str() {
                        reply = text.to_string();
                    }
                }
                let status = msg["params"]["turn"]["status"].as_str().unwrap_or("completed");
                if status == "failed" {
                    mark_turn(pool, turn_id, "failed").await?;
                    let message = msg["params"]["turn"]["error"]["message"]
                        .as_str()
                        .unwrap_or("turn failed");
                    return Err(AppError::unprocessable("UPSTREAM_TURN", message));
                }
                if status == "interrupted" {
                    terminal = Some("interrupted");
                }
                break;
            }
            _ => {}
        }
    }
    if terminal == Some("interrupted") {
        commit_turn_reply(pool, session_id, turn_id, &reply, "interrupted").await?;
        return Ok(reply);
    }
    if reply.is_empty() {
        reply = "completed".into();
    }
    commit_turn_reply(pool, session_id, turn_id, &reply, "completed").await?;
    Ok(reply)
}

async fn emit_reply_progress(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    text: &str,
) -> AppResult<()> {
    let mut tx = crate::db::begin_immediate(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let actor_id = session_actor(&mut tx, session_id).await?;
    let revision = next_reply_revision(&mut tx, turn_id).await?;
    crate::events::emit_tx(
        &mut tx,
        "agent.reply",
        "agent",
        Some(session_id),
        Some(turn_id),
        Some(revision),
        actor_id.as_deref(),
        serde_json::json!({"turn_id": turn_id, "partial": true, "text": text}),
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}

async fn commit_turn_reply(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    body: &str,
    state: &str,
) -> AppResult<()> {
    let mut tx = crate::db::begin_immediate(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let actor_id = session_actor(&mut tx, session_id).await?;
    let mut message_id = None;
    if !body.is_empty() {
        let seq: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM agent_messages WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let id = new_id();
        sqlx::query(
            "INSERT INTO agent_messages(id, session_id, seq, client_message_id, mode, delivery_state, body, created_at)
             VALUES(?, ?, ?, ?, 'assistant', 'accepted', ?, ?)",
        )
        .bind(&id)
        .bind(session_id)
        .bind(seq.0)
        .bind(format!("assistant:{turn_id}"))
        .bind(body)
        .bind(crate::db::now_ms())
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        message_id = Some(id);
    }
    let changed = sqlx::query(
        "UPDATE agent_turns SET state = ?, revision = revision + 1
         WHERE id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    )
    .bind(state)
    .bind(turn_id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let newer: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM agent_turns newer
         JOIN agent_turns current ON current.id = ?
         WHERE newer.session_id = current.session_id
           AND newer.id != current.id
           AND newer.created_at > current.created_at",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if changed.rows_affected() > 0 && newer.is_none() {
        if state == "unknown" {
            sqlx::query(
                "UPDATE agent_sessions SET state = 'needs_reconcile', revision = revision + 1, updated_at = ?
                 WHERE id = ? AND state != 'archived'",
            )
            .bind(crate::db::now_ms())
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        } else if matches!(state, "completed" | "interrupted" | "failed") {
            sqlx::query(
                "UPDATE agent_sessions SET state = 'idle', revision = revision + 1, updated_at = ?
                 WHERE id = ? AND state = 'stopping'",
            )
            .bind(crate::db::now_ms())
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        }
    }
    let revision = next_reply_revision(&mut tx, turn_id).await?;
    crate::events::emit_tx(
        &mut tx,
        "agent.reply",
        "agent",
        Some(session_id),
        Some(turn_id),
        Some(revision),
        actor_id.as_deref(),
        serde_json::json!({
            "turn_id": turn_id,
            "message_id": message_id,
            "state": state,
            "partial": false,
            "text": body
        }),
    )
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}

async fn session_actor(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &str,
) -> AppResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT actor_id FROM agent_sessions WHERE id = ?")
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(row.map(|row| row.0))
}

async fn next_reply_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    turn_id: &str,
) -> AppResult<i64> {
    let (revision,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(revision), 0) + 1 FROM events
         WHERE type = 'agent.reply' AND resource_id = ?",
    )
    .bind(turn_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(revision)
}

async fn run_codex_turn<W>(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    thread_id: &str,
    text: &str,
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    writer: &mut W,
) -> AppResult<String>
where
    W: crate::agent_io::AgentStdin + Unpin,
{
    drive_live_turn(pool, session_id, turn_id, thread_id, text, lines, writer, STOP_GRACE).await
}

async fn turn_stop_requested(pool: &SqlitePool, turn_id: &str) -> AppResult<bool> {
    let state: Option<(String,)> =
        sqlx::query_as("SELECT state FROM agent_turns WHERE id = ?")
            .bind(turn_id)
            .fetch_optional(pool)
            .await
            .map_err(anyhow::Error::from)?;
    Ok(state.is_some_and(|row| row.0 == "stop_requested"))
}

async fn write_turn_interrupt<W>(writer: &mut W, thread_id: &str, turn_id: &str) -> AppResult<()>
where
    W: crate::agent_io::AgentStdin + Unpin,
{
    writer
        .write_control(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "turn/interrupt",
            "params": {"threadId": thread_id, "turnId": turn_id}
        }))
        .await
}

fn tool_str<'a>(args: &'a serde_json::Value, key: &str) -> AppResult<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| AppError::bad_request("TOOL_ARGS", key))
}

/// MCP tool names allowed for Agent. Live Codex/DeepSeek loops stay out of this executor.
pub fn codex_launch(bin: Option<&std::path::Path>) -> (std::path::PathBuf, Vec<String>) {
    let program = bin
        .map(|path| path.to_path_buf())
        .or_else(|| std::env::var("GALLERY_CODEX_BIN").ok().map(std::path::PathBuf::from))
        .or_else(|| {
            let local = std::path::PathBuf::from(".tools/codex");
            local.is_file().then_some(local)
        })
        .unwrap_or_else(|| std::path::PathBuf::from("codex"));
    (program, vec!["app-server".into()])
}

struct LiveCodex {
    child: tokio::process::Child,
    gate: crate::agent_io::StdinGate,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    thread_id: String,
    session_id: String,
    db: SqlitePool,
    run_epoch: i64,
}

struct CodexPool {
    slots: crate::agent_io::CodexSlots<LiveCodex>,
}

struct ActiveTurn {
    session_id: String,
    done: bool,
    resumed: Option<LiveCodex>,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        if !self.done {
            if let Ok(mut pool) = codex_pool().lock() {
                pool.slots.abort(&self.session_id);
            }
        }
    }
}

fn codex_pool() -> &'static std::sync::Mutex<CodexPool> {
    static POOL: std::sync::OnceLock<std::sync::Mutex<CodexPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        std::sync::Mutex::new(CodexPool {
            slots: crate::agent_io::CodexSlots::new(
                crate::agent_io::CODEX_IDLE_LIMIT,
                crate::agent_io::CODEX_IDLE_TTL,
            ),
        })
    })
}

pub fn codex_idle_sessions() -> Vec<String> {
    codex_pool()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .slots
        .idle_sessions()
}

pub async fn reset_codex_processes() {
    let released = {
        let mut pool = codex_pool().lock().unwrap_or_else(|err| err.into_inner());
        pool.slots.take_all()
    };
    stop_lives(released).await;
}

async fn stop_lives(lives: Vec<LiveCodex>) {
    for live in lives {
        stop_live(live).await;
    }
}

async fn stop_live(mut live: LiveCodex) {
    let _ = live.child.kill().await;
    drop(live.gate);
    let _ = live.child.wait().await;
    let _ = close_bridge(&live.db, &live.session_id).await;
}

async fn close_bridge(pool: &SqlitePool, session_id: &str) -> AppResult<()> {
    let mut tx = crate::db::begin_immediate(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let now = crate::db::now_ms();
    sqlx::query("UPDATE agent_sessions SET bridge_token = NULL, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE agent_messages SET delivery_state = 'delivery_unknown'
         WHERE session_id = ? AND delivery_state = 'queued'",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE agent_turns SET state = 'unknown', revision = revision + 1
         WHERE session_id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}

async fn admit_codex(session_id: &str) -> AppResult<ActiveTurn> {
    let admission = {
        let mut pool = codex_pool().lock().unwrap_or_else(|err| err.into_inner());
        pool.slots.begin(session_id, std::time::Instant::now())
    };
    match admission {
        Ok(admission) => {
            let resumed = admission.resumed;
            stop_lives(admission.released).await;
            Ok(ActiveTurn {
                session_id: session_id.to_string(),
                done: false,
                resumed,
            })
        }
        Err((kind, released)) => {
            stop_lives(released).await;
            Err(match kind {
                crate::agent_io::SlotError::SessionBusy => {
                    AppError::conflict("SESSION_BUSY", "this session already has an active turn")
                }
                crate::agent_io::SlotError::InstanceBusy => {
                    AppError::conflict("INSTANCE_BUSY", "another agent turn is active")
                }
            })
        }
    }
}

async fn park_codex(active: &mut ActiveTurn, mut live: LiveCodex) {
    let running = live.child.try_wait().ok().flatten().is_none();
    if running {
        let released = {
            let mut pool = codex_pool().lock().unwrap_or_else(|err| err.into_inner());
            pool.slots
                .park(&active.session_id, live, std::time::Instant::now())
        };
        active.done = true;
        stop_lives(released).await;
    } else {
        stop_live(live).await;
    }
}

pub async fn spawn_codex_turn(
    pool: &SqlitePool,
    _actor: &Actor,
    session_id: &str,
    turn_id: &str,
    text: &str,
    bin: Option<&std::path::Path>,
) -> AppResult<String> {
    let mut active = admit_codex(session_id).await?;
    if let Some(mut live) = active.resumed.take() {
        let current_epoch = session_run_epoch(pool, session_id).await?;
        if live.run_epoch == current_epoch && live.child.try_wait().ok().flatten().is_none() {
            let result = run_codex_turn(
                pool,
                session_id,
                turn_id,
                &live.thread_id,
                text,
                &mut live.lines,
                &mut live.gate,
            )
            .await;
            return match result {
                Ok(text) => {
                    park_codex(&mut active, live).await;
                    Ok(text)
                }
                Err(err) => {
                    stop_live(live).await;
                    Err(err)
                }
            };
        }
        stop_live(live).await;
    }
    let (program, args) = codex_launch(bin);
    let codex_home = std::env::var("GALLERY_CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(".codex-home"))
        .join(session_id);
    let mcp = codex_mcp_config(pool, session_id).await;
    prepare_codex_home(&codex_home, &mcp)?;
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("CODEX_HOME", &codex_home)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("GALLERY_OPENROUTER_KEY");
    if let Some(key) = openrouter_key_from_file() {
        command.env("OPENROUTER_API_KEY", key);
    }
    let mut child = command
        .spawn()
        .map_err(|_| {
            AppError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "CODEX_UNAVAILABLE",
                "Service Unavailable",
                "codex app-server is not installed",
            )
        })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        )
    })?;
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut stdout = tokio::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let stdin = child.stdin.take().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        )
    })?;
    if let Some(stderr) = child.stderr.take() {
        crate::agent_io::pump_stderr(stderr);
    }
    let mut stdin = crate::agent_io::StdinGate::spawn(stdin);
    stdin
        .write_normal(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "gallery", "title": "Gallery", "version": env!("CARGO_PKG_VERSION")}}
        }))
        .await?;
    let hello = read_response(&mut lines, 1, HANDSHAKE_BUDGET).await?;
    if hello.get("result").is_none() {
        let _ = child.kill().await;
        return Err(AppError::unprocessable(
            "UPSTREAM_PROTOCOL",
            "app-server did not complete initialize",
        ));
    }
    stdin
        .write_normal(&serde_json::json!({"jsonrpc": "2.0", "method": "initialized"}))
        .await?;
    let model = std::env::var("GALLERY_CODEX_MODEL")
        .unwrap_or_else(|_| "deepseek/deepseek-v4.1-flash".into());
    stdin
        .write_normal(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": {
                "model": model,
                "cwd": std::env::var("GALLERY_CODEX_CWD").unwrap_or_else(|_| {
                    std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        .display()
                        .to_string()
                }),
                "approvalPolicy": "never",
                "sandbox": "read-only"
            }
        }))
        .await?;
    let thread_msg = read_response(&mut lines, 2, HANDSHAKE_BUDGET).await?;
    let thread_id = thread_msg["result"]["thread"]["id"]
        .as_str()
        .or_else(|| thread_msg["result"]["threadId"].as_str())
        .ok_or_else(|| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "thread/start did not return a thread id")
        })?
        .to_string();
    sqlx::query("UPDATE agent_sessions SET upstream_thread_id = ?, updated_at = ? WHERE id = ?")
        .bind(&thread_id)
        .bind(crate::db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    let result = run_codex_turn(pool, session_id, turn_id, &thread_id, text, &mut lines, &mut stdin).await;
    let live = LiveCodex {
        child,
        gate: stdin,
        lines,
        thread_id,
        session_id: session_id.to_string(),
        db: pool.clone(),
        run_epoch: session_run_epoch(pool, session_id).await?,
    };
    match result {
        Ok(text) => {
            park_codex(&mut active, live).await;
            Ok(text)
        }
        Err(err) => {
            stop_live(live).await;
            Err(err)
        }
    }
}

pub async fn spawn_and_drive(
    state: &crate::app::AppState,
    actor: &Actor,
    session_id: &str,
    turn_id: &str,
    text: &str,
    program: &std::path::Path,
) -> AppResult<String> {
    let mut child = tokio::process::Command::new(program)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("GALLERY_OPENROUTER_KEY")
        .spawn()
        .map_err(anyhow::Error::from)?;
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        )
    })?);
    if let Some(stderr) = child.stderr.take() {
        crate::agent_io::pump_stderr(stderr);
    }
    let mut stdin = child.stdin.take().ok_or_else(|| {
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        )
    })?;
    let result = drive_turn(state, actor, session_id, turn_id, text, &mut stdout, &mut stdin).await;
    let _ = child.wait().await;
    result
}

pub async fn drive_turn<R, W>(
    state: &crate::app::AppState,
    actor: &Actor,
    session_id: &str,
    turn_id: &str,
    text: &str,
    reader: &mut R,
    writer: &mut W,
) -> AppResult<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let pool = &state.pool;
    get_session(pool, actor, session_id).await?;
    sqlx::query(
        "INSERT INTO agent_turns(id, session_id, generation, state, created_at)
         VALUES(?, ?, 1, 'running', ?)
         ON CONFLICT(id) DO UPDATE SET state = 'running'",
    )
    .bind(turn_id)
    .bind(session_id)
    .bind(crate::db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let (thread_id,): (Option<String>,) =
        sqlx::query_as("SELECT upstream_thread_id FROM agent_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(anyhow::Error::from)?;
    let start = serde_json::json!({
        "method": "turn/start",
        "params": {"session_id": session_id, "turn_id": turn_id, "thread_id": thread_id, "text": text}
    });
    write_line(writer, &start).await?;
    let mut completed = None;
    for _ in 0..8 {
        let line = match read_line(reader).await {
            Ok(line) => line,
            Err(_) => {
                mark_turn(pool, turn_id, "needs_reconcile").await?;
                return Err(AppError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "UPSTREAM_CLOSED",
                    "Bad Gateway",
                    "agent upstream closed",
                ));
            }
        };
        let msg: serde_json::Value = serde_json::from_str(&line).map_err(|_| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "upstream line was not json")
        })?;
        match msg["method"].as_str() {
            Some("tool") => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"]["arguments"].clone();
                let result = match dispatch_tool(state, actor, session_id, name, args).await {
                    Ok(value) => serde_json::json!({"ok": true, "value": value}),
                    Err(err) => serde_json::json!({"ok": false, "error": err.to_string()}),
                };
                write_line(
                    writer,
                    &serde_json::json!({
                        "method": "turn/toolResult",
                        "params": {"name": name, "result": result}
                    }),
                )
                .await?;
            }
            Some("completed") | Some("turn/completed") => {
                completed = msg["params"]["text"]
                    .as_str()
                    .or_else(|| msg["params"]["message"].as_str())
                    .map(str::to_string);
                break;
            }
            _ => {
                mark_turn(pool, turn_id, "needs_reconcile").await?;
                return Err(AppError::unprocessable("UPSTREAM_PROTOCOL", "unexpected upstream method"));
            }
        }
    }
    let text = completed.ok_or_else(|| {
        AppError::unprocessable("UPSTREAM_PROTOCOL", "turn ended without completion")
    })?;
    commit_turn_reply(pool, session_id, turn_id, &text, "completed").await?;
    Ok(text)
}

async fn write_line<S>(stream: &mut S, value: &serde_json::Value) -> AppResult<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_vec(value).map_err(anyhow::Error::from)?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(anyhow::Error::from)?;
    stream.flush().await.map_err(anyhow::Error::from)?;
    Ok(())
}

async fn read_line<S>(stream: &mut S) -> AppResult<String>
where
    S: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    let n = stream.read_line(&mut line).await.map_err(anyhow::Error::from)?;
    if n == 0 {
        return Err(AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "UPSTREAM_CLOSED",
            "Bad Gateway",
            "agent upstream closed",
        ));
    }
    Ok(line)
}

async fn mark_turn(pool: &SqlitePool, turn_id: &str, state: &str) -> AppResult<()> {
    let changed = sqlx::query(
        "UPDATE agent_turns SET state = ?, revision = revision + 1
         WHERE id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
    )
    .bind(state)
    .bind(turn_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    if changed.rows_affected() == 0 || newer_turn_exists(pool, turn_id).await? {
        return Ok(());
    }
    if state == "unknown" {
        sqlx::query(
            "UPDATE agent_sessions SET state = 'needs_reconcile', revision = revision + 1, updated_at = ?
             WHERE id = (SELECT session_id FROM agent_turns WHERE id = ?) AND state != 'archived'",
        )
        .bind(crate::db::now_ms())
        .bind(turn_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    } else if matches!(state, "completed" | "interrupted" | "failed") {
        sqlx::query(
            "UPDATE agent_sessions SET state = 'idle', revision = revision + 1, updated_at = ?
             WHERE id = (SELECT session_id FROM agent_turns WHERE id = ?) AND state = 'stopping'",
        )
        .bind(crate::db::now_ms())
        .bind(turn_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

pub async fn resume_codex_thread<R, W>(
    pool: &SqlitePool,
    actor: &Actor,
    session_id: &str,
    reader: &mut R,
    writer: &mut W,
) -> AppResult<serde_json::Value>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    get_session(pool, actor, session_id).await?;
    let (thread_id,): (Option<String>,) =
        sqlx::query_as("SELECT upstream_thread_id FROM agent_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .map_err(anyhow::Error::from)?;
    let Some(thread_id) = thread_id.filter(|id| !id.is_empty()) else {
        return Err(AppError::unprocessable(
            "THREAD_MISSING",
            "this session has no upstream thread to resume",
        ));
    };
    write_line(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 33,
            "method": "thread/resume",
            "params": {"threadId": thread_id}
        }),
    )
    .await?;
    if let Err(err) = read_matching(reader, 33, CONTROL_BUDGET).await {
        mark_needs_reconcile(pool, session_id).await?;
        return Err(err);
    }
    write_line(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 34,
            "method": "thread/turns/list",
            "params": {"threadId": thread_id}
        }),
    )
    .await?;
    let listed = match read_matching(reader, 34, CONTROL_BUDGET).await {
        Ok(msg) => msg,
        Err(err) => {
            mark_needs_reconcile(pool, session_id).await?;
            return Err(err);
        }
    };
    let upstream = listed_turns(&listed);
    let local = sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT id, upstream_turn_id, state FROM agent_turns WHERE session_id = ?",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut unknown = Vec::new();
    for (id, upstream_id, state) in local {
        if !matches!(state.as_str(), "queued" | "starting" | "running" | "stop_requested") {
            continue;
        }
        let matched = upstream_id.as_deref().and_then(|needle| {
            upstream.iter().find(|(turn_id, _)| turn_id == needle).map(|(_, status)| *status)
        });
        match matched {
            Some("completed" | "interrupted" | "failed") => {
                record_turn_terminal(pool, actor, session_id, &id, matched.unwrap()).await?;
            }
            Some(_) => unknown.push(id),
            None => unknown.push(id),
        }
    }
    for id in &unknown {
        sqlx::query(
            "UPDATE agent_turns SET state = 'unknown', revision = revision + 1
             WHERE id = ? AND state IN ('queued', 'starting', 'running', 'stop_requested')",
        )
        .bind(id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    if unknown.is_empty() {
        sqlx::query(
            "UPDATE agent_sessions SET state = 'idle', revision = revision + 1, updated_at = ?
             WHERE id = ? AND state IN ('stopping', 'needs_reconcile', 'active')",
        )
        .bind(crate::db::now_ms())
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    } else {
        mark_needs_reconcile(pool, session_id).await?;
    }
    let unknown_messages: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = ? AND delivery_state = 'delivery_unknown'",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let committed = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT id, tool_name, result_ref FROM agent_actions
         WHERE session_id = ? AND state = 'committed' ORDER BY created_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(serde_json::json!({
        "thread_id": thread_id,
        "resumed": true,
        "unknown_turns": unknown,
        "delivery_unknown": unknown_messages.0,
        "committed_actions": committed.into_iter().map(|(id, tool, result)| serde_json::json!({
            "id": id,
            "tool_name": tool,
            "result_ref": result
        })).collect::<Vec<_>>()
    }))
}

async fn mark_needs_reconcile(pool: &SqlitePool, session_id: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE agent_sessions SET state = 'needs_reconcile', revision = revision + 1, updated_at = ?
         WHERE id = ? AND state != 'archived'",
    )
    .bind(crate::db::now_ms())
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

fn listed_turns(msg: &serde_json::Value) -> Vec<(String, &str)> {
    let turns = msg["result"]["turns"]
        .as_array()
        .or_else(|| msg["result"]["data"].as_array());
    let Some(turns) = turns else {
        return Vec::new();
    };
    turns
        .iter()
        .filter_map(|turn| {
            let id = turn["id"].as_str().or_else(|| turn["turnId"].as_str())?;
            let status = turn["status"].as_str().unwrap_or("");
            Some((id.to_string(), status))
        })
        .collect()
}

async fn read_matching<R>(
    reader: &mut R,
    id: i64,
    budget: std::time::Duration,
) -> AppResult<serde_json::Value>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let deadline = std::time::Instant::now() + budget;
    for _ in 0..20 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(upstream_timeout());
        }
        let line = match tokio::time::timeout(left, read_line(reader)).await {
            Ok(line) => line?,
            Err(_) => return Err(upstream_timeout()),
        };
        let msg: serde_json::Value = serde_json::from_str(line.trim()).map_err(|_| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "upstream line was not json")
        })?;
        if msg.get("id").and_then(|value| value.as_i64()) != Some(id) {
            continue;
        }
        if let Some(error) = msg.get("error") {
            let message = error["message"].as_str().unwrap_or("thread resume failed");
            if message.to_ascii_lowercase().contains("not found") {
                return Err(AppError::unprocessable("THREAD_MISSING", message));
            }
            return Err(AppError::unprocessable("UPSTREAM_PROTOCOL", message));
        }
        return Ok(msg);
    }
    Err(AppError::unprocessable(
        "UPSTREAM_PROTOCOL",
        "thread resume response was not seen",
    ))
}

pub async fn steer_codex_turn<W>(
    writer: &mut W,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
) -> AppResult<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    write_line(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "turn/steer",
            "params": {
                "threadId": thread_id,
                "expectedTurnId": expected_turn_id,
                "input": [{"type": "text", "text": text}]
            }
        }),
    )
    .await
}

pub async fn interrupt_codex_turn<R, W>(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    thread_id: &str,
    reader: &mut R,
    writer: &mut W,
) -> AppResult<&'static str>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    sqlx::query("UPDATE agent_turns SET state = 'stop_requested', revision = revision + 1 WHERE id = ? AND session_id = ?")
        .bind(turn_id)
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE agent_messages SET delivery_state = 'paused' WHERE session_id = ? AND delivery_state = 'queued'",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    write_line(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "turn/interrupt",
            "params": {"threadId": thread_id, "turnId": turn_id}
        }),
    )
    .await?;
    finish_interrupt(pool, session_id, turn_id, reader, CONTROL_BUDGET).await
}

pub async fn finish_interrupt<R>(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
    reader: &mut R,
    budget: std::time::Duration,
) -> AppResult<&'static str>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let deadline = std::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return control_timeout(pool, session_id, turn_id).await;
        }
        let line = match tokio::time::timeout(left, read_line(reader)).await {
            Ok(line) => line?,
            Err(_) => return control_timeout(pool, session_id, turn_id).await,
        };
        let msg: serde_json::Value = serde_json::from_str(line.trim()).map_err(|_| {
            AppError::unprocessable("UPSTREAM_PROTOCOL", "upstream line was not json")
        })?;
        if msg["method"].as_str() != Some("turn/completed") {
            continue;
        }
        let status = msg["params"]["turn"]["status"].as_str().unwrap_or("completed");
        let state = if status == "interrupted" {
            "interrupted"
        } else {
            "completed"
        };
        mark_turn(pool, turn_id, state).await?;
        return Ok(if status == "interrupted" { "interrupted" } else { "completed" });
    }
}

async fn control_timeout(
    pool: &SqlitePool,
    session_id: &str,
    turn_id: &str,
) -> AppResult<&'static str> {
    mark_turn(pool, turn_id, "unknown").await?;
    mark_needs_reconcile(pool, session_id).await?;
    Err(AppError::conflict(
        "NEEDS_RECONCILE",
        "control request timed out",
    ))
}

pub fn allowed_tools() -> &'static [&'static str] {
    &[
        "album.list",
        "album.read",
        "album.update",
        "album.reorder",
        "album.move_images",
        "image.list",
        "job.status",
        "job.audit_variants",
        "job.repair_variants",
        "upload.status",
        "download.status",
        "storage.check",
        "scans.request",
        "archive.request",
    ]
}

async fn link_turn_jobs(pool: &SqlitePool, session_id: &str, job_ids: &[String]) -> AppResult<()> {
    let turn: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM agent_turns WHERE session_id = ? AND state = 'running' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((turn_id,)) = turn else {
        return Ok(());
    };
    let now = crate::db::now_ms();
    for job_id in job_ids {
        sqlx::query(
            "INSERT INTO agent_turn_jobs(turn_id, job_id, submitted_at) VALUES(?, ?, ?)
             ON CONFLICT(turn_id, job_id) DO NOTHING",
        )
        .bind(&turn_id)
        .bind(job_id)
        .bind(now)
        .execute(pool)
        .await
        .map_err(anyhow::Error::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_deltas_wait_one_hundred_milliseconds() {
        let start = std::time::Instant::now();
        let mut coalescer = ReplyCoalescer::new();
        coalescer.mark_dirty(start);
        coalescer.mark_dirty(start + std::time::Duration::from_millis(40));
        assert!(!coalescer.due(start + std::time::Duration::from_millis(99)));
        assert!(coalescer.due(start + REPLY_COALESCE));
        coalescer.mark_flushed(start + REPLY_COALESCE);
        assert!(!coalescer.dirty());
        let again = start + REPLY_COALESCE;
        coalescer.mark_dirty(again);
        assert_eq!(coalescer.remaining(again), REPLY_COALESCE);
        assert!(coalescer.due(again + REPLY_COALESCE));
    }

    #[tokio::test]
    async fn handshake_budget_is_ten_seconds_and_a_short_wait_times_out() {
        assert_eq!(HANDSHAKE_BUDGET, std::time::Duration::from_secs(10));
        assert_eq!(CONTROL_BUDGET, std::time::Duration::from_secs(5));
        assert_eq!(TURN_BUDGET, std::time::Duration::from_secs(10 * 60));
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let started = std::time::Instant::now();
        let err = read_response(&mut rx, 7, std::time::Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("UPSTREAM_TIMEOUT"), "{err}");
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_millis(500), "{elapsed:?}");
    }
}
