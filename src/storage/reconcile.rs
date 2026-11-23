use anyhow::Result;
use serde_json::json;

use crate::app::AppState;
use crate::db;

pub const GRACE_MS: i64 = 24 * 60 * 60 * 1000;

pub async fn ensure_daily(pool: &sqlx::SqlitePool, backend_id: &str) -> Result<Option<String>> {
    if crate::db::write_barrier(pool).await? {
        return Ok(None);
    }
    if crate::db::kv_get(pool, "workers_draining").await?.as_deref() == Some("1") {
        return Ok(None);
    }
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let dedupe = format!("reconcile:{backend_id}:{day}");
    let payload = crate::jobs::JobPayload {
        kind: "reconcile".into(),
        payload_version: 1,
        scope: serde_json::json!({"backend_id": backend_id, "day": day}),
        target: serde_json::json!({}),
        actor_id: None,
    };
    let mut tx = crate::db::begin_immediate(pool).await?;
    let id = crate::jobs::enqueue(&mut tx, "reconcile", &dedupe, &payload, None, 20).await?;
    tx.commit().await?;
    Ok(Some(id))
}

pub fn clean_old_partials(root: &std::path::Path) -> Result<Vec<String>> {
    let migrate = root.join("work").join("migrate");
    if !migrate.is_dir() {
        return Ok(Vec::new());
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_millis(GRACE_MS as u64))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let mut removed = Vec::new();
    let mut stack = vec![migrate];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|err| anyhow::anyhow!(err.to_string()))? {
            let entry = entry.map_err(|err| anyhow::anyhow!(err.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".partial") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::now());
            if modified > cutoff {
                continue;
            }
            std::fs::remove_file(&path).map_err(|err| anyhow::anyhow!(err.to_string()))?;
            if let Ok(relative) = path.strip_prefix(root) {
                removed.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(removed)
}

pub async fn clean_attempt_leftovers(pool: &sqlx::SqlitePool) -> Result<(Vec<String>, u64)> {
    let locals = sqlx::query_as::<_, (String,)>("SELECT config_json FROM storage_backends WHERE kind = 'local'")
        .fetch_all(pool)
        .await?;
    let mut partials = Vec::new();
    for (json,) in locals {
        let Ok(cfg) = serde_json::from_str::<crate::storage::BackendConfig>(&json) else {
            continue;
        };
        let crate::storage::BackendConfig::Local { root } = cfg else {
            continue;
        };
        if root.as_os_str().is_empty() {
            continue;
        }
        partials.extend(clean_old_partials(&root).unwrap_or_default());
    }
    let cutoff = db::now_ms() - GRACE_MS;
    let due = sqlx::query_as::<_, (String, String, String)>(
        "SELECT upload_id, backend_id, object_key FROM storage_multipart_uploads WHERE opened_at < ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut uploads = 0_u64;
    for (upload_id, backend_id, key) in due {
        let Ok(storage) = crate::storage::open_by_id(pool, &backend_id).await else {
            continue;
        };
        if storage.backend().abort_multipart(&key, &upload_id).await.is_err() {
            continue;
        }
        sqlx::query("DELETE FROM storage_multipart_uploads WHERE upload_id = ?")
            .bind(&upload_id)
            .execute(pool)
            .await?;
        uploads += 1;
    }
    let backends = sqlx::query_as::<_, (String,)>("SELECT id FROM storage_backends WHERE kind = 's3'")
        .fetch_all(pool)
        .await?;
    for (backend_id,) in backends {
        let Ok(storage) = crate::storage::open_by_id(pool, &backend_id).await else {
            continue;
        };
        let Ok(found) = storage.backend().abort_stale_multiparts(cutoff).await else {
            continue;
        };
        uploads += found;
    }
    Ok((partials, uploads))
}

pub async fn run(state: &AppState) -> Result<serde_json::Value> {
    let _ = clean_attempt_leftovers(&state.pool).await?;
    let media = state.media().await?;
    crate::upload::reclaim_expired_reservations(&state.pool, &media).await?;
    crate::upload::reclaim_abandoned_uploads(&state.pool, &media).await?;
    crate::versions::reclaim_expired_variants(&state.pool, &media).await?;
    crate::album::enqueue_expired_trash(&state.pool)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    crate::events::prune_audit(&state.pool).await?;
    crate::events::retain_events(&state.pool).await?;
    let mut walked = Vec::new();
    let mut cursor = None;
    loop {
        let page = media.walk_owned(cursor.as_deref(), 200).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().cloned();
        let full = page.len() < 200;
        walked.extend(page);
        if full {
            break;
        }
    }
    let referenced = sqlx::query_as::<_, (String,)>(
        "SELECT object_key FROM media_locations WHERE backend_id = ?
         UNION
         SELECT planned_object_key FROM original_reservations",
    )
    .bind(&media.backend_id)
    .fetch_all(&state.pool)
    .await?;
    let referenced: std::collections::HashSet<String> = referenced.into_iter().map(|row| row.0).collect();
    let seen: std::collections::HashSet<&str> = walked.iter().map(String::as_str).collect();
    let now = db::now_ms();
    let mut marked = 0_i64;
    for key in &walked {
        if referenced.contains(key) {
            sqlx::query("DELETE FROM storage_reclaim WHERE backend_id = ? AND object_key = ? AND state = 'marked'")
                .bind(&media.backend_id)
                .bind(key)
                .execute(&state.pool)
                .await?;
            continue;
        }
        let inserted = sqlx::query(
            "INSERT INTO storage_reclaim(backend_id, object_key, marked_at, state)
             VALUES(?, ?, ?, 'marked')
             ON CONFLICT(backend_id, object_key) DO NOTHING",
        )
        .bind(&media.backend_id)
        .bind(key)
        .bind(now)
        .execute(&state.pool)
        .await?;
        marked += inserted.rows_affected() as i64;
    }
    let due = sqlx::query_as::<_, (String,)>(
        "SELECT object_key FROM storage_reclaim
         WHERE backend_id = ? AND state = 'marked' AND marked_at <= ?",
    )
    .bind(&media.backend_id)
    .bind(now - GRACE_MS)
    .fetch_all(&state.pool)
    .await?;
    let mut deleted = 0_i64;
    for (key,) in due {
        if referenced.contains(&key) {
            sqlx::query("DELETE FROM storage_reclaim WHERE backend_id = ? AND object_key = ?")
                .bind(&media.backend_id)
                .bind(&key)
                .execute(&state.pool)
                .await?;
            continue;
        }
        media.delete_key(&key).await?;
        sqlx::query(
            "UPDATE storage_reclaim SET state = 'deleted' WHERE backend_id = ? AND object_key = ?",
        )
        .bind(&media.backend_id)
        .bind(&key)
        .execute(&state.pool)
        .await?;
        deleted += 1;
    }
    let missing = sqlx::query_as::<_, (String,)>(
        "SELECT object_key FROM media_locations WHERE backend_id = ? AND state = 'verified'",
    )
    .bind(&media.backend_id)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .filter(|row| !seen.contains(row.0.as_str()))
    .count() as i64;
    Ok(json!({
        "walked": walked.len() as i64,
        "marked": marked,
        "deleted": deleted,
        "missing": missing
    }))
}
