use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gallery::config::Config;
use gallery::processing::codec_worker_main;
use gallery::scheduler::{issue_bootstrap_token, router};

#[derive(Parser)]
#[command(name = "gallery", about = "Personal self-hosted gallery")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    /// Recover a standalone data directory from a signed backup package.
    Recover {
        #[arg(long)]
        repository: std::path::PathBuf,
        #[arg(long)]
        backup_id: String,
        #[arg(long)]
        keys_dir: std::path::PathBuf,
    },
    /// Open writes on a verified, isolated offline recovery.
    AcceptRecovery {
        #[arg(long)]
        source_ack: Option<String>,
    },
    /// Switch a standalone Gallery upstream after a verified host handoff.
    Handoff {
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        source_ack: String,
        #[arg(long, default_value = "http://127.0.0.1:2019")]
        caddy_admin: String,
        #[arg(long)]
        config_path: String,
    },
    #[command(name = "codec-worker")]
    CodecWorker,
    Healthcheck {
        #[arg(long, default_value = "live")]
        kind: String,
    },
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    Upgrade {
        #[command(subcommand)]
        command: UpgradeCommand,
    },
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
    /// MCP stdio proxy. Codex spawns this; credentials come from the process environment.
    Mcp,
}

#[derive(Subcommand)]
enum UpgradeCommand {
    Plan,
    Rollback,
}

#[derive(Subcommand)]
enum MaintenanceCommand {
    Enter {
        #[arg(long)]
        reason: String,
    },
    Release,
}

#[derive(Subcommand)]
enum AdminCommand {
    #[command(name = "bootstrap-token")]
    BootstrapToken,
    #[command(name = "password-apply")]
    PasswordApply {
        #[arg(long)]
        from_config: bool,
        #[arg(long)]
        rebind: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gallery=info,tower_http=info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::CodecWorker => codec_worker_main(),
        Command::Healthcheck { kind } => healthcheck(&kind).await,
        Command::Admin { command } => admin(command).await,
        Command::Upgrade { command } => upgrade_cmd(command).await,
        Command::Maintenance { command } => maintenance_cmd(command).await,
        Command::Mcp => gallery::mcp::proxy_stdio().await,
        Command::Serve => serve().await,
        Command::Recover { repository, backup_id, keys_dir } => {
            let report = gallery::recovery::recover(&Config::from_env()?, &repository, &backup_id, &keys_dir).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        Command::AcceptRecovery { source_ack } => {
            let report = gallery::recovery::accept(&Config::from_env()?, source_ack.as_deref()).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        Command::Handoff { source, target, source_ack, caddy_admin, config_path } => {
            let report = gallery::handoff::switch_caddy(
                &Config::from_env()?, &source, &target, &source_ack, &caddy_admin, &config_path,
            ).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
    }
}

async fn serve() -> Result<()> {
    let mut config = Config::from_env()?;
    if config.web_root.is_none() {
        let development = std::path::PathBuf::from("frontend/dist");
        if development.join("index.html").is_file() {
            config.web_root = Some(development);
        }
    }
    if let Some(root) = &config.web_root {
        anyhow::ensure!(root.join("index.html").is_file(), "Gallery frontend index is missing");
    }
    let lock_path = gallery::app::lock_path(&config);
    std::fs::create_dir_all(lock_path.parent().unwrap())?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    lock_file
        .try_lock_exclusive()
        .context("another gallery writer holds the data directory lock")?;
    std::mem::forget(lock_file);
    let listen = config.listen;
    let state = gallery::app::bootstrap(config, false).await?;
    let pool = state.pool.clone();
    let app = router(state).layer(axum::middleware::from_fn(gallery::http_util::request_id_mw));
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "gallery listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = gallery::jobs::shutdown_workers(&pool, std::time::Duration::from_secs(10)).await;
        })
        .await?;
    Ok(())
}

trait FileLock {
    fn try_lock_exclusive(&self) -> Result<()>;
}

impl FileLock for std::fs::File {
    fn try_lock_exclusive(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(self.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                anyhow::bail!("lock failed");
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }
}

async fn healthcheck(kind: &str) -> Result<()> {
    let addr = std::env::var("GALLERY_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let path = if kind == "ready" {
        "/health/ready"
    } else {
        "/health/live"
    };
    let url = format!("http://{addr}{path}");
    let res = reqwest::get(&url).await?;
    if res.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("health {kind} {}", res.status())
    }
}

async fn maintenance_cmd(cmd: MaintenanceCommand) -> Result<()> {
    let config = Config::from_env()?;
    let pool = gallery::db::connect(&config.db_path()).await?;
    let image = config.image_id.as_deref().unwrap_or("local");
    match cmd {
        MaintenanceCommand::Enter { reason } => {
            gallery::upgrade::enter_maintenance(&pool, &reason, image).await?;
            println!("maintenance");
            Ok(())
        }
        MaintenanceCommand::Release => {
            gallery::upgrade::release_maintenance(&pool, image).await?;
            println!("released");
            Ok(())
        }
    }
}

async fn upgrade_cmd(cmd: UpgradeCommand) -> Result<()> {
    let config = Config::from_env()?;
    match cmd {
        UpgradeCommand::Plan => {
            let report = gallery::upgrade::plan(&config.db_path()).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        UpgradeCommand::Rollback => {
            let record = gallery::upgrade::rollback(&config.data_dir)?;
            println!("{}", record.status);
            Ok(())
        }
    }
}

async fn admin(cmd: AdminCommand) -> Result<()> {
    let config = Config::from_env()?;
    let pool = gallery::db::connect(&config.db_path()).await?;
    match cmd {
        AdminCommand::BootstrapToken => {
            let token = issue_bootstrap_token(&pool).await?;
            println!("{token}");
            Ok(())
        }
        AdminCommand::PasswordApply { from_config, rebind } => {
            if !from_config {
                anyhow::bail!("pass --from-config");
            }
            let password = if let Some(file) = &config.admin_password_file {
                gallery::config::read_password_file(file)?
            } else {
                config
                    .admin_password
                    .clone()
                    .context("GALLERY_ADMIN_PASSWORD")?
            };
            if !rebind {
                anyhow::bail!("refusing to apply without --rebind");
            }
            gallery::auth::sync_configured_password(
                &pool,
                &config.keys_root(),
                &config.admin_username,
                Some(&password),
                config.admin_password_revision.as_deref(),
                if config.admin_password_file.is_some() { "file" } else { "env" },
                true,
            )
            .await?;
            Ok(())
        }
    }
}
