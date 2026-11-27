//! Backups and restore.
//!
//! A backup is a consistent copy of the whole database (SQLite VACUUM INTO, safe
//! while the app runs), gzip-compressed: <data>/backups/m3usort-backup-
//! YYYY-MM-DD_HHMM.db.gz. Credentials stay encrypted inside, so a backup is
//! useless without M3USORT_KEY (and a restore needs that same key).
//! - daily at a set local time (Settings → Backups), keeping the newest N;
//! - optionally copied to an SMB share with the same retention (as Python);
//! - restore: upload or `m3usort restore FILE`. The file is checked (M3Usort
//!   database, not from a newer version, opens with the current key) and staged
//!   as <data>/restore-pending.db; at the next start it replaces the database.
//!   The old database is kept as m3usort.db.before-restore-<time>.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::now_secs;
use crate::settings as s;
use crate::web::AppState;

const PREFIX: &str = "m3usort-backup-";
const SUFFIX: &str = ".db.gz";
pub const PENDING: &str = "restore-pending.db";
const BEFORE_RESTORE: &str = "m3usort.db.before-restore-";
/// Old databases kept after restores.
const KEEP_BEFORE_RESTORE: usize = 3;
const LAST_KEY: &str = "backup_last";

pub const DEFAULT_TIME: &str = "02:30";
pub const DEFAULT_KEEP: u32 = 7;

struct Paths {
    data: PathBuf,
    key_input: String,
}

static PATHS: OnceLock<Paths> = OnceLock::new();

/// Called by `serve` with the data folder and the raw M3USORT_KEY (to check
/// uploaded backups against it).
pub fn init(data_dir: &Path, key_input: &str) {
    let _ = PATHS.set(Paths {
        data: data_dir.to_path_buf(),
        key_input: key_input.to_string(),
    });
}

pub fn dir() -> Option<PathBuf> {
    PATHS.get().map(|p| p.data.join("backups"))
}

pub fn is_backup_name(name: &str) -> bool {
    name.starts_with(PREFIX)
        && name.ends_with(SUFFIX)
        && name.len() < 80
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

#[derive(Clone, Serialize)]
pub struct Local {
    pub name: String,
    pub size: String,
    pub when: String,
}

/// Local backups, newest first.
pub fn list() -> Vec<Local> {
    let Some(d) = dir() else { return Vec::new() };
    let mut out: Vec<(String, u64)> = std::fs::read_dir(&d)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            is_backup_name(&n).then(|| (n, e.metadata().map(|m| m.len()).unwrap_or(0)))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter()
        .map(|(name, size)| Local {
            when: when_of(&name),
            size: crate::files::human(size),
            name,
        })
        .collect()
}

/// "2026-11-27 18:30" from "m3usort-backup-2026-11-27_1830.db.gz".
fn when_of(name: &str) -> String {
    let core = name.trim_start_matches(PREFIX).trim_end_matches(SUFFIX);
    match core.split_once('_') {
        Some((d, t)) if t.len() >= 4 => format!("{d} {}:{}", &t[..2], &t[2..4]),
        _ => core.to_string(),
    }
}

/// Result of the last backup, shown on the Backups page.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Last {
    pub at: i64,
    pub ok: bool,
    pub file: String,
    pub size: String,
    pub smb: String,
    pub message: String,
}

pub async fn last(db: &sqlx::SqlitePool) -> Option<Last> {
    let raw = s::get(db, LAST_KEY).await.ok().flatten()?;
    serde_json::from_str(&raw).ok()
}

async fn keep_count(db: &sqlx::SqlitePool) -> usize {
    s::get(db, s::BACKUP_KEEP)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| (1..=365).contains(n))
        .unwrap_or(DEFAULT_KEEP as usize)
}

/// Make a backup now: local file, retention, SMB copy when enabled.
pub async fn run(st: &AppState, why: &str) -> Last {
    let mut last = Last {
        at: now_secs(),
        ..Default::default()
    };
    match create(st).await {
        Ok((path, size)) => {
            last.ok = true;
            last.file = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            last.size = crate::files::human(size);
            let keep = keep_count(&st.db).await;
            let removed = prune_local(keep);
            last.message = format!(
                "Backup {} ({}){}",
                last.file,
                last.size,
                if removed > 0 {
                    format!("; {removed} old one(s) removed")
                } else {
                    String::new()
                }
            );
            match smb_settings(st).await {
                Ok(Some(cfg)) => match smb_upload(&cfg, &path, keep).await {
                    Ok(note) => last.smb = note,
                    Err(e) => {
                        last.ok = false;
                        last.smb = format!("SMB copy failed: {e}");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    last.ok = false;
                    last.smb = format!("SMB copy failed: {e}");
                }
            }
        }
        Err(e) => last.message = format!("Backup failed: {e:#}"),
    }
    if last.ok {
        tracing::info!(
            "backup ({why}): {}{}",
            last.message,
            if last.smb.is_empty() {
                String::new()
            } else {
                format!("; {}", last.smb)
            }
        );
    } else {
        tracing::warn!("backup ({why}): {} {}", last.message, last.smb);
    }
    if let Ok(json) = serde_json::to_string(&last) {
        let _ = s::set(&st.db, LAST_KEY, &json).await;
    }
    last
}

/// VACUUM INTO a temp file, then gzip it next to it. Returns (path, bytes).
async fn create(st: &AppState) -> anyhow::Result<(PathBuf, u64)> {
    let dir = dir().ok_or_else(|| anyhow::anyhow!("backup folder unknown"))?;
    tokio::fs::create_dir_all(&dir).await?;
    let stamp = crate::localtime::now()
        .strftime("%Y-%m-%d_%H%M")
        .to_string();
    let tmp = dir.join(format!(".tmp-{stamp}-{}.db", crate::auth::random_code(6)));
    sqlx::query("VACUUM INTO ?")
        .bind(tmp.to_string_lossy().to_string())
        .execute(&st.db)
        .await?;
    let mut name = format!("{PREFIX}{stamp}{SUFFIX}");
    let mut n = 1;
    while dir.join(&name).exists() {
        n += 1;
        name = format!("{PREFIX}{stamp}-{n}{SUFFIX}");
    }
    let out = dir.join(&name);
    let (t2, o2) = (tmp.clone(), out.clone());
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let mut input = std::fs::File::open(&t2)?;
        let part = o2.with_extension("gz.part");
        let f = std::fs::File::create(&part)?;
        let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        std::io::copy(&mut input, &mut gz)?;
        gz.finish()?.sync_all()?;
        std::fs::rename(&part, &o2)?;
        Ok(std::fs::metadata(&o2)?.len())
    })
    .await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok((out, res?))
}

fn prune_local(keep: usize) -> usize {
    let Some(d) = dir() else { return 0 };
    let mut removed = 0;
    for b in list().into_iter().skip(keep) {
        if std::fs::remove_file(d.join(&b.name)).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub fn delete_local(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(is_backup_name(name), "invalid backup name");
    let d = dir().ok_or_else(|| anyhow::anyhow!("backup folder unknown"))?;
    std::fs::remove_file(d.join(name))?;
    Ok(())
}

pub fn local_path(name: &str) -> Option<PathBuf> {
    is_backup_name(name)
        .then(|| dir().map(|d| d.join(name)))
        .flatten()
}

/// Daily backup at the set local time (default on, 02:30).
pub async fn maybe_run_daily(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done() {
        return Ok(());
    }
    let enabled = s::get(&st.db, s::BACKUP_ENABLED)
        .await?
        .is_none_or(|v| v == "1");
    if !enabled {
        return Ok(());
    }
    let at = s::get(&st.db, s::BACKUP_TIME)
        .await?
        .unwrap_or_else(|| DEFAULT_TIME.into());
    if crate::localtime::hh_mm() < at {
        return Ok(());
    }
    if last(&st.db)
        .await
        .is_some_and(|l| crate::localtime::date(l.at) == crate::localtime::today())
    {
        return Ok(());
    }
    run(st, "daily").await;
    Ok(())
}

// ------------------------------------------------------------------ SMB

pub struct SmbConfig {
    pub host: String,
    pub share: String,
    pub path: String,
    pub username: String,
    pub password: String,
}

async fn smb_settings(st: &AppState) -> anyhow::Result<Option<SmbConfig>> {
    if s::get(&st.db, s::SMB_ENABLED).await?.as_deref() != Some("1") {
        return Ok(None);
    }
    let g = |k: &'static str| {
        let db = st.db.clone();
        async move { s::get(&db, k).await.ok().flatten().unwrap_or_default() }
    };
    Ok(Some(SmbConfig {
        host: g(s::SMB_HOST).await,
        share: g(s::SMB_SHARE).await,
        path: g(s::SMB_PATH).await,
        username: g(s::SMB_USERNAME).await,
        password: s::get_secret(&st.db, &st.key, s::SECRET_SMB_PASSWORD)
            .await?
            .unwrap_or_default(),
    }))
}

/// "nas" → "nas:445"; "nas:1445" stays.
fn smb_addr(host: &str) -> String {
    let h = host
        .trim()
        .trim_start_matches("smb://")
        .trim_start_matches(r"\\")
        .trim_end_matches(['/', '\\']);
    if h.rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
        && !h.starts_with('[')
    {
        h.to_string()
    } else {
        format!("{h}:445")
    }
}

/// Folder inside the share with forward slashes, no leading/trailing slash.
fn smb_dir(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

async fn smb_connect(cfg: &SmbConfig) -> anyhow::Result<(smb2::SmbClient, smb2::Tree)> {
    anyhow::ensure!(
        !cfg.host.trim().is_empty() && !cfg.share.trim().is_empty(),
        "SMB host or share not set"
    );
    let mut client = tokio::time::timeout(
        Duration::from_secs(20),
        smb2::connect(&smb_addr(&cfg.host), &cfg.username, &cfg.password),
    )
    .await
    .map_err(|_| anyhow::anyhow!("no answer from {} within 20 s", smb_addr(&cfg.host)))?
    .map_err(|e| anyhow::anyhow!("cannot log in to {}: {e}", cfg.host))?;
    let share = client
        .connect_share(cfg.share.trim().trim_matches(['/', '\\']))
        .await
        .map_err(|e| anyhow::anyhow!("cannot open share '{}': {e}", cfg.share))?;
    Ok((client, share))
}

/// Create the folder path inside the share one level at a time (existing is fine).
async fn smb_mkdirs(client: &mut smb2::SmbClient, share: &smb2::Tree, dir: &str) {
    let mut acc = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let _ = client.create_directory(share, &acc).await;
    }
}

/// Upload a backup and keep the newest `keep` there too. Returns a short note.
async fn smb_upload(cfg: &SmbConfig, file: &Path, keep: usize) -> anyhow::Result<String> {
    let data = tokio::fs::read(file).await?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (mut client, mut share) = smb_connect(cfg).await?;
    let dir = smb_dir(&cfg.path);
    smb_mkdirs(&mut client, &share, &dir).await;
    let remote = if dir.is_empty() {
        name.clone()
    } else {
        format!("{dir}/{name}")
    };
    client
        .write_file(&share, &remote, &data)
        .await
        .map_err(|e| anyhow::anyhow!("cannot write {remote}: {e}"))?;
    // Retention only after a confirmed write (as Python).
    let mut removed = 0;
    let list_path = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    if let Ok(entries) = client.list_directory(&mut share, &list_path).await {
        let mut names: Vec<String> = entries
            .into_iter()
            .filter(|e| !e.is_directory && is_backup_name(&e.name))
            .map(|e| e.name)
            .collect();
        names.sort_by(|a, b| b.cmp(a));
        for old in names.into_iter().skip(keep) {
            let p = if dir.is_empty() {
                old.clone()
            } else {
                format!("{dir}/{old}")
            };
            if client.delete_file(&share, &p).await.is_ok() {
                removed += 1;
            }
        }
    }
    let _ = client.disconnect_share(&share).await;
    Ok(format!(
        "copied to \\\\{}\\{}\\{}{}",
        cfg.host.trim(),
        cfg.share.trim(),
        remote.replace('/', "\\"),
        if removed > 0 {
            format!(" ({removed} old one(s) removed there)")
        } else {
            String::new()
        }
    ))
}

/// Settings test: log in, open the share, write and delete a small file.
pub async fn smb_test(cfg: &SmbConfig) -> Result<String, String> {
    let run = async {
        let (mut client, share) = smb_connect(cfg).await?;
        let dir = smb_dir(&cfg.path);
        smb_mkdirs(&mut client, &share, &dir).await;
        let probe = if dir.is_empty() {
            ".m3usort-test".to_string()
        } else {
            format!("{dir}/.m3usort-test")
        };
        client
            .write_file(&share, &probe, b"m3usort")
            .await
            .map_err(|e| anyhow::anyhow!("can log in, but cannot write to the folder: {e}"))?;
        let _ = client.delete_file(&share, &probe).await;
        let _ = client.disconnect_share(&share).await;
        anyhow::Ok(format!(
            "SMB OK: wrote a test file to \\\\{}\\{}\\{}",
            cfg.host.trim(),
            cfg.share.trim(),
            dir.replace('/', "\\")
        ))
    };
    run.await.map_err(|e| format!("{e:#}"))
}

pub fn smb_config(
    host: &str,
    share: &str,
    path: &str,
    username: &str,
    password: &str,
) -> SmbConfig {
    SmbConfig {
        host: host.into(),
        share: share.into(),
        path: path.into(),
        username: username.into(),
        password: password.into(),
    }
}

// ------------------------------------------------------------------ restore

/// Check an uploaded/selected backup and stage it for the next start.
/// Accepts .db.gz (our backups) and plain .db files. With `without_credentials`,
/// a backup made with another key is accepted: its stored credentials are
/// removed (enter them again under Admin → Settings).
pub async fn stage_restore(
    data: Vec<u8>,
    without_credentials: bool,
) -> anyhow::Result<crate::db::BackupInfo> {
    let p = PATHS.get().ok_or_else(|| anyhow::anyhow!("not ready"))?;
    stage_restore_in(&p.data, &p.key_input, data, without_credentials).await
}

pub async fn stage_restore_in(
    data_dir: &Path,
    key_input: &str,
    data: Vec<u8>,
    without_credentials: bool,
) -> anyhow::Result<crate::db::BackupInfo> {
    anyhow::ensure!(!data.is_empty(), "empty file");
    let tmp = data_dir.join(format!("{PENDING}.tmp"));
    let t2 = tmp.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut out = std::fs::File::create(&t2)?;
        if data.starts_with(&[0x1f, 0x8b]) {
            let mut gz = flate2::read::GzDecoder::new(&data[..]);
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = gz
                    .read(&mut buf)
                    .map_err(|e| anyhow::anyhow!("not a valid .gz file: {e}"))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
        } else {
            out.write_all(&data)?;
        }
        out.sync_all()?;
        Ok(())
    })
    .await??;
    let mut header = [0u8; 16];
    let is_sqlite = std::fs::File::open(&tmp)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && &header == b"SQLite format 3\0";
    if !is_sqlite {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("this is not an M3Usort backup (.db.gz or .db)");
    }
    let checked = match crate::db::check_backup(&tmp, key_input, without_credentials).await {
        Ok(info) if info.without_credentials => {
            crate::db::strip_credentials(&tmp).await.map(|()| info)
        }
        other => other,
    };
    match checked {
        Ok(info) => {
            std::fs::rename(&tmp, data_dir.join(PENDING))?;
            tracing::info!("restore staged: a backup replaces the database at the next start");
            Ok(info)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// At start, before the database is opened: put a staged backup in place.
/// The current database (with its -wal/-shm) is kept as before-restore-<time>.
pub fn apply_pending(data_dir: &Path, db_path: &Path) -> anyhow::Result<bool> {
    let pending = data_dir.join(PENDING);
    if !pending.exists() {
        return Ok(false);
    }
    let stamp = crate::localtime::now()
        .strftime("%Y%m%d-%H%M%S")
        .to_string();
    let keep = data_dir.join(format!("{BEFORE_RESTORE}{stamp}"));
    if db_path.exists() {
        std::fs::rename(db_path, &keep)?;
        for ext in ["-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{ext}", db_path.display()));
            if from.exists() {
                std::fs::rename(&from, PathBuf::from(format!("{}{ext}", keep.display())))?;
            }
        }
    }
    std::fs::rename(&pending, db_path)?;
    // Keep only the newest few old databases.
    let mut old: Vec<PathBuf> = std::fs::read_dir(data_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().map(|n| n.to_string_lossy()).is_some_and(|n| {
                n.starts_with(BEFORE_RESTORE) && !n.ends_with("-wal") && !n.ends_with("-shm")
            })
        })
        .collect();
    old.sort();
    old.reverse();
    for p in old.into_iter().skip(KEEP_BEFORE_RESTORE) {
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(format!("{}-wal", p.display()));
        let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(is_backup_name("m3usort-backup-2026-11-27_1830.db.gz"));
        assert!(is_backup_name("m3usort-backup-2026-11-27_1830-2.db.gz"));
        assert!(!is_backup_name("../m3usort-backup-x.db.gz"));
        assert!(!is_backup_name("m3usort.db"));
        assert_eq!(
            when_of("m3usort-backup-2026-11-27_1830.db.gz"),
            "2026-11-27 18:30"
        );
    }

    #[test]
    fn smb_bits() {
        assert_eq!(smb_addr("nas"), "nas:445");
        assert_eq!(smb_addr("192.168.0.5:1445"), "192.168.0.5:1445");
        assert_eq!(smb_addr(r"\\nas\"), "nas:445");
        assert_eq!(smb_dir(r"\backups\m3usort\"), "backups/m3usort");
        assert_eq!(smb_dir(""), "");
    }

    /// Full cycle: database → backup file → staged restore with the right key,
    /// refused with a wrong key → applied at "start".
    #[tokio::test]
    async fn backup_and_restore_cycle() {
        let dir = std::env::temp_dir().join(format!("m3usort-bk-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("m3usort.db");
        let pool = crate::db::open(&db_path, true).await.unwrap();
        let salt = crate::db::kdf_salt(&pool).await.unwrap();
        let key = crate::crypto::MasterKey::derive("correct horse battery", &salt).unwrap();
        crate::db::verify_or_init_key(&pool, &key, false)
            .await
            .unwrap();
        s::set(&pool, s::SETUP_COMPLETED, "1").await.unwrap();
        s::set_secret(&pool, &key, s::SECRET_PROVIDER_PASSWORD, "pw")
            .await
            .unwrap();
        // A backup the way create() makes one.
        let copy = dir.join("copy.db");
        sqlx::query("VACUUM INTO ?")
            .bind(copy.to_string_lossy().to_string())
            .execute(&pool)
            .await
            .unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&std::fs::read(&copy).unwrap()).unwrap();
        let backup = gz.finish().unwrap();
        // Change the live database after the backup.
        s::set(&pool, "marker", "after-backup").await.unwrap();
        pool.close().await;

        let wrong = stage_restore_in(&dir, "another key", backup.clone(), false)
            .await
            .unwrap_err();
        assert!(
            wrong.to_string().contains("different M3USORT_KEY"),
            "{wrong}"
        );
        assert!(!dir.join(PENDING).exists());
        let junk = stage_restore_in(&dir, "correct horse battery", b"hello".to_vec(), false)
            .await
            .unwrap_err();
        assert!(junk.to_string().contains("not an M3Usort backup"), "{junk}");

        // Another key, explicitly without credentials: accepted, secrets removed.
        let other = stage_restore_in(&dir, "another key", backup.clone(), true)
            .await
            .unwrap();
        assert!(other.without_credentials);
        std::fs::remove_file(dir.join(PENDING)).unwrap();

        let info = stage_restore_in(&dir, "correct horse battery", backup, false)
            .await
            .unwrap();
        assert!(!info.without_credentials);
        assert!(info.setup_done);
        assert_eq!(info.secrets, 1);
        assert!(apply_pending(&dir, &db_path).unwrap());
        assert!(!dir.join(PENDING).exists());
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(BEFORE_RESTORE))
            .count();
        assert!(kept >= 1);
        let pool = crate::db::open(&db_path, false).await.unwrap();
        assert_eq!(
            s::get(&pool, "marker").await.unwrap(),
            None,
            "restored database is the backup"
        );
        assert_eq!(
            s::get_secret(&pool, &key, s::SECRET_PROVIDER_PASSWORD)
                .await
                .unwrap()
                .as_deref(),
            Some("pw")
        );
        pool.close().await;
        std::fs::remove_dir_all(dir).ok();
    }
}
