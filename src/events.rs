use anyhow::Result;
use sqlx::{SqlitePool, Transaction};

use crate::db;

pub async fn emit(
    pool: &SqlitePool,
    ty: &str,
    scope_kind: &str,
    scope_id: Option<&str>,
    resource_id: Option<&str>,
    revision: Option<i64>,
    actor_id: Option<&str>,
    payload: serde_json::Value,
) -> Result<i64> {
    let mut tx = crate::db::begin_immediate(pool).await?;
    let id = emit_tx(
        &mut tx,
        ty,
        scope_kind,
        scope_id,
        resource_id,
        revision,
        actor_id,
        payload,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn emit_tx(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    ty: &str,
    scope_kind: &str,
    scope_id: Option<&str>,
    resource_id: Option<&str>,
    revision: Option<i64>,
    actor_id: Option<&str>,
    payload: serde_json::Value,
) -> Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO events(type, scope_kind, scope_id, resource_id, revision, actor_id, payload_json, occurred_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(ty)
    .bind(scope_kind)
    .bind(scope_id)
    .bind(resource_id)
    .bind(revision)
    .bind(actor_id)
    .bind(payload.to_string())
    .bind(db::now_ms())
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

pub struct EventFeed {
    pub stale: bool,
    pub cursor: i64,
    pub events: Vec<serde_json::Value>,
}

pub async fn event_floor(pool: &SqlitePool) -> Result<i64> {
    let raw = db::kv_get(pool, "event_floor").await?;
    Ok(raw.and_then(|value| value.parse().ok()).unwrap_or(0))
}

pub const EVENT_RETAIN_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const EVENT_RETAIN_COUNT: i64 = 100_000;

pub async fn retain_events(pool: &SqlitePool) -> Result<u64> {
    retain_events_limited(pool, EVENT_RETAIN_MS, EVENT_RETAIN_COUNT).await
}

pub async fn retain_events_limited(pool: &SqlitePool, retain_ms: i64, keep: i64) -> Result<u64> {
    let cutoff = db::now_ms() - retain_ms;
    let (expired,): (Option<i64>,) =
        sqlx::query_as("SELECT MAX(id) FROM events WHERE occurred_at < ?")
            .bind(cutoff)
            .fetch_one(pool)
            .await?;
    let overflow = if keep > 0 {
        sqlx::query_as::<_, (i64,)>("SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?")
            .bind(keep)
            .fetch_optional(pool)
            .await?
            .map(|row| row.0)
    } else {
        None
    };
    let Some(inclusive) = [expired, overflow].into_iter().flatten().max() else {
        return Ok(0);
    };
    prune_through(pool, inclusive).await
}

pub async fn prune_through(pool: &SqlitePool, inclusive_id: i64) -> Result<u64> {
    let mut tx = db::begin_immediate(pool).await?;
    let deleted = sqlx::query("DELETE FROM events WHERE id <= ?")
        .bind(inclusive_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let current = sqlx::query_as::<_, (String,)>("SELECT value FROM schema_app WHERE key = 'event_floor'")
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|(value,)| value.parse::<i64>().ok())
        .unwrap_or(0);
    if inclusive_id > current {
        db::kv_set(&mut tx, "event_floor", &inclusive_id.to_string()).await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

pub async fn feed(
    pool: &SqlitePool,
    after_id: Option<i64>,
    actor_id: Option<&str>,
    owner: bool,
    limit: i64,
) -> Result<EventFeed> {
    let floor = event_floor(pool).await?;
    let watermark = sqlx::query_as::<_, (Option<i64>,)>("SELECT MAX(id) FROM events")
        .fetch_one(pool)
        .await?
        .0
        .unwrap_or(floor);
    let Some(after_id) = after_id else {
        return Ok(EventFeed {
            stale: false,
            cursor: watermark,
            events: Vec::new(),
        });
    };
    if after_id < floor {
        return Ok(EventFeed {
            stale: true,
            cursor: watermark,
            events: Vec::new(),
        });
    }
    let events = after(pool, after_id, actor_id, owner, limit).await?;
    Ok(EventFeed {
        stale: false,
        cursor: watermark,
        events,
    })
}

pub async fn after(
    pool: &SqlitePool,
    after_id: i64,
    actor_id: Option<&str>,
    owner: bool,
    limit: i64,
) -> Result<Vec<serde_json::Value>> {
    let rows = if owner {
        sqlx::query_as::<_, (i64, String, String, Option<String>, Option<String>, Option<i64>, i64)>(
            "SELECT id, type, scope_kind, scope_id, resource_id, revision, occurred_at
             FROM events WHERE id > ? ORDER BY id ASC LIMIT ?",
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else if let Some(actor_id) = actor_id {
        sqlx::query_as(
            "SELECT id, type, scope_kind, scope_id, resource_id, revision, occurred_at
             FROM events e
             WHERE e.id > ?
               AND (
                 e.actor_id = ?
                 OR (
                   e.scope_kind = 'album'
                   AND EXISTS (
                     SELECT 1 FROM albums a
                     LEFT JOIN album_grants g ON g.album_id = a.id AND g.actor_id = ?
                     WHERE a.id = e.scope_id AND a.deleted_at IS NULL
                       AND (a.visibility = 'public' OR IFNULL(g.browse, 0) = 1)
                   )
                 )
               )
             ORDER BY e.id ASC LIMIT ?",
        )
        .bind(after_id)
        .bind(actor_id)
        .bind(actor_id)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        vec![]
    };
    Ok(rows
        .into_iter()
        .map(|(id, ty, sk, sid, rid, rev, at)| {
            serde_json::json!({
                "id": id,
                "type": ty,
                "scope_kind": sk,
                "scope_id": sid,
                "resource_id": rid,
                "revision": rev,
                "occurred_at": db::rfc3339(at)
            })
        })
        .collect())
}

pub const AUDIT_RETAIN_MS: i64 = 180 * 24 * 60 * 60 * 1000;

pub async fn prune_audit(pool: &SqlitePool) -> Result<i64> {
    let retain = crate::db::kv_get(pool, "audit_retain_ms")
        .await?
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(AUDIT_RETAIN_MS);
    let cutoff = crate::db::now_ms() - retain;
    let removed = sqlx::query("DELETE FROM audit_log WHERE occurred_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() as i64)
}

pub async fn export_audit_jsonl(pool: &SqlitePool, actor: &crate::auth::Actor) -> crate::error::AppResult<String> {
    crate::auth::require_owner(actor)?;
    let retain = crate::db::kv_get(pool, "audit_retain_ms")
        .await
        .map_err(anyhow::Error::from)?
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(AUDIT_RETAIN_MS);
    let cutoff = crate::db::now_ms() - retain;
    let rows = sqlx::query_as::<_, (i64, Option<String>, Option<String>, String, Option<String>, String, Option<String>, i64)>(
        "SELECT id, request_id, actor_id, action, target, result, detail, occurred_at
         FROM audit_log WHERE occurred_at >= ? ORDER BY id",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let mut out = String::new();
    for (id, request_id, actor_id, action, target, result, detail, occurred_at) in rows {
        let line = serde_json::json!({
            "id": id,
            "occurred_at": crate::db::rfc3339(occurred_at),
            "request_id": request_id,
            "actor_id": actor_id,
            "action": action,
            "target": target,
            "result": result,
            "detail": detail.as_deref().and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok()).unwrap_or(serde_json::Value::Null)
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

pub async fn audit(
    pool: &SqlitePool,
    request_id: &str,
    actor_id: Option<&str>,
    action: &str,
    target: Option<&str>,
    result: &str,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log(request_id, actor_id, action, target, result, detail, occurred_at)
         VALUES(?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(request_id)
    .bind(actor_id)
    .bind(action)
    .bind(target)
    .bind(result)
    .bind(detail)
    .bind(db::now_ms())
    .execute(pool)
    .await?;
    Ok(())
}
