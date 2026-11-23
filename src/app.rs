use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use crate::config::Config;
use crate::storage::Storage;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub storage: Storage,
    pub config: Arc<Config>,
    pub job_hints: Arc<crate::jobs::JobHints>,
    pub inline_codec: bool,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    pub mcp_socket: std::path::PathBuf,
}

impl AppState {
    pub fn wake_jobs(&self) -> bool {
        self.job_hints.push()
    }

    pub async fn media(&self) -> Result<crate::storage::Storage> {
        crate::storage::open_routed(&self.pool).await
    }
}

pub async fn bootstrap(config: Config, inline_codec: bool) -> Result<AppState> {
    crate::config::ensure_dirs(&config)?;
    let pool = crate::db::connect(&config.db_path()).await?;
    if crate::db::kv_get(&pool, "instance_id").await?.is_none() {
        let mut tx = crate::db::begin_immediate(&pool).await?;
        crate::db::kv_set(&mut tx, "instance_id", &crate::ids::new_id()).await?;
        crate::db::kv_set(&mut tx, "policy_epoch", "1").await?;
        tx.commit().await?;
    }
    crate::versions::ensure_default_profile(&pool).await?;
    let storage = crate::storage::bootstrap_local(&pool, config.media_root()).await?;
    let (password, source) = if let Some(file) = &config.admin_password_file {
        (Some(crate::config::read_password_file(file)?), "file")
    } else {
        (config.admin_password.clone(), "env")
    };
    crate::auth::sync_configured_password(
        &pool,
        &config.keys_root(),
        &config.admin_username,
        password.as_deref(),
        config.admin_password_revision.as_deref(),
        source,
        false,
    )
    .await?;
    let state = AppState {
        pool,
        storage,
        config: Arc::new(config),
        job_hints: Arc::new(crate::jobs::JobHints::new()),
        inline_codec,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mcp_socket: crate::mcp::allocate_socket(),
    };
    crate::mcp::serve(state.clone()).await?;
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    crate::db::kv_set(
        &mut tx,
        "mcp_socket",
        &state.mcp_socket.to_string_lossy(),
    )
    .await?;
    tx.commit().await?;
    crate::jobs::resume_workers(&state.pool).await?;
    crate::backup::finish_signed_backups(
        &state.pool,
        &state.config.backup_root(),
        &state.config.keys_root(),
    )
    .await?;
    crate::backup::recover_interrupted_backups(&state.pool).await?;
    crate::upgrade::hold_foreign_image(&state.pool, state.config.image_id.as_deref()).await?;
    crate::migrate_storage::recover_opening(&state.pool).await?;
    crate::jobs::reclaim_expired(&state.pool).await?;
    crate::storage::reconcile::ensure_daily(&state.pool, &state.storage.backend_id).await?;
    crate::album::enqueue_expired_trash(&state.pool).await?;
    crate::jobs::spawn_workers(state.clone());
    crate::backup_maintenance::spawn(state.clone());
    Ok(state)
}

pub fn lock_path(config: &Config) -> std::path::PathBuf {
    config.data_dir.join("state").join("gallery.write.lock")
}
