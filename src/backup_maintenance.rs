use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::backup_package as package;
use crate::{backup, db, jobs};

pub fn spawn(state: AppState) {
    if !state.config.daily_backup
        && !state.config.restore_drills
        && state.config.backup_replica.is_none()
    {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = schedule(&state, db::now_ms()).await {
                tracing::warn!(error = %err, "backup maintenance scheduling failed");
            }
        }
    });
}

pub async fn schedule(state: &AppState, now: i64) -> Result<()> {
    let mut failure = None;
    if state.config.daily_backup {
        failure = backup::ensure_daily_backup(state).await.err();
    }
    if state.config.restore_drills && !db::source_readonly(&state.pool).await? {
        if let Err(err) = schedule_drills(state, now).await {
            failure = Some(err);
        }
    }
    if state.config.backup_replica.is_some() {
        if let Err(err) = schedule_replica(state, now).await {
            failure = Some(err);
        }
    }
    failure.map_or(Ok(()), Err)
}

async fn schedule_drills(state: &AppState, now: i64) -> Result<()> {
    let month = chrono::DateTime::from_timestamp_millis(now)
        .context("maintenance clock")?
        .format("%Y-%m");
    let slot = format!("monthly:{month}");
    if let Some((id,)) = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs WHERE state = 'succeeded' AND snapshot_at IS NOT NULL
         ORDER BY snapshot_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?
    {
        enqueue(state, "restore_drill", &slot, &id, now).await?;
    }

    let migrations: Vec<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.cutover_at, (
            SELECT b.id FROM backup_runs b WHERE b.state = 'succeeded'
                AND b.snapshot_at >= m.cutover_at
                AND b.routing_epoch = r.routing_epoch
            ORDER BY b.snapshot_at DESC, b.id DESC LIMIT 1
         )
         FROM storage_migrations m JOIN storage_route r ON r.active_backend_id = m.target_backend_id
         WHERE m.stage = 'protected' AND m.cutover_at IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM backup_drills d WHERE d.slot = 'cutover:' || m.id)",
    )
    .fetch_all(&state.pool)
    .await?;
    for (migration, cutover, backup_id) in migrations {
        if let Some(id) = backup_id {
            enqueue(
                state,
                "restore_drill",
                &format!("cutover:{migration}"),
                &id,
                now,
            )
            .await?;
        } else {
            backup::ensure_backup_since(state, cutover).await?;
        }
    }
    Ok(())
}

async fn schedule_replica(state: &AppState, now: i64) -> Result<()> {
    let Some((id,)) = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM backup_runs WHERE state = 'succeeded' AND format_version = 2
         AND snapshot_at IS NOT NULL ORDER BY snapshot_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?
    else {
        return Ok(());
    };
    let (_, repository) = replica_location(state).await?;
    let present: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM backup_runs WHERE id = ? AND replica_verified_at IS NOT NULL
         AND replica_repository = ?",
    )
    .bind(&id)
    .bind(&repository)
    .fetch_one(&state.pool)
    .await?;
    if present.0 == 0 {
        enqueue(
            state,
            "backup_replica",
            &format!("{id}:{repository}"),
            &id,
            now,
        )
        .await?;
    }
    Ok(())
}

async fn enqueue(
    state: &AppState,
    kind: &str,
    slot: &str,
    backup_id: &str,
    now: i64,
) -> Result<()> {
    let day = now.div_euclid(86_400_000);
    let dedupe = format!("{kind}:{slot}:{backup_id}:{day}");
    let mut tx = db::begin_immediate(&state.pool).await?;
    let blocked: (i64,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM backup_drills WHERE slot = ?)
            OR EXISTS(SELECT 1 FROM jobs WHERE dedupe_key = ? OR
                    (kind = ? AND json_extract(scope_json, '$.slot') = ?
                        AND state IN ('queued','running','retry_wait','cancel_requested')))",
    )
    .bind(slot)
    .bind(&dedupe)
    .bind(kind)
    .bind(slot)
    .fetch_one(&mut *tx)
    .await?;
    if blocked.0 != 0 {
        return Ok(());
    }
    jobs::enqueue(
        &mut tx,
        kind,
        &dedupe,
        &jobs::JobPayload {
            kind: kind.into(),
            payload_version: 1,
            scope: json!({ "backup_id": backup_id, "slot": slot }),
            target: json!({}),
            actor_id: None,
        },
        None,
        0,
    )
    .await?;
    tx.commit().await?;
    state.wake_jobs();
    Ok(())
}

pub async fn run_drill(state: &AppState, payload: &str) -> Result<()> {
    let payload: Value = serde_json::from_str(payload)?;
    let id = payload["scope"]["backup_id"]
        .as_str()
        .context("drill backup")?;
    let slot = payload["scope"]["slot"].as_str().context("drill slot")?;
    let prior: Option<(String,)> =
        sqlx::query_as("SELECT backup_id FROM backup_drills WHERE slot = ?")
            .bind(slot)
            .fetch_optional(&state.pool)
            .await?;
    if prior.is_some() {
        return Ok(());
    }
    let temp = tempfile::tempdir_in(state.config.work_root())?;
    backup::restore_into(
        &state.config.keys_root(),
        id,
        None,
        None,
        None,
        temp.path(),
        &state.config.backup_root(),
    )
    .await?;
    let mut report: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("report.json"))?)?;
    ensure!(
        report["catalog_ok"] == true
            && report["smoke"]["ok"] == true
            && report["originals"] == report["originals_verified"],
        "isolated restoration did not pass its checks",
    );
    let isolated = db::connect(&temp.path().join("gallery.sqlite")).await?;
    let (owner, username, revision): (String, String, i64) = sqlx::query_as(
        "SELECT id, username, revision FROM actors WHERE role = 'owner' AND status = 'active'",
    )
    .fetch_one(&isolated)
    .await?;
    let actor = crate::auth::Actor {
        id: owner,
        username,
        revision,
        role: "owner".into(),
        must_change_password: false,
    };
    report["synthetic"] = backup::drill_smoke_on(&isolated, &actor).await?;
    isolated.close().await;
    let now = db::now_ms();
    let mut tx = db::begin_immediate(&state.pool).await?;
    sqlx::query(
        "INSERT INTO backup_drills(slot, backup_id, completed_at, report_json) VALUES(?, ?, ?, ?)
         ON CONFLICT(slot) DO NOTHING",
    )
    .bind(slot)
    .bind(id)
    .bind(now)
    .bind(report.to_string())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE backup_runs SET verified_at = ? WHERE id = ? AND state = 'succeeded'")
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn replica_location(state: &AppState) -> Result<(PathBuf, String)> {
    let root = state
        .config
        .backup_replica
        .as_ref()
        .context("backup replica is disabled")?
        .canonicalize()?;
    let source = state.config.data_dir.canonicalize()?;
    ensure!(
        root.is_dir() && !root.starts_with(&source) && !source.starts_with(&root),
        "backup replica must be outside the Gallery data directory"
    );
    let metadata = std::fs::metadata(&root)?;
    ensure!(
        metadata.dev() != std::fs::metadata(&source)?.dev()
            && metadata.dev() != std::fs::metadata(state.config.backup_root())?.dev(),
        "backup replica volume is unavailable or shares the primary filesystem"
    );
    let locals: Vec<(String,)> =
        sqlx::query_as("SELECT config_json FROM storage_backends WHERE kind = 'local'")
            .fetch_all(&state.pool)
            .await?;
    for (config,) in locals {
        let crate::storage::BackendConfig::Local { root: media } = serde_json::from_str(&config)?
        else {
            return Err(anyhow!("invalid local storage configuration"));
        };
        if let Ok(media) = media.canonicalize() {
            ensure!(
                std::fs::metadata(media)?.dev() != metadata.dev(),
                "backup replica shares a media filesystem"
            );
        }
    }
    let fingerprint =
        crate::ids::sha256_hex(format!("{}:{}", root.display(), metadata.dev()).as_bytes());
    Ok((root, fingerprint))
}

pub async fn verified_replica_since(state: &AppState, since: i64) -> Result<bool> {
    let (root, repository) = replica_location(state).await?;
    let ids: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM backup_runs WHERE state = 'succeeded' AND replica_verified_at IS NOT NULL
         AND replica_repository = ? AND snapshot_at >= ? ORDER BY snapshot_at DESC",
    )
    .bind(repository)
    .bind(since)
    .fetch_all(&state.pool)
    .await?;
    let keys = state.config.keys_root();
    Ok(ids
        .into_iter()
        .any(|(id,)| package::read_committed(&keys, &root.join(&id), &id).is_ok()))
}

pub async fn run_replica(state: &AppState, payload: &str) -> Result<()> {
    let payload: Value = serde_json::from_str(payload)?;
    let id = payload["scope"]["backup_id"]
        .as_str()
        .context("replica backup")?;
    let (root, repository) = replica_location(state).await?;
    let keys = state.config.keys_root();
    let primary = state.config.backup_root();
    let id_owned = id.to_owned();
    let copied =
        tokio::task::spawn_blocking(move || copy_replica(&keys, &primary, &root, &id_owned))
            .await??;
    let (_, current) = replica_location(state).await?;
    ensure!(repository == current, "backup replica location changed");
    let now = db::now_ms();
    sqlx::query(
        "UPDATE backup_runs SET replica_repository = ?, replica_verified_at = ? WHERE id = ? AND state = 'succeeded'",
    ).bind(repository).bind(now).bind(id).execute(&state.pool).await?;
    tracing::info!(backup_id = id, copied, "backup replica verified");
    Ok(())
}

pub(crate) fn copy_replica(keys: &Path, primary: &Path, replica: &Path, id: &str) -> Result<bool> {
    ensure!(
        id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && !id.is_empty(),
        "invalid backup id"
    );
    let source = primary.join(id);
    let (manifest_digest, manifest) = package::read_committed(keys, &source, id)?;
    ensure!(
        manifest["format_version"] == package::FORMAT,
        "legacy backup cannot be mirrored"
    );
    let files = replica_files(&manifest, &manifest_digest)?;
    let target = replica.join(id);
    if target.exists() {
        let (existing, _) = package::read_committed(keys, &target, id)?;
        ensure!(
            existing == manifest_digest,
            "replica has a different signed backup"
        );
        for (name, sha) in files {
            ensure!(
                crate::storage::local::hash_file(&target.join(name))?.1 == sha,
                "backup replica hash mismatch"
            );
        }
        return Ok(false);
    }
    let partial = tempfile::tempdir_in(replica)?;
    for (name, sha) in files {
        let path = partial.path().join(name);
        std::fs::copy(source.join(name), &path)?;
        let file = std::fs::File::open(&path)?;
        file.sync_all()?;
        ensure!(
            crate::storage::local::hash_file(&path)?.1 == sha,
            "backup replica hash mismatch"
        );
    }
    let complete = std::fs::read(source.join("COMPLETE"))?;
    package::atomic_write(&partial.path().join("COMPLETE"), &complete)?;
    package::read_committed(keys, partial.path(), id)?;
    std::fs::rename(partial.path(), target)?;
    std::fs::File::open(replica)?.sync_all()?;
    Ok(true)
}

fn replica_files<'a>(
    manifest: &'a Value,
    manifest_digest: &'a str,
) -> Result<Vec<(&'a str, &'a str)>> {
    let mut files = vec![
        (package::MANIFEST, manifest_digest),
        (
            package::CONFIG,
            manifest["config_sha256"]
                .as_str()
                .context("config digest")?,
        ),
        (
            "gallery.sqlite.age",
            manifest["sqlite_sha256"]
                .as_str()
                .context("sqlite digest")?,
        ),
    ];
    for item in package::items(manifest)? {
        files.push((
            package::object_name(item)?,
            item["ciphertext_sha256"]
                .as_str()
                .context("object digest")?,
        ));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encrypted_snapshot_replica_and_isolated_recovery() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut cfg = crate::config::Config::from_env()?;
        cfg.data_dir = temp.path().join("source");
        cfg.admin_password = Some("local-test-password".into());
        cfg.daily_backup = false;
        cfg.restore_drills = false;
        cfg.backup_replica = None;
        let mut state = crate::app::bootstrap(cfg.clone(), true).await?;
        jobs::shutdown_workers(&state.pool, Duration::from_secs(3)).await?;
        let (owner,): (String,) = sqlx::query_as("SELECT id FROM actors WHERE role = 'owner'")
            .fetch_one(&state.pool)
            .await?;
        let actor = crate::auth::Actor {
            id: owner,
            username: cfg.admin_username.clone(),
            role: "owner".into(),
            revision: 1,
            must_change_password: false,
        };
        let bytes = vec![42; 600_000];
        let sha = crate::ids::sha256_hex(&bytes);
        let media = state.media().await?;
        let key = "media/sample/sample.bin";
        media
            .put_bytes(key, &bytes, "application/octet-stream")
            .await?;
        let now = db::now_ms();
        let mut tx = db::begin_immediate(&state.pool).await?;
        let blob = crate::storage::record_blob(
            &mut tx,
            &sha,
            bytes.len() as i64,
            "application/octet-stream",
            "original",
            &media.backend_id,
            key,
        )
        .await?;
        sqlx::query("INSERT INTO images(id, source_sha256, original_blob_id, original_name, width, height,
            original_mime, created_seq, created_at) VALUES('image', ?, ?, 'image', 1, 1, 'image/png', 1, ?)")
            .bind(&sha).bind(&blob).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;

        let frozen = backup::freeze_source(&state.pool, &actor).await?;
        let queued = backup::start_backup(&state, &actor).await?;
        let id = queued["id"].as_str().context("backup id")?;
        let scope = |slot: &str| json!({"scope":{"backup_id":id,"slot":slot}}).to_string();
        backup::run_backup(&state, &scope("backup")).await?;
        assert!(db::source_readonly(&state.pool).await?);
        assert!(db::write_barrier(&state.pool).await?);
        let root = cfg.backup_root();
        let keys = cfg.keys_root();
        let dir = root.join(id);
        assert!(!dir.join("manifest.json").exists());
        assert!(!dir.join("config.json").exists());
        assert!(!dir.join(package::SEAL).exists());
        assert!(!dir.join("gallery.sqlite").exists());
        let (_, manifest) = package::read_committed(&keys, &dir, id)?;
        assert_eq!(manifest["format_version"], package::FORMAT);
        let config = package::check_config(&keys, &dir, &manifest)?.unwrap();
        assert!(!String::from_utf8(config)?.contains("local-test-password"));
        let report = backup::verify_backup(&state, &actor, id).await?;
        assert_eq!(report["objects_verified"], 1);
        assert_eq!(report["ok"], true);

        let replica = temp.path().join("replica");
        std::fs::create_dir(&replica)?;
        assert!(copy_replica(&keys, &root, &replica, id)?);
        assert!(!copy_replica(&keys, &root, &replica, id)?);
        let object = package::object_name(&package::items(&manifest)?[0])?;
        let replica_object = replica.join(id).join(object);
        let ciphertext = std::fs::read(&replica_object)?;
        std::fs::write(&replica_object, b"corrupt")?;
        assert!(copy_replica(&keys, &root, &replica, id).is_err());
        std::fs::write(&replica_object, ciphertext)?;

        run_drill(&state, &scope("monthly:2026-11")).await?;
        run_drill(&state, &scope("monthly:2026-11")).await?;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_drills")
            .fetch_one(&state.pool)
            .await?;
        assert_eq!(rows, 1);
        let checkpoints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM restore_runs")
            .fetch_one(&state.pool)
            .await?;
        assert_eq!(checkpoints, 0);

        // A new host can recover using the replica and its own copy of the key material.
        cfg.data_dir = temp.path().join("new-host");
        let recovered = crate::recovery::recover(&cfg, &replica, id, &keys).await?;
        assert_eq!(recovered["originals_verified"], 1);
        assert!(crate::recovery::recover(&cfg, &replica, id, &keys)
            .await
            .is_err());
        assert!(crate::recovery::accept(&cfg, Some("wrong")).await.is_err());
        assert_eq!(
            crate::recovery::accept(&cfg, frozen["ack"].as_str()).await?["writes"],
            "open"
        );
        let target = db::connect(&cfg.db_path()).await?;
        assert!(!db::write_barrier(&target).await?);
        let target_media = crate::storage::open_routed(&target).await?;
        assert_eq!(
            target_media
                .hash_object(object, bytes.len() as i64)
                .await?
                .0,
            sha
        );
        target.close().await;

        // A cutover with a new route selects a matching snapshot, once per slot.
        sqlx::query("DELETE FROM schema_app WHERE key = 'source_readonly'")
            .execute(&state.pool)
            .await?;
        sqlx::query("UPDATE storage_route SET write_barrier = 0")
            .execute(&state.pool)
            .await?;
        sqlx::query("INSERT INTO storage_migrations(id,source_backend_id,target_backend_id,direction,
            stage,state,cutover_at,created_at,updated_at) VALUES('migration',?,?,'forward','protected','protected',?,?,?)")
            .bind(&media.backend_id).bind(&media.backend_id).bind(now).bind(now).bind(now)
            .execute(&state.pool).await?;
        std::sync::Arc::make_mut(&mut state.config).restore_drills = true;
        schedule(&state, now).await?;
        schedule(&state, now).await?;
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'restore_drill'
            AND json_extract(scope_json, '$.slot') = 'cutover:migration'",
        )
        .fetch_one(&state.pool)
        .await?;
        assert_eq!(active, 1);
        Ok(())
    }
}
