use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::db;
use crate::error::{AppError, AppResult};

const DEFAULT_UPLOAD_ITEMS: i64 = 200;
const DEFAULT_DOWNLOAD_SELECTION: i64 = 50;
const DEFAULT_UPLOAD_CONCURRENCY: i64 = 3;
const DEFAULT_AI_BUDGET: i64 = 1_000_000;
const DEFAULT_STAGING_BYTES: i64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_LIBRARY_BYTES: i64 = 1024 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_upload_items: i64,
    pub upload_concurrency_hint: i64,
    pub max_download_selection: i64,
    pub ai_daily_token_budget: i64,
    pub staging_bytes: i64,
    pub library_bytes: i64,
    pub source: &'static str,
    pub revision: i64,
}

pub async fn limits(pool: &SqlitePool) -> AppResult<Limits> {
    let revision = kv_i64(pool, "settings_revision").await?.unwrap_or(1);
    let upload = kv_i64(pool, "limit_max_upload_items").await?;
    let download = kv_i64(pool, "limit_max_download_selection").await?;
    let concurrency = kv_i64(pool, "limit_upload_concurrency_hint").await?;
    let budget = kv_i64(pool, "ai_daily_token_budget").await?;
    let staging = kv_i64(pool, "limit_staging_bytes").await?;
    let library = kv_i64(pool, "limit_library_bytes").await?;
    let source = if upload.is_some() || download.is_some() || concurrency.is_some() || budget.is_some() {
        "sqlite"
    } else {
        "default"
    };
    Ok(Limits {
        max_upload_items: upload.unwrap_or(DEFAULT_UPLOAD_ITEMS),
        upload_concurrency_hint: concurrency.unwrap_or(DEFAULT_UPLOAD_CONCURRENCY),
        max_download_selection: download.unwrap_or(DEFAULT_DOWNLOAD_SELECTION),
        ai_daily_token_budget: budget.unwrap_or(DEFAULT_AI_BUDGET),
        staging_bytes: staging.unwrap_or(DEFAULT_STAGING_BYTES),
        library_bytes: library.unwrap_or(DEFAULT_LIBRARY_BYTES),
        source,
        revision,
    })
}

pub async fn patch_limits(
    pool: &SqlitePool,
    expected: i64,
    max_upload_items: Option<i64>,
    upload_concurrency_hint: Option<i64>,
    max_download_selection: Option<i64>,
    ai_daily_token_budget: Option<i64>,
) -> AppResult<Limits> {
    if max_upload_items.is_none()
        && upload_concurrency_hint.is_none()
        && max_download_selection.is_none()
        && ai_daily_token_budget.is_none()
    {
        return Err(AppError::unprocessable("SETTINGS_EMPTY", "no writable limit was provided"));
    }
    check_range("max_upload_items", max_upload_items, 1, DEFAULT_UPLOAD_ITEMS)?;
    check_range("upload_concurrency_hint", upload_concurrency_hint, 1, 8)?;
    check_range(
        "max_download_selection",
        max_download_selection,
        1,
        DEFAULT_DOWNLOAD_SELECTION,
    )?;
    check_range("ai_daily_token_budget", ai_daily_token_budget, 0, 10_000_000)?;
    let mut tx = db::begin_immediate(pool).await.map_err(anyhow::Error::from)?;
    let current = sqlx::query_as::<_, (String,)>(
        "SELECT value FROM schema_app WHERE key = 'settings_revision'",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .and_then(|row| row.0.parse().ok())
    .unwrap_or(1);
    if current != expected {
        return Err(AppError::precondition("REVISION_CONFLICT", "settings revision"));
    }
    if let Some(value) = max_upload_items {
        db::kv_set(&mut tx, "limit_max_upload_items", &value.to_string())
            .await
            .map_err(anyhow::Error::from)?;
    }
    if let Some(value) = upload_concurrency_hint {
        db::kv_set(&mut tx, "limit_upload_concurrency_hint", &value.to_string())
            .await
            .map_err(anyhow::Error::from)?;
    }
    if let Some(value) = max_download_selection {
        db::kv_set(&mut tx, "limit_max_download_selection", &value.to_string())
            .await
            .map_err(anyhow::Error::from)?;
    }
    if let Some(value) = ai_daily_token_budget {
        db::kv_set(&mut tx, "ai_daily_token_budget", &value.to_string())
            .await
            .map_err(anyhow::Error::from)?;
    }
    db::kv_set(&mut tx, "settings_revision", &(expected + 1).to_string())
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    limits(pool).await
}

pub fn limits_json(limits: &Limits) -> Value {
    json!({
        "source": limits.source,
        "writable": true,
        "max_upload_items": limits.max_upload_items,
        "upload_concurrency_hint": limits.upload_concurrency_hint,
        "max_download_selection": limits.max_download_selection,
        "ai_daily_token_budget": limits.ai_daily_token_budget,
        "staging_bytes": limits.staging_bytes,
        "library_bytes": limits.library_bytes
    })
}

fn check_range(name: &'static str, value: Option<i64>, min: i64, max: i64) -> AppResult<()> {
    if let Some(value) = value {
        if value < min || value > max {
            return Err(AppError::unprocessable("SETTINGS_RANGE", name));
        }
    }
    Ok(())
}

async fn kv_i64(pool: &SqlitePool, key: &str) -> AppResult<Option<i64>> {
    Ok(db::kv_get(pool, key)
        .await
        .map_err(anyhow::Error::from)?
        .and_then(|value| value.parse().ok()))
}
