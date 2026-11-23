use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, SqlitePool, Transaction};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5));
    // journal_mode=WAL takes an exclusive lock that sqlite3_busy_timeout cannot wait for.
    // Set it once, then let the pool open connections without repeating that pragma.
    {
        let bootstrap = sqlx::sqlite::SqliteConnection::connect_with(
            &options.clone().journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .context("enable sqlite wal")?;
        bootstrap.close().await.context("close wal bootstrap")?;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .context("connect sqlite")?;
    sqlx::migrate!("./migrations").run(&pool).await.context("migrate")?;
    Ok(pool)
}

pub async fn begin_immediate(pool: &SqlitePool) -> Result<Transaction<'_, sqlx::Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

pub async fn kv_get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM schema_app WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

pub async fn kv_set(tx: &mut Transaction<'_, sqlx::Sqlite>, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO schema_app(key, value) VALUES(?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn bump_catalog(tx: &mut Transaction<'_, sqlx::Sqlite>) -> Result<i64> {
    sqlx::query("UPDATE storage_route SET catalog_revision = catalog_revision + 1 WHERE id = 1")
        .execute(&mut **tx)
        .await?;
    let (rev,): (i64,) = sqlx::query_as("SELECT catalog_revision FROM storage_route WHERE id = 1")
        .fetch_one(&mut **tx)
        .await?;
    Ok(rev)
}

pub async fn current_route(pool: &SqlitePool) -> Result<(String, i64)> {
    Ok(sqlx::query_as("SELECT active_backend_id, routing_epoch FROM storage_route WHERE id = 1")
        .fetch_one(pool)
        .await?)
}

pub async fn publish_route_ok<'e, E>(executor: E, backend_id: &str, epoch: i64) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row: (String, i64) =
        sqlx::query_as("SELECT active_backend_id, routing_epoch FROM storage_route WHERE id = 1")
            .fetch_one(executor)
            .await?;
    Ok(row.0 == backend_id && row.1 == epoch)
}

pub async fn source_readonly(pool: &SqlitePool) -> Result<bool> {
    Ok(kv_get(pool, "source_readonly").await?.as_deref() == Some("1"))
}

pub async fn write_barrier(pool: &SqlitePool) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT write_barrier FROM storage_route WHERE id = 1")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0 == 1).unwrap_or(false))
}

pub async fn policy_epoch(pool: &SqlitePool) -> Result<i64> {
    Ok(kv_get(pool, "policy_epoch")
        .await?
        .and_then(|s| s.parse().ok())
        .unwrap_or(1))
}

pub async fn instance_id(pool: &SqlitePool) -> Result<String> {
    Ok(kv_get(pool, "instance_id")
        .await?
        .unwrap_or_else(|| "unknown".into()))
}
