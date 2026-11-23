use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use crate::backup::{self, restore_into};
use crate::backup_maintenance::copy_replica;
use crate::backup_package::{self as package, atomic_write};
use crate::config::Config;
use crate::db;

pub async fn recover(config: &Config, repository: &Path, id: &str, keys: &Path) -> Result<Value> {
    ensure!(
        id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && !id.is_empty(),
        "invalid backup id"
    );
    ensure!(
        !config.data_dir.exists(),
        "recovery data directory must be new"
    );
    let parent = config
        .data_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize()?;
    let target = parent.join(config.data_dir.file_name().context("recovery directory")?);
    let scratch = tempfile::tempdir_in(&parent)?;
    let mut staging = config.clone();
    staging.data_dir = scratch.path().to_path_buf();
    crate::config::ensure_dirs(&staging)?;
    let isolated = staging.work_root().join("isolated");
    let (_, manifest) = package::read_committed(keys, &repository.join(id), id)?;
    ensure!(
        manifest["format_version"] == package::FORMAT,
        "offline recovery expects an encrypted v2 backup"
    );
    restore_into(keys, id, None, None, None, &isolated, repository).await?;
    let report: Value = serde_json::from_slice(&std::fs::read(isolated.join("report.json"))?)?;
    ensure!(
        report["catalog_ok"] == true
            && report["smoke"]["ok"] == true
            && report["originals"] == report["originals_verified"],
        "isolated recovery checks failed"
    );
    let db_path = isolated.join("gallery.sqlite");
    let pool = db::connect(&db_path).await?;
    let now = db::now_ms();
    let root = target.join("media");
    sqlx::query("UPDATE storage_backends SET config_json = ? WHERE id = 'restore-target'")
        .bind(json!({"kind":"local","root":root}).to_string())
        .execute(&pool)
        .await?;
    // The recovered host can only serve the package it actually received.
    sqlx::query("DELETE FROM backup_pins")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE backup_runs SET state = 'unavailable' WHERE id <> ? AND state IN ('queued','running','succeeded')")
        .bind(id).execute(&pool).await?;
    sqlx::query(
        "INSERT INTO backup_runs(id, state, created_at, updated_at, instance_id, branch_id,
          snapshot_at, catalog_revision, routing_epoch, schema_version, format_version, verified_at)
         VALUES(?, 'succeeded', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET state = 'succeeded', updated_at = excluded.updated_at,
          snapshot_at = excluded.snapshot_at, catalog_revision = excluded.catalog_revision,
          routing_epoch = excluded.routing_epoch, schema_version = excluded.schema_version,
          format_version = excluded.format_version, verified_at = excluded.verified_at",
    )
    .bind(id)
    .bind(now)
    .bind(now)
    .bind(manifest["instance_id"].as_str())
    .bind(manifest["branch_id"].as_str().unwrap_or("main"))
    .bind(manifest["snapshot_at"].as_i64())
    .bind(manifest["catalog_revision"].as_i64())
    .bind(manifest["routing_epoch"].as_i64())
    .bind(manifest["schema_version"].as_i64())
    .bind(package::FORMAT)
    .bind(now)
    .execute(&pool)
    .await?;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await?;
    pool.close().await;
    copy_replica(keys, repository, &staging.backup_root(), id)?;
    for name in [
        "backup-age.key",
        "backup-age.pub",
        "backup-ed25519.key",
        "backup-ed25519.pub",
    ] {
        let src = keys.join(name);
        if src.is_file() {
            atomic_write(&staging.keys_root().join(name), &std::fs::read(src)?)?;
        }
    }
    std::fs::rename(db_path, staging.db_path())?;
    std::fs::rename(isolated.join("media"), staging.media_root())?;
    std::fs::rename(
        isolated.join("report.json"),
        staging.data_dir.join("state/recovery-report.json"),
    )?;
    std::fs::remove_dir_all(isolated)?;
    ensure!(
        !target.exists(),
        "recovery destination was created concurrently"
    );
    std::fs::rename(scratch.path(), &target)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(report)
}

pub async fn accept(config: &Config, source_ack: Option<&str>) -> Result<Value> {
    ensure!(
        config.data_dir.join("state/recovery-report.json").is_file(),
        "no offline recovery to accept"
    );
    let pool = db::connect(&config.db_path()).await?;
    if let Some(expected) = db::kv_get(&pool, "restored_from_freeze_ack").await? {
        ensure!(
            source_ack == Some(&expected),
            "source freeze acknowledgement does not match the restored snapshot"
        );
    }
    if let Some(at) = db::kv_get(&pool, "restore_accepted_at").await? {
        return Ok(json!({"writes":"open","accepted_at":at.parse::<i64>()?}));
    }
    let actor: (String, String, i64) = sqlx::query_as(
        "SELECT id, username, revision FROM actors WHERE role = 'owner' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await?;
    let actor = crate::auth::Actor {
        id: actor.0,
        username: actor.1,
        revision: actor.2,
        role: "owner".into(),
        must_change_password: false,
    };
    Ok(backup::accept_isolated_restore(&pool, &actor, &config.media_root()).await?)
}
