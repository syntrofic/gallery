use anyhow::{anyhow, Result};

use crate::app::AppState;
use crate::processing::{CodecOp, CodecPlan};
use crate::storage::variant_key;

pub async fn execute(
    state: &AppState,
    kind: &str,
    payload_json: &str,
    job_id: &str,
    lease: &str,
) -> Result<()> {
    match kind {
        "generate_variant" => {
            generate_variant(state, payload_json, job_id, lease)
                .await
                .map_err(keep_permanent)
        }
        "build_archive" => {
            let v: serde_json::Value = serde_json::from_str(payload_json)?;
            let archive_id = v["scope"]["archive_id"]
                .as_str()
                .ok_or_else(|| anyhow!("archive_id"))?;
            crate::download::build_zip(state, archive_id).await
        }
        "purge_media" => purge_media(state, payload_json).await,
        "backup_replica" => crate::backup_maintenance::run_replica(state, payload_json).await,
        "restore_drill" => crate::backup_maintenance::run_drill(state, payload_json).await,
        "backup" => crate::backup::run_backup(state, payload_json).await,
        "restore" => crate::backup::run_restore(state, payload_json).await,
        "storage_migrate_copy" => crate::migrate_storage::copy_item(state, payload_json).await,
        "caption_image" => crate::vision::caption_image(state, payload_json).await,
        "scan_plan" => {
            let v: serde_json::Value = serde_json::from_str(payload_json)?;
            let scan_id = v["scope"]["scan_id"]
                .as_str()
                .ok_or_else(|| anyhow!("scan_id"))?;
            crate::ai::dispatch_scan_page(state, scan_id).await
        }
        "agent_run" => anyhow::bail!("ai jobs are gated"),
        "convert_image" => convert_image(state, payload_json, job_id, lease).await,
        "reconcile" => crate::storage::reconcile::run(state).await.map(|_| ()),
        "rebuild_search" => crate::search::rebuild(&state.pool).await.map(|_| ()),
        other => anyhow::bail!("unknown job kind {other}"),
    }
}

fn keep_permanent(err: anyhow::Error) -> anyhow::Error {
    let message = err.to_string();
    if let Some(index) = message.find("permanent:") {
        anyhow!("{}", &message[index..])
    } else {
        anyhow!("generate_variant: {message}")
    }
}

async fn publish_allowed(state: &AppState, actor_id: Option<&str>, image_id: &str) -> Result<()> {
    let Some(actor_id) = actor_id else {
        return Ok(());
    };
    let actor = sqlx::query_as::<_, (String, String)>(
        "SELECT role, status FROM actors WHERE id = ?",
    )
    .bind(actor_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((role, status)) = actor else {
        anyhow::bail!("permanent:PERMISSION_REVOKED");
    };
    if status != "active" {
        anyhow::bail!("permanent:PERMISSION_REVOKED");
    }
    if role == "owner" {
        return Ok(());
    }
    let (allowed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM album_images ai
         JOIN albums a ON a.id = ai.album_id
         JOIN album_grants g ON g.album_id = a.id
         WHERE ai.image_id = ? AND a.deleted_at IS NULL AND g.actor_id = ? AND g.edit = 1",
    )
    .bind(image_id)
    .bind(actor_id)
    .fetch_one(&state.pool)
    .await?;
    if allowed == 0 {
        anyhow::bail!("permanent:PERMISSION_REVOKED");
    }
    Ok(())
}

async fn generate_variant(
    state: &AppState,
    payload_json: &str,
    job_id: &str,
    lease: &str,
) -> Result<()> {
    let payload: crate::jobs::JobPayload = serde_json::from_str(payload_json)?;
    let image_id = payload.scope["image_id"].as_str().ok_or_else(|| anyhow!("image_id"))?;
    let spec_id = payload.target["spec_id"].as_str().ok_or_else(|| anyhow!("spec_id"))?;
    let profile_revision = payload.target["profile_revision"].as_i64().unwrap_or(1);
    let image = sqlx::query_as::<_, (String, String, String)>(
        "SELECT original_blob_id, original_mime, state FROM images WHERE id = ?",
    )
    .bind(image_id)
    .fetch_one(&state.pool)
    .await?;
    if image.2 != "active" {
        anyhow::bail!("permanent:IMAGE_UNAVAILABLE");
    }
    publish_allowed(state, payload.actor_id.as_deref(), image_id).await?;
    let (_, specs) = crate::versions::current_profile(&state.pool).await?;
    if !specs.iter().any(|spec| spec.spec_id == spec_id) {
        anyhow::bail!("permanent:CONFIG");
    }
    let (route_backend, route_epoch) = crate::db::current_route(&state.pool).await?;
    let media = crate::storage::open_by_id(&state.pool, &route_backend).await?;
    let (key, size, sha, _) =
        crate::storage::location_for(&state.pool, &image.0, &media.backend_id).await?;
    let root = state.config.work_root().join("codec");
    std::fs::create_dir_all(&root)?;
    let attempt = tempfile::Builder::new().prefix(job_id).tempdir_in(root)?;
    let work = attempt.path().to_path_buf();
    let input = work.join("original.bin");
    media.copy_verified(&key, size, &sha, &input).await.map_err(|err| {
        if err.is::<crate::storage::ObjectMismatch>() {
            anyhow!("permanent:UNDECODABLE original failed integrity check")
        } else {
            err
        }
    })?;
    let spec = specs
        .into_iter()
        .find(|s| s.spec_id == spec_id)
        .ok_or_else(|| anyhow!("unknown spec"))?;
    let output = work.join(format!("{spec_id}.png"));
    let plan = CodecPlan {
        plan_id: format!("{image_id}:{spec_id}"),
        ops: vec![CodecOp {
            input_path: input,
            output_path: output.clone(),
            spec_id: spec.spec_id.clone(),
            format: spec.format.clone(),
            width: spec.width,
            height: spec.height,
            long_edge: spec.long_edge,
            fit: spec.fit.clone(),
        }],
    };
    let results = if state.inline_codec {
        crate::processing::execute_plan(&plan)?
    } else {
        crate::processing::run_in_worker(
            &state.config.worker_bin,
            &plan,
            crate::processing::CONVERT_DEADLINE,
            Some(&work),
        )
        .await?
    };
    let out = results.into_iter().next().ok_or_else(|| anyhow!("no output"))?;
    let image_state = sqlx::query_as::<_, (String,)>("SELECT state FROM images WHERE id = ?")
        .bind(image_id)
        .fetch_one(&state.pool)
        .await?;
    if image_state.0 != "active" {
        anyhow::bail!("permanent:IMAGE_UNAVAILABLE");
    }
    publish_allowed(state, payload.actor_id.as_deref(), image_id).await?;
    let object_key = variant_key(image_id, spec_id, profile_revision, &out.sha256);
    let already = reusable_variant_blob(
        &state.pool,
        &media.backend_id,
        image_id,
        &out.sha256,
        out.size as i64,
    )
    .await?;
    let mut staged = false;
    if already.is_none() {
        media
            .backend().put_file(&object_key, &out.output_path, &out.mime)
            .await?;
        staged = true;
    }
    if !lease_held(&state.pool, lease).await? {
        discard_own_staging(state, &work, staged.then_some(object_key.as_str())).await?;
        anyhow::bail!("lease lost");
    }
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    let live = sqlx::query_as::<_, (String,)>("SELECT state FROM images WHERE id = ?")
        .bind(image_id)
        .fetch_optional(&mut *tx)
        .await?;
    if live.as_ref().map(|row| row.0.as_str()) != Some("active") {
        tx.rollback().await.ok();
        discard_own_staging(state, &work, staged.then_some(object_key.as_str())).await?;
        anyhow::bail!("permanent:IMAGE_UNAVAILABLE");
    }
    if !crate::db::publish_route_ok(&mut *tx, &media.backend_id, route_epoch).await? {
        tx.rollback().await.ok();
        discard_own_staging(state, &work, staged.then_some(object_key.as_str())).await?;
        anyhow::bail!("ROUTE_CHANGED");
    }
    let reused = reusable_variant_blob(
        &mut *tx,
        &media.backend_id,
        image_id,
        &out.sha256,
        out.size as i64,
    )
    .await?;
    let discard_staged = staged && reused.is_some();
    let blob_id = if let Some(id) = reused {
        id
    } else if staged {
        crate::storage::record_blob(
            &mut tx,
            &out.sha256,
            out.size as i64,
            &out.mime,
            "variant",
            &media.backend_id,
            &object_key,
        )
        .await?
    } else {
        tx.rollback().await.ok();
        anyhow::bail!("variant bytes disappeared before publish");
    };
    let variant_id = crate::ids::new_id();
    sqlx::query(
        "INSERT INTO image_variants(id, image_id, profile_revision, spec_id, blob_id, state, width, height)
         VALUES(?, ?, ?, ?, ?, 'published', ?, ?)
         ON CONFLICT(image_id, profile_revision, spec_id) DO UPDATE SET
            blob_id = excluded.blob_id, state = 'published', width = excluded.width, height = excluded.height",
    )
    .bind(&variant_id)
    .bind(image_id)
    .bind(profile_revision)
    .bind(spec_id)
    .bind(&blob_id)
    .bind(out.width as i64)
    .bind(out.height as i64)
    .execute(&mut *tx)
    .await?;
    let (vid,): (String,) = sqlx::query_as(
        "SELECT id FROM image_variants WHERE image_id = ? AND profile_revision = ? AND spec_id = ?",
    )
    .bind(image_id)
    .bind(profile_revision)
    .bind(spec_id)
    .fetch_one(&mut *tx)
    .await?;
    let now = crate::db::now_ms();
    let claimed = crate::versions::claim_variant_head(
        &mut tx,
        image_id,
        spec_id,
        &vid,
        profile_revision,
    )
    .await?;
    if claimed {
        sqlx::query(
            "UPDATE image_variants SET retain_until = ?
             WHERE image_id = ? AND spec_id = ? AND id != ? AND state = 'published' AND retain_until IS NULL",
        )
        .bind(now + 7 * 24 * 60 * 60 * 1000)
        .bind(image_id)
        .bind(spec_id)
        .bind(&vid)
        .execute(&mut *tx)
        .await?;
        crate::db::bump_catalog(&mut tx).await?;
        crate::events::emit_tx(
            &mut tx,
            "variant.published",
            "image",
            Some(image_id),
            Some(spec_id),
            Some(profile_revision),
            None,
            serde_json::json!({"variant_id": vid}),
        )
        .await?;
    } else {
        sqlx::query("UPDATE image_variants SET retain_until = ? WHERE id = ? AND retain_until IS NULL")
            .bind(now + 7 * 24 * 60 * 60 * 1000)
            .bind(&vid)
            .execute(&mut *tx)
            .await?;
        crate::events::emit_tx(
            &mut tx,
            "variant.stale",
            "image",
            Some(image_id),
            Some(spec_id),
            Some(profile_revision),
            None,
            serde_json::json!({"variant_id": vid, "reason": "stale_revision"}),
        )
        .await?;
    }
    tx.commit().await?;
    if discard_staged {
        let _ = media.delete_key(&object_key).await;
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

async fn reusable_variant_blob<'e, E>(
    executor: E,
    backend_id: &str,
    image_id: &str,
    sha: &str,
    size: i64,
) -> Result<Option<String>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT b.id
         FROM image_variants v
         JOIN media_blobs b ON b.id = v.blob_id
         JOIN media_locations l ON l.blob_id = b.id
         WHERE v.image_id = ? AND b.sha256 = ? AND b.size = ?
           AND l.backend_id = ? AND l.state = 'verified'
         LIMIT 1",
    )
    .bind(image_id)
    .bind(sha)
    .bind(size)
    .bind(backend_id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|item| item.0))
}

async fn convert_image(
    state: &AppState,
    payload_json: &str,
    job_id: &str,
    lease: &str,
) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let conversion_id = payload["scope"]["conversion_id"]
        .as_str()
        .ok_or_else(|| anyhow!("conversion_id"))?;
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT source_image_id, spec_json FROM conversions WHERE id = ?",
    )
    .bind(conversion_id)
    .fetch_one(&state.pool)
    .await?;
    let image_id = row.0;
    let spec: crate::versions::VariantSpec = serde_json::from_str(&row.1)?;
    let (blob_id,): (String,) = sqlx::query_as("SELECT original_blob_id FROM images WHERE id = ?")
        .bind(&image_id)
        .fetch_one(&state.pool)
        .await?;
    let media = state.media().await?;
    let (key, size, sha, _) =
        crate::storage::location_for(&state.pool, &blob_id, &media.backend_id).await?;
    let root = state.config.work_root().join("convert");
    std::fs::create_dir_all(&root)?;
    let attempt = tempfile::Builder::new().prefix(job_id).tempdir_in(root)?;
    let work = attempt.path().to_path_buf();
    let input = work.join("in.bin");
    let output = work.join("out.bin");
    media.copy_verified(&key, size, &sha, &input).await?;
    let plan = CodecPlan {
        plan_id: conversion_id.to_string(),
        ops: vec![CodecOp {
            input_path: input,
            output_path: output.clone(),
            spec_id: spec.spec_id.clone(),
            format: spec.format.clone(),
            width: spec.width,
            height: spec.height,
            long_edge: spec.long_edge,
            fit: spec.fit.clone(),
        }],
    };
    let results = crate::processing::execute_plan(&plan)?;
    let out = results.into_iter().next().ok_or_else(|| anyhow!("no output"))?;
    let object_key = format!("work/conversions/{conversion_id}");
    media
        .backend().put_file(&object_key, &out.output_path, &out.mime)
        .await?;
    if !lease_held(&state.pool, lease).await? {
        discard_own_staging(state, &work, Some(&object_key)).await?;
        anyhow::bail!("lease lost");
    }
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    let blob = crate::storage::record_blob(
        &mut tx,
        &out.sha256,
        out.size as i64,
        &out.mime,
        "other",
        &media.backend_id,
        &object_key,
    )
    .await?;
    sqlx::query(
        "UPDATE conversions SET state = 'ready', output_blob_id = ?, expires_at = ? WHERE id = ?",
    )
    .bind(&blob)
    .bind(crate::db::now_ms() + 24 * 3600 * 1000)
    .bind(conversion_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

pub async fn discard_own_staging(
    state: &AppState,
    attempt_dir: &std::path::Path,
    object_key: Option<&str>,
) -> Result<()> {
    if let Some(key) = object_key {
        let (referenced,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM media_locations WHERE object_key = ? AND state = 'verified'",
        )
        .bind(key)
        .fetch_one(&state.pool)
        .await?;
        if referenced == 0 {
            if let Ok(media) = state.media().await {
                let _ = media.delete_key(key).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(attempt_dir);
    Ok(())
}

async fn lease_held(pool: &sqlx::SqlitePool, lease: &str) -> Result<bool> {
    if lease.is_empty() {
        return Ok(true);
    }
    let (held,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM jobs
         WHERE lease_token = ? AND state = 'running' AND lease_until > ?",
    )
    .bind(lease)
    .bind(crate::db::now_ms())
    .fetch_one(pool)
    .await?;
    Ok(held == 1)
}

async fn purge_media(state: &AppState, payload_json: &str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let image_id = payload["scope"]["image_id"]
        .as_str()
        .ok_or_else(|| anyhow!("image_id"))?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT original_blob_id FROM images WHERE id = ? AND state = 'purging'")
            .bind(image_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((blob_id,)) = row else {
        return Ok(());
    };
    let (pinned,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM backup_pins p
         JOIN backup_runs r ON r.id = p.backup_id
         WHERE p.blob_id = ? AND r.state IN ('queued', 'running')",
    )
    .bind(&blob_id)
    .fetch_one(&state.pool)
    .await?;
    if pinned > 0 {
        anyhow::bail!("backup pin held");
    }
    let media = state.media().await?;
    let loc = crate::storage::location_for(&state.pool, &blob_id, &media.backend_id).await;
    if let Ok((key, _, _, _)) = loc {
        media.backend().delete(&key).await?;
    }
    sqlx::query("UPDATE images SET state = 'purged', deleted_at = ? WHERE id = ?")
        .bind(crate::db::now_ms())
        .bind(image_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}
