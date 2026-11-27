//! SQLite database: opening, migrations, and the key check.

use std::path::Path;
use std::str::FromStr;

use sqlx::ConnectOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::crypto::{self, MasterKey, SALT_LEN};

/// Name of the meta row holding a value encrypted with the master key.
/// Decrypting it at startup proves M3USORT_KEY matches this database.
const KEY_CHECK: &str = "key_check";
const KEY_CHECK_PLAINTEXT: &[u8] = b"m3usort-key-check-v1";
/// Salt for stretching a passphrase M3USORT_KEY with Argon2id. Not secret.
const KDF_SALT: &str = "kdf_salt";

/// The Argon2id salt for this database, created on first use.
pub async fn kdf_salt(pool: &SqlitePool) -> anyhow::Result<Vec<u8>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT value FROM meta WHERE name = ?")
        .bind(KDF_SALT)
        .fetch_optional(pool)
        .await?;
    if let Some((salt,)) = row {
        return Ok(salt);
    }
    let salt = crypto::random_bytes::<SALT_LEN>().to_vec();
    sqlx::query("INSERT INTO meta (name, value) VALUES (?, ?)")
        .bind(KDF_SALT)
        .bind(&salt)
        .execute(pool)
        .await?;
    Ok(salt)
}

pub async fn open(path: &Path, create: bool) -> anyhow::Result<SqlitePool> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(create)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        // SQLx would dump a whole bulk INSERT (thousands of placeholders) into the log
        // when it takes > 1 s. Slow steps are timed and logged by the app itself.
        .log_slow_statements(log::LevelFilter::Off, std::time::Duration::from_secs(60));
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub enum KeyCheck {
    /// Fresh database: check value has just been written.
    Initialised,
    Ok,
    /// Key differed, but nothing was encrypted yet (setup not finished), so the
    /// database has been re-bound to the new key. Only when `allow_rebind` is set.
    Rebound,
    Mismatch,
}

/// Number of encrypted credentials. Zero means setup was never finished: the
/// wizard saves all credentials in one transaction at Finish.
async fn secret_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM secrets")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn verify_or_init_key(
    pool: &SqlitePool,
    key: &MasterKey,
    allow_rebind: bool,
) -> anyhow::Result<KeyCheck> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT value FROM meta WHERE name = ?")
        .bind(KEY_CHECK)
        .fetch_optional(pool)
        .await?;
    match row {
        None => {
            let blob = key.encrypt(KEY_CHECK, KEY_CHECK_PLAINTEXT);
            sqlx::query("INSERT INTO meta (name, value) VALUES (?, ?)")
                .bind(KEY_CHECK)
                .bind(blob)
                .execute(pool)
                .await?;
            Ok(KeyCheck::Initialised)
        }
        Some((blob,)) => match key.decrypt(KEY_CHECK, &blob) {
            Ok(pt) if pt == KEY_CHECK_PLAINTEXT => Ok(KeyCheck::Ok),
            _ if allow_rebind && secret_count(pool).await? == 0 => {
                sqlx::query("UPDATE meta SET value = ? WHERE name = ?")
                    .bind(key.encrypt(KEY_CHECK, KEY_CHECK_PLAINTEXT))
                    .bind(KEY_CHECK)
                    .execute(pool)
                    .await?;
                Ok(KeyCheck::Rebound)
            }
            _ => Ok(KeyCheck::Mismatch),
        },
    }
}

/// Re-encrypt every secret and the key check with `new`, in one transaction.
/// Either everything moves to the new key or nothing changes.
pub async fn rotate_key(
    pool: &SqlitePool,
    old: &MasterKey,
    new: &MasterKey,
    new_salt: &[u8],
) -> anyhow::Result<usize> {
    let mut tx = pool.begin().await?;

    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT name, blob FROM secrets")
        .fetch_all(&mut *tx)
        .await?;
    for (name, blob) in &rows {
        let plain = old.decrypt(name, blob).map_err(|_| {
            anyhow::anyhow!("secret '{name}' cannot be decrypted with the current key")
        })?;
        sqlx::query("UPDATE secrets SET blob = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE name = ?")
            .bind(new.encrypt(name, &plain))
            .bind(name)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("UPDATE meta SET value = ? WHERE name = ?")
        .bind(new.encrypt(KEY_CHECK, KEY_CHECK_PLAINTEXT))
        .bind(KEY_CHECK)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO meta (name, value) VALUES (?, ?) ON CONFLICT(name) DO UPDATE SET value = excluded.value")
        .bind(KDF_SALT)
        .bind(new_salt)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::generate_key;

    fn k() -> MasterKey {
        MasterKey::derive(&generate_key(), b"0123456789abcdef").unwrap()
    }

    #[tokio::test]
    async fn salt_is_created_once() {
        let (_d, pool) = temp_db().await;
        let a = kdf_salt(&pool).await.unwrap();
        assert_eq!(a.len(), SALT_LEN);
        assert_eq!(kdf_salt(&pool).await.unwrap(), a);
    }

    async fn temp_db() -> (tempdir::Guard, SqlitePool) {
        let dir = tempdir::Guard::new();
        let pool = open(&dir.path.join("t.db"), true).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn key_check_lifecycle() {
        let (_d, pool) = temp_db().await;
        let a = k();
        let b = k();
        assert!(matches!(
            verify_or_init_key(&pool, &a, false).await.unwrap(),
            KeyCheck::Initialised
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &a, false).await.unwrap(),
            KeyCheck::Ok
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &b, false).await.unwrap(),
            KeyCheck::Mismatch
        ));
    }

    #[tokio::test]
    async fn rebind_only_while_no_secrets() {
        let (_d, pool) = temp_db().await;
        let a = k();
        let b = k();
        let c = k();
        verify_or_init_key(&pool, &a, true).await.unwrap();
        // Setup not finished: switching key is allowed and re-binds.
        assert!(matches!(
            verify_or_init_key(&pool, &b, true).await.unwrap(),
            KeyCheck::Rebound
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &b, true).await.unwrap(),
            KeyCheck::Ok
        ));
        // Once a credential exists, a different key is a hard mismatch.
        sqlx::query("INSERT INTO secrets (name, blob) VALUES ('x', ?)")
            .bind(b.encrypt("x", b"v"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            verify_or_init_key(&pool, &c, true).await.unwrap(),
            KeyCheck::Mismatch
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &b, true).await.unwrap(),
            KeyCheck::Ok
        ));
    }

    #[tokio::test]
    async fn rotation_moves_everything() {
        let (_d, pool) = temp_db().await;
        let old = k();
        let new = k();
        verify_or_init_key(&pool, &old, false).await.unwrap();
        sqlx::query("INSERT INTO secrets (name, blob) VALUES (?, ?)")
            .bind("tmdb_api_key")
            .bind(old.encrypt("tmdb_api_key", b"t0k3n"))
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            rotate_key(&pool, &old, &new, b"newsalt-newsalt!")
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            verify_or_init_key(&pool, &new, false).await.unwrap(),
            KeyCheck::Ok
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &old, false).await.unwrap(),
            KeyCheck::Mismatch
        ));
        let (blob,): (Vec<u8>,) =
            sqlx::query_as("SELECT blob FROM secrets WHERE name = 'tmdb_api_key'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(new.decrypt("tmdb_api_key", &blob).unwrap(), b"t0k3n");
    }

    /// Minimal temp dir helper, avoids an extra dev-dependency.
    mod tempdir {
        pub struct Guard {
            pub path: std::path::PathBuf,
        }
        impl Guard {
            pub fn new() -> Self {
                let mut n = [0u8; 8];
                getrandom::fill(&mut n).unwrap();
                let path =
                    std::env::temp_dir().join(format!("m3usort-test-{}", u64::from_le_bytes(n)));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}

/// Remove the stored credentials and the key check from a staged backup that
/// was made with another key. At the next start the current key is bound to it.
pub async fn strip_credentials(path: &Path) -> anyhow::Result<()> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?.log_statements(log::LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM secrets").execute(&mut *tx).await?;
    sqlx::query("DELETE FROM meta WHERE name = ?")
        .bind(KEY_CHECK)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    pool.close().await;
    Ok(())
}

/// What a backup holds, for the restore confirmation.
#[derive(Debug, Clone)]
pub struct BackupInfo {
    /// The key didn't match; restored without the stored credentials.
    pub without_credentials: bool,
    pub setup_done: bool,
    pub secrets: i64,
    pub library_movies: i64,
    pub library_series: i64,
    pub catalog: i64,
}

/// Check a database file (a backup) without changing it: it must be an M3Usort
/// database, made by this or an older version (newer migrations can't be
/// opened), and `key_input` (M3USORT_KEY) must decrypt it.
/// With `allow_other_key`, a backup made with another key is accepted too
/// (`without_credentials`); the caller must then remove its secrets.
pub async fn check_backup(
    path: &Path,
    key_input: &str,
    allow_other_key: bool,
) -> anyhow::Result<BackupInfo> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .read_only(true)
        .log_statements(log::LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(|e| anyhow::anyhow!("not a database file: {e}"))?;
    let result = check_backup_pool(&pool, key_input, allow_other_key).await;
    pool.close().await;
    result
}

async fn check_backup_pool(
    pool: &SqlitePool,
    key_input: &str,
    allow_other_key: bool,
) -> anyhow::Result<BackupInfo> {
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await
            .map_err(|_| anyhow::anyhow!("this is not an M3Usort backup"))?;
    let known: Vec<i64> = sqlx::migrate!("./migrations")
        .iter()
        .map(|m| m.version)
        .collect();
    if let Some(v) = applied.iter().find(|v| !known.contains(v)) {
        anyhow::bail!(
            "this backup was made by a newer M3Usort (database version {v}); update M3Usort first"
        );
    }
    let salt: Option<Vec<u8>> = sqlx::query_scalar("SELECT value FROM meta WHERE name = ?")
        .bind(KDF_SALT)
        .fetch_optional(pool)
        .await?;
    let check: Option<Vec<u8>> = sqlx::query_scalar("SELECT value FROM meta WHERE name = ?")
        .bind(KEY_CHECK)
        .fetch_optional(pool)
        .await?;
    let (Some(salt), Some(check)) = (salt, check) else {
        anyhow::bail!("this backup has no key check (not an M3Usort database, or never started)");
    };
    let input = key_input.to_string();
    let key = tokio::task::spawn_blocking(move || MasterKey::derive(&input, &salt)).await??;
    let without_credentials = match key.decrypt(KEY_CHECK, &check) {
        Ok(pt) if pt == KEY_CHECK_PLAINTEXT => false,
        _ if allow_other_key => true,
        _ => anyhow::bail!(
            "this backup was made with a different M3USORT_KEY. Put the key it was made with in .env, \
             or restore it without its stored credentials (see docs/KEY-MANAGEMENT.md)"
        ),
    };
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    };
    Ok(BackupInfo {
        without_credentials,
        setup_done: count(
            "SELECT COUNT(*) FROM settings WHERE name = 'setup_completed' AND value = '1'",
        )
        .await
            > 0,
        secrets: count("SELECT COUNT(*) FROM secrets").await,
        library_movies: count("SELECT COUNT(*) FROM library_movies").await,
        library_series: count("SELECT COUNT(*) FROM library_series").await,
        catalog: count("SELECT (SELECT COUNT(*) FROM movies) + (SELECT COUNT(*) FROM series)")
            .await,
    })
}
