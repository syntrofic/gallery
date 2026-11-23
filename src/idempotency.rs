use sqlx::SqlitePool;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::ids::sha256_hex;

pub struct Replay {
    pub status: u16,
    pub body: serde_json::Value,
}

pub async fn lookup(
    pool: &SqlitePool,
    actor_id: &str,
    method: &str,
    route: &str,
    key: &str,
    body: &serde_json::Value,
) -> AppResult<Option<Replay>> {
    let hash = sha256_hex(body.to_string().as_bytes());
    let row = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT request_hash, status, response_json FROM idempotency_records
         WHERE actor_id = ? AND method = ? AND route = ? AND key = ?",
    )
    .bind(actor_id)
    .bind(method)
    .bind(route)
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((stored, status, response)) = row else {
        return Ok(None);
    };
    if stored != hash {
        return Err(AppError::conflict(
            "IDEMPOTENCY_CONFLICT",
            "same key with a different request",
        ));
    }
    let body = serde_json::from_str(&response).unwrap_or(serde_json::json!({}));
    Ok(Some(Replay {
        status: status as u16,
        body,
    }))
}

pub async fn store(
    pool: &SqlitePool,
    actor_id: &str,
    method: &str,
    route: &str,
    key: &str,
    body: &serde_json::Value,
    status: u16,
    response: &serde_json::Value,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO idempotency_records(actor_id, method, route, key, request_hash, status, response_json, created_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(actor_id, method, route, key) DO NOTHING",
    )
    .bind(actor_id)
    .bind(method)
    .bind(route)
    .bind(key)
    .bind(sha256_hex(body.to_string().as_bytes()))
    .bind(status as i64)
    .bind(response.to_string())
    .bind(db::now_ms())
    .execute(pool)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}

pub struct Command {
    actor_id: String,
    method: &'static str,
    route: String,
    key: Option<String>,
    body: serde_json::Value,
}

impl Command {
    pub fn prepare(
        actor_id: &str,
        method: &'static str,
        route: impl Into<String>,
        headers: &axum::http::HeaderMap,
        body: serde_json::Value,
    ) -> Self {
        Self {
            actor_id: actor_id.to_string(),
            method,
            route: route.into(),
            key: header_key(headers),
            body,
        }
    }

    pub async fn replay(&self, pool: &SqlitePool) -> AppResult<Option<Replay>> {
        let Some(key) = &self.key else {
            return Ok(None);
        };
        lookup(pool, &self.actor_id, self.method, &self.route, key, &self.body).await
    }

    pub async fn remember(
        &self,
        pool: &SqlitePool,
        status: u16,
        response: &serde_json::Value,
    ) -> AppResult<()> {
        let Some(key) = &self.key else {
            return Ok(());
        };
        store(
            pool,
            &self.actor_id,
            self.method,
            &self.route,
            key,
            &self.body,
            status,
            response,
        )
        .await
    }
}

pub fn header_key(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
}
