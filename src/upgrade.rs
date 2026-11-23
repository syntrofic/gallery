use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::db;
use crate::ids::{new_id, sha256_hex};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveDb {
    Previous,
    Candidate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpgradeRecord {
    pub id: String,
    pub previous_db: PathBuf,
    pub candidate_db: PathBuf,
    pub active: ActiveDb,
    pub writes_opened: bool,
    pub status: String,
}

pub fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state").join("upgrade.json")
}

pub fn load(data_dir: &Path) -> Result<UpgradeRecord> {
    let text = std::fs::read_to_string(record_path(data_dir))?;
    Ok(serde_json::from_str(&text)?)
}

fn save(data_dir: &Path, record: &UpgradeRecord) -> Result<()> {
    let path = record_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(record)?)?;
    Ok(())
}

pub async fn prepare(data_dir: &Path, source_db: &Path) -> Result<UpgradeRecord> {
    let id = new_id();
    let candidate = data_dir
        .join("db")
        .join("generations")
        .join(&id)
        .join("gallery.sqlite");
    if let Some(parent) = candidate.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pool = db::connect(source_db).await?;
    let escaped = candidate.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{escaped}'"))
        .execute(&pool)
        .await?;
    let record = UpgradeRecord {
        id,
        previous_db: source_db.to_path_buf(),
        candidate_db: candidate,
        active: ActiveDb::Previous,
        writes_opened: false,
        status: "prepared".into(),
    };
    save(data_dir, &record)?;
    Ok(record)
}

pub async fn apply(data_dir: &Path, migration_id: &str, sql: &str) -> Result<UpgradeRecord> {
    let mut record = load(data_dir)?;
    if record.writes_opened {
        anyhow::bail!("writes already opened; refuse schema change on the upgrade record");
    }
    let checksum = sha256_hex(sql.as_bytes());
    let pool = db::connect(&record.candidate_db).await?;
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT checksum FROM schema_migrations_app WHERE id = ?")
            .bind(migration_id)
            .fetch_optional(&pool)
            .await?;
    if let Some((old,)) = existing {
        if old != checksum {
            anyhow::bail!("migration checksum changed");
        }
        return Ok(record);
    }
    let mut tx = db::begin_immediate(&pool).await?;
    if let Err(err) = sqlx::query(sql).execute(&mut *tx).await {
        record.status = "apply_failed".into();
        save(data_dir, &record)?;
        anyhow::bail!("candidate migration failed: {err}");
    }
    sqlx::query(
        "INSERT INTO schema_migrations_app(id, before_schema, after_schema, checksum, status, applied_at)
         VALUES(?, 'current', 'next', ?, 'applied', ?)",
    )
    .bind(migration_id)
    .bind(&checksum)
    .bind(db::now_ms())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    record.status = "applied".into();
    save(data_dir, &record)?;
    Ok(record)
}

pub async fn activate(data_dir: &Path) -> Result<UpgradeRecord> {
    let mut record = load(data_dir)?;
    if record.status == "apply_failed" {
        anyhow::bail!("refusing to activate a failed candidate");
    }
    let pool = db::connect(&record.candidate_db).await?;
    let (integrity,): (String,) = sqlx::query_as("PRAGMA integrity_check").fetch_one(&pool).await?;
    if integrity != "ok" {
        anyhow::bail!("candidate integrity_check failed");
    }
    let violations = sqlx::query("PRAGMA foreign_key_check").fetch_all(&pool).await?;
    if !violations.is_empty() {
        anyhow::bail!("candidate foreign_key_check failed");
    }
    let (foreign_keys,): (i64,) = sqlx::query_as("PRAGMA foreign_keys").fetch_one(&pool).await?;
    if foreign_keys != 1 {
        anyhow::bail!("candidate connection left foreign_keys disabled");
    }
    record.active = ActiveDb::Candidate;
    record.status = "activated".into();
    save(data_dir, &record)?;
    Ok(record)
}

pub fn open_writes(data_dir: &Path) -> Result<UpgradeRecord> {
    let mut record = load(data_dir)?;
    record.writes_opened = true;
    record.status = "writes_opened".into();
    save(data_dir, &record)?;
    Ok(record)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradePlan {
    pub sqlite_version: String,
    pub applied: Vec<i64>,
    pub pending: Vec<i64>,
}

pub async fn plan(db_path: &Path) -> Result<UpgradePlan> {
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .read_only(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let (sqlite_version,): (String,) = sqlx::query_as("SELECT sqlite_version()").fetch_one(&pool).await?;
    let applied = sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await?;
    let embedded: Vec<(i64, Vec<u8>)> = sqlx::migrate!("./migrations")
        .iter()
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect();
    let mut pending = Vec::new();
    for (version, checksum) in &embedded {
        match applied.iter().find(|(applied_version, _)| applied_version == version) {
            Some((_, stored)) if stored != checksum => {
                anyhow::bail!("migration checksum changed: {version}");
            }
            Some(_) => {}
            None => pending.push(*version),
        }
    }
    for (version, _) in &applied {
        if !embedded.iter().any(|(embedded_version, _)| embedded_version == version) {
            anyhow::bail!("applied migration is not in this release: {version}");
        }
    }
    Ok(UpgradePlan {
        sqlite_version,
        applied: applied.into_iter().map(|(version, _)| version).collect(),
        pending,
    })
}

pub async fn enter_maintenance(pool: &sqlx::SqlitePool, reason: &str, image_id: &str) -> Result<()> {
    let reason = reason.trim();
    let image_id = image_id.trim();
    if reason.is_empty() || image_id.is_empty() {
        anyhow::bail!("maintenance reason and image id are required");
    }
    let mut tx = db::begin_immediate(pool).await?;
    let updated = sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    if updated.rows_affected() != 1 {
        anyhow::bail!("storage route missing");
    }
    db::kv_set(&mut tx, "maintenance_reason", reason).await?;
    db::kv_set(&mut tx, "maintenance_image", image_id).await?;
    db::kv_set(&mut tx, "workers_draining", "1").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn release_maintenance(pool: &sqlx::SqlitePool, image_id: &str) -> Result<()> {
    let image_id = image_id.trim();
    let stored = db::kv_get(pool, "maintenance_image").await?;
    if let Some(stored) = stored.as_deref() {
        if !stored.is_empty() && stored != image_id {
            anyhow::bail!("image digest mismatch; write barrier remains");
        }
    }
    if db::source_readonly(pool).await? {
        anyhow::bail!("source is read-only until handoff finishes");
    }
    let mut tx = db::begin_immediate(pool).await?;
    sqlx::query(
        "UPDATE storage_route SET write_barrier = 0 WHERE id = 1
         AND NOT EXISTS (SELECT 1 FROM schema_app WHERE key = 'source_readonly' AND value = '1')",
    )
        .execute(&mut *tx)
        .await?;
    db::kv_set(&mut tx, "workers_draining", "0").await?;
    db::kv_set(&mut tx, "maintenance_reason", "").await?;
    db::kv_set(&mut tx, "maintenance_image", "").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn hold_foreign_image(pool: &sqlx::SqlitePool, image_id: Option<&str>) -> Result<bool> {
    let stored = db::kv_get(pool, "maintenance_image").await?;
    let Some(stored) = stored.as_deref().filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    if image_id == Some(stored) {
        return Ok(false);
    }
    let mut tx = db::begin_immediate(pool).await?;
    sqlx::query("UPDATE storage_route SET write_barrier = 1 WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    db::kv_set(&mut tx, "workers_draining", "1").await?;
    tx.commit().await?;
    Ok(true)
}

pub fn rollback(data_dir: &Path) -> Result<UpgradeRecord> {
    let mut record = load(data_dir)?;
    if record.writes_opened {
        anyhow::bail!("writes opened; roll back from the current database, not the old generation");
    }
    record.active = ActiveDb::Previous;
    record.status = "rolled_back".into();
    save(data_dir, &record)?;
    Ok(record)
}
