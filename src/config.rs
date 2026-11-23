use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// Keep the existing configuration and implementations available for a later release.
pub(crate) const AI_FEATURES_AVAILABLE: bool = false;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub web_root: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub public_url: String,
    pub admin_username: String,
    pub admin_password: Option<String>,
    pub admin_password_file: Option<PathBuf>,
    pub admin_password_revision: Option<String>,
    pub ai_enabled: bool,
    pub daily_backup: bool,
    pub restore_drills: bool,
    pub backup_replica: Option<PathBuf>,
    pub worker_bin: PathBuf,
    pub image_id: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen: SocketAddr = std::env::var("GALLERY_LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()
            .context("GALLERY_LISTEN_ADDR")?;
        let data_dir = PathBuf::from(
            std::env::var("GALLERY_DATA_DIR").unwrap_or_else(|_| "./gallery-data".into()),
        );
        let admin_password = nonempty_env("GALLERY_ADMIN_PASSWORD");
        let admin_password_file = nonempty_env("GALLERY_ADMIN_PASSWORD_FILE").map(PathBuf::from);
        Ok(Self {
            listen,
            web_root: nonempty_env("GALLERY_WEB_ROOT").map(PathBuf::from),
            data_dir,
            public_url: std::env::var("GALLERY_PUBLIC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
            admin_username: std::env::var("GALLERY_ADMIN_USERNAME")
                .unwrap_or_else(|_| "admin".into()),
            admin_password,
            admin_password_file,
            admin_password_revision: nonempty_env("GALLERY_ADMIN_PASSWORD_REVISION"),
            ai_enabled: AI_FEATURES_AVAILABLE && std::env::var("GALLERY_AI_ENABLED").ok().as_deref() == Some("1"),
            daily_backup: std::env::var("GALLERY_DAILY_BACKUP").ok().as_deref() == Some("1"),
            restore_drills: std::env::var("GALLERY_RESTORE_DRILLS").ok().as_deref() == Some("1"),
            backup_replica: nonempty_env("GALLERY_BACKUP_REPLICA_DIR").map(PathBuf::from),
            worker_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gallery")),
            image_id: nonempty_env("GALLERY_IMAGE_ID"),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("db").join("gallery.sqlite")
    }

    pub fn media_root(&self) -> PathBuf {
        self.data_dir.join("media")
    }

    pub fn work_root(&self) -> PathBuf {
        self.data_dir.join("work")
    }

    pub fn backup_root(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    pub fn keys_root(&self) -> PathBuf {
        self.data_dir.join("keys")
    }

    pub fn cookie_secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn ensure_dirs(cfg: &Config) -> Result<()> {
    for p in [
        cfg.data_dir.join("db"),
        cfg.media_root(),
        cfg.work_root(),
        cfg.backup_root(),
        cfg.keys_root(),
        cfg.data_dir.join("state"),
    ] {
        std::fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
    }
    Ok(())
}

pub fn read_password_file(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

pub fn openrouter_key() -> Option<String> {
    let path = std::env::var("GALLERY_OPENROUTER_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("secrets/openrouter.key"));
    let text = std::fs::read_to_string(path).ok()?;
    let key = text.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}
