mod account;
mod auth;
mod backup;
mod catalog;
mod config;
mod crypto;
mod db;
mod enrich;
mod files;
mod integrations;
mod jellyfin;
mod library;
mod localtime;
mod logfile;
mod playlist;
mod settings;
mod setup;
mod tmdb;
mod version;
mod web;

use anyhow::Context;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::crypto::{KeyKind, MasterKey};
use crate::db::KeyCheck;
use crate::web::Blocked;

const USAGE: &str = "\
Usage: m3usort [COMMAND]

Commands:
  serve         Run the web app (default)
  gen-key       Print a new random M3USORT_KEY
  rotate-key    Re-encrypt all stored credentials with a new key (stop the app first)
  reset-admin   Set a new random admin password and log out all admin sessions
  reset-ip IP   Clear the login lockout for one IP (or --all for every IP)
  restore FILE  Check a backup (.db.gz) against M3USORT_KEY and put it in place at the next start
                (add --without-credentials for a backup made with another key)
  locked-ips    List IPs that are locked out right now
  version       Print version and commit
  healthcheck   Exit 0 if the running app answers on /healthcheck (for Docker HEALTHCHECK)
";

/// M3USORT_LOG_LEVEL (default info). The SMB library logs every share connect
/// at info, so it is kept at warn unless the value names smb2 itself.
fn log_filter() -> String {
    let v = std::env::var("M3USORT_LOG_LEVEL").unwrap_or_default();
    let v = if v.trim().is_empty() {
        "info".to_string()
    } else {
        v.trim().to_string()
    };
    if v.contains("smb2") {
        v
    } else {
        format!("{v},smb2=warn")
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_timer(localtime::LogTimer)
        .with_ansi(false)
        .with_writer(logfile::Tee)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(log_filter())
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,smb2=warn")),
        )
        .init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let result = match cmd.as_str() {
        "serve" => serve().await,
        "gen-key" => {
            println!("M3USORT_KEY={}", crypto::generate_key());
            Ok(())
        }
        "rotate-key" => rotate_key().await,
        "reset-admin" => reset_admin().await,
        "reset-ip" => reset_ip(std::env::args().nth(2)).await,
        "restore" => {
            restore(
                std::env::args().nth(2),
                std::env::args().any(|a| a == "--without-credentials"),
            )
            .await
        }
        "locked-ips" => locked_ips().await,
        "healthcheck" => healthcheck().await,
        "version" | "-V" | "--version" => {
            println!("{}", version::version_line());
            Ok(())
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(anyhow::anyhow!("unknown command '{other}'\n\n{USAGE}")),
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}

async fn serve() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("cannot create data dir {}", cfg.data_dir.display()))?;
    let _lock = lock_data_dir(&cfg)?;
    if let Err(e) = logfile::enable(&cfg.data_dir.join("logs")) {
        tracing::warn!("no log files (log viewer empty): {e}");
    }
    match backup::apply_pending(&cfg.data_dir, &cfg.db_path()) {
        Ok(true) => tracing::info!(
            "restored the database from a backup; the previous one is kept as m3usort.db.before-restore-…"
        ),
        Ok(false) => {}
        Err(e) => {
            tracing::error!("restoring the staged backup failed (current database kept): {e:#}")
        }
    }
    backup::init(&cfg.data_dir, cfg.key.as_deref().unwrap_or_default());
    warn_if_not_a_volume(&cfg.data_dir);
    let db_exists = cfg.db_path().exists();
    info!("{}", version::version_line());
    let update = version::spawn_checker();

    let router = match (&cfg.key, db_exists) {
        (None, false) => {
            let suggested = crypto::generate_key();
            warn!("M3USORT_KEY is not set. Add this line to .env and recreate the container:");
            warn!("M3USORT_KEY={suggested}");
            web::blocked_router(Blocked::NeedKey { suggested })
        }
        (None, true) => {
            error!(
                "M3USORT_KEY is not set but {} exists; refusing to start",
                cfg.db_path().display()
            );
            web::blocked_router(Blocked::KeyMissingForExistingDb)
        }
        (Some(raw), _) => {
            let pool = db::open(&cfg.db_path(), true).await?;
            let key = derive_key(raw, &pool).await?;
            if let KeyKind::Passphrase(n) = key.kind() {
                if n < crypto::SHORT_PASSPHRASE {
                    warn!(
                        "M3USORT_KEY is a short passphrase ({n} characters). Fine on a trusted LAN, but a \
                         stolen database or backup can be brute-forced offline. `m3usort gen-key` makes a random key."
                    );
                } else {
                    info!("M3USORT_KEY is a passphrase, stretched with Argon2id");
                }
            }
            match db::verify_or_init_key(&pool, &key, true).await? {
                KeyCheck::Initialised => {
                    info!("database bound to M3USORT_KEY (new install, or a backup restored without its credentials)");
                    start_app(pool, key, update.clone()).await?
                }
                KeyCheck::Ok => {
                    info!("M3USORT_KEY verified");
                    start_app(pool, key, update.clone()).await?
                }
                KeyCheck::Rebound => {
                    warn!(
                        "M3USORT_KEY changed before setup was finished; database re-bound to the new key"
                    );
                    start_app(pool, key, update.clone()).await?
                }
                KeyCheck::Mismatch => {
                    error!("M3USORT_KEY does not match this database; nothing was changed");
                    web::blocked_router(Blocked::KeyMismatch)
                }
            }
        }
    };

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.listen))?;
    info!("listening on http://{}", cfg.listen);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Build the app state, start background jobs, return the router.
async fn start_app(
    pool: sqlx::SqlitePool,
    key: MasterKey,
    update: version::SharedStatus,
) -> anyhow::Result<axum::Router> {
    let state = web::AppState::new(pool, key, update).await?;
    catalog::spawn_scheduler(state.clone());
    Ok(web::app_router(state))
}

async fn rotate_key() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let raw = cfg
        .key
        .as_deref()
        .context("M3USORT_KEY (the current key) must be set to rotate")?;
    let _lock = lock_data_dir(&cfg)?;
    anyhow::ensure!(
        cfg.db_path().exists(),
        "no database at {}",
        cfg.db_path().display()
    );

    let pool = db::open(&cfg.db_path(), false).await?;
    let old = derive_key(raw, &pool).await?;
    match db::verify_or_init_key(&pool, &old, false).await? {
        KeyCheck::Ok => {}
        KeyCheck::Mismatch => {
            anyhow::bail!("M3USORT_KEY does not match this database; nothing was changed")
        }
        KeyCheck::Initialised => {
            anyhow::bail!("database has no key check value; start the app once first")
        }
        KeyCheck::Rebound => unreachable!("rebind is disabled for rotate-key"),
    }

    // New key: your own (M3USORT_NEW_KEY, random key or passphrase) or a generated one.
    let own = std::env::var("M3USORT_NEW_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if let Some(n) = &own {
        anyhow::ensure!(
            n.trim() != raw.trim(),
            "M3USORT_NEW_KEY is the same as M3USORT_KEY"
        );
    }
    let new_value = own.clone().unwrap_or_else(crypto::generate_key);
    let new_salt = crypto::random_bytes::<{ crypto::SALT_LEN }>();
    let new = {
        let v = new_value.clone();
        tokio::task::spawn_blocking(move || MasterKey::derive(&v, &new_salt)).await??
    };
    let n = db::rotate_key(&pool, &old, &new, &new_salt).await?;
    pool.close().await;

    println!("Re-encrypted {n} credential(s).\n");
    if own.is_some() {
        println!("Now set M3USORT_KEY in .env to the value you gave as M3USORT_NEW_KEY,");
        println!("and remove the M3USORT_NEW_KEY line.\n");
    } else {
        println!("Replace the M3USORT_KEY line in .env with:\n");
        println!("M3USORT_KEY={new_value}\n");
    }
    println!(
        "Then recreate the container (docker compose up -d). Backups made before now still need the OLD key."
    );
    Ok(())
}

/// Forgotten admin password: set a random one. Works while the app runs
/// (`docker exec`), needs no M3USORT_KEY: only someone on the Docker host can run it.
async fn reset_admin() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    anyhow::ensure!(
        cfg.db_path().exists(),
        "no database at {}",
        cfg.db_path().display()
    );
    let pool = db::open(&cfg.db_path(), false).await?;
    anyhow::ensure!(
        settings::setup_completed(&pool).await?,
        "setup is not finished yet; use the setup wizard"
    );
    let password = auth::random_code(16);
    let hash = setup::hash_off_thread(password.clone()).await;
    let mut tx = pool.begin().await?;
    settings::set(&mut *tx, settings::ADMIN_PASSWORD_HASH, &hash).await?;
    settings::bump_session_generation(&mut *tx).await?;
    tx.commit().await?;
    pool.close().await;
    println!("New admin password: {password}");
    println!("All admin sessions are logged out. Change it under Admin after logging in.");
    Ok(())
}

async fn open_existing_db() -> anyhow::Result<sqlx::SqlitePool> {
    let cfg = Config::from_env()?;
    anyhow::ensure!(
        cfg.db_path().exists(),
        "no database at {}",
        cfg.db_path().display()
    );
    db::open(&cfg.db_path(), false).await
}

/// Clear a login lockout. Works while the app runs (`docker exec`): lockouts live
/// in the database, and the app checks them there on every attempt.
async fn reset_ip(arg: Option<String>) -> anyhow::Result<()> {
    let arg = arg.context("usage: m3usort reset-ip <ip> | --all   (see: m3usort locked-ips)")?;
    let ip = if arg == "--all" {
        None
    } else {
        Some(
            arg.parse::<std::net::IpAddr>()
                .map_err(|_| anyhow::anyhow!("'{arg}' is not an IP address"))?,
        )
    };
    let pool = open_existing_db().await?;
    let n = auth::reset_ip(&pool, ip).await?;
    pool.close().await;
    match (ip, n) {
        (Some(ip), 0) => println!("{ip} had no failed logins; nothing to reset."),
        (Some(ip), _) => println!("Lockout and failed-login count cleared for {ip}."),
        (None, n) => println!("Cleared failed logins for {n} IP(s)."),
    }
    Ok(())
}

async fn locked_ips() -> anyhow::Result<()> {
    let pool = open_existing_db().await?;
    let list = auth::locked_ips(&pool).await?;
    pool.close().await;
    if list.is_empty() {
        println!("No IPs are locked out.");
    }
    for (ip, min) in list {
        println!("{ip}\tlocked for {min} more minute(s)");
    }
    Ok(())
}

/// Derive the master key using this database's salt. Argon2id (for passphrases)
/// is CPU/memory heavy, so it runs off the async runtime.
async fn derive_key(raw: &str, pool: &sqlx::SqlitePool) -> anyhow::Result<MasterKey> {
    let salt = db::kdf_salt(pool).await?;
    let raw = raw.to_string();
    tokio::task::spawn_blocking(move || MasterKey::derive(&raw, &salt)).await?
}

/// In a container, the data folder must be a mounted volume, or the database is
/// lost when the container is recreated. A mount point sits on a different device
/// than its parent folder; if it doesn't, warn loudly.
fn warn_if_not_a_volume(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let in_container = std::path::Path::new("/.dockerenv").exists();
        let parent = dir.parent().unwrap_or(std::path::Path::new("/"));
        if let (true, Ok(d), Ok(p)) = (
            in_container,
            std::fs::metadata(dir),
            std::fs::metadata(parent),
        ) && d.dev() == p.dev()
        {
            warn!(
                "{} is not a mounted volume: the database will be LOST when the container is recreated. \
                 Check the volumes in docker-compose.yml, and remove M3USORT_DATA_DIR from .env.",
                dir.display()
            );
        }
    }
}

/// Exclusive lock on the data dir for as long as the returned file is alive.
/// Stops `rotate-key` from running while the app is up (the running app would keep
/// writing with the old key), and stops two app instances sharing one database.
/// flock works across containers that bind-mount the same host folder.
fn lock_data_dir(cfg: &Config) -> anyhow::Result<std::fs::File> {
    let path = cfg.data_dir.join("m3usort.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "{} is in use by another M3Usort process. Stop the app first (docker compose stop m3usort).",
            cfg.data_dir.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("cannot lock data dir"),
    }
}

async fn healthcheck() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.listen.port()));
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /healthcheck HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf);
    anyhow::ensure!(
        head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200"),
        "unhealthy"
    );
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    info!("shutting down");
}

/// `m3usort restore FILE`: check a backup against M3USORT_KEY and stage it; the
/// app puts it in place at its next start (restart the container).
async fn restore(file: Option<String>, without_credentials: bool) -> anyhow::Result<()> {
    let Some(file) = file else {
        anyhow::bail!("usage: m3usort restore /data/backups/m3usort-backup-….db.gz");
    };
    let cfg = config::Config::from_env()?;
    let key = cfg.key.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "M3USORT_KEY is not set; the backup can only be opened with the key it was made with"
        )
    })?;
    let data = std::fs::read(&file).with_context(|| format!("cannot read {file}"))?;
    let info = backup::stage_restore_in(&cfg.data_dir, &key, data, without_credentials).await?;
    println!(
        "Backup OK: setup {}, {}, {} movie(s) and {} series in the library, {} catalog titles.",
        if info.setup_done {
            "finished"
        } else {
            "not finished"
        },
        if info.without_credentials {
            "made with another key: restored WITHOUT its credentials (enter them again under Admin → Settings)".to_string()
        } else {
            format!("{} stored credential(s)", info.secrets)
        },
        info.library_movies,
        info.library_series,
        info.catalog
    );
    println!(
        "Restart the container to put it in place (the current database is kept as m3usort.db.before-restore-…)."
    );
    Ok(())
}
