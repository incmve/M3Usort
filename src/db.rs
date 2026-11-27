//! SQLite database: opening, migrations, and the key check.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::crypto::MasterKey;

/// Name of the meta row holding a value encrypted with the master key.
/// Decrypting it at startup proves M3USORT_KEY matches this database.
const KEY_CHECK: &str = "key_check";
const KEY_CHECK_PLAINTEXT: &[u8] = b"m3usort-key-check-v1";

pub async fn open(path: &Path, create: bool) -> anyhow::Result<SqlitePool> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(create)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
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
    Mismatch,
}

pub async fn verify_or_init_key(pool: &SqlitePool, key: &MasterKey) -> anyhow::Result<KeyCheck> {
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

    tx.commit().await?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::generate_key;

    async fn temp_db() -> (tempdir::Guard, SqlitePool) {
        let dir = tempdir::Guard::new();
        let pool = open(&dir.path.join("t.db"), true).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn key_check_lifecycle() {
        let (_d, pool) = temp_db().await;
        let a = MasterKey::from_base64(&generate_key()).unwrap();
        let b = MasterKey::from_base64(&generate_key()).unwrap();
        assert!(matches!(
            verify_or_init_key(&pool, &a).await.unwrap(),
            KeyCheck::Initialised
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &a).await.unwrap(),
            KeyCheck::Ok
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &b).await.unwrap(),
            KeyCheck::Mismatch
        ));
    }

    #[tokio::test]
    async fn rotation_moves_everything() {
        let (_d, pool) = temp_db().await;
        let old = MasterKey::from_base64(&generate_key()).unwrap();
        let new = MasterKey::from_base64(&generate_key()).unwrap();
        verify_or_init_key(&pool, &old).await.unwrap();
        sqlx::query("INSERT INTO secrets (name, blob) VALUES (?, ?)")
            .bind("tmdb_api_key")
            .bind(old.encrypt("tmdb_api_key", b"t0k3n"))
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(rotate_key(&pool, &old, &new).await.unwrap(), 1);
        assert!(matches!(
            verify_or_init_key(&pool, &new).await.unwrap(),
            KeyCheck::Ok
        ));
        assert!(matches!(
            verify_or_init_key(&pool, &old).await.unwrap(),
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
