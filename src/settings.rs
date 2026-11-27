//! Typed access to the `settings` and `secrets` tables.

use sqlx::{Executor, Sqlite, SqlitePool};

use crate::crypto::MasterKey;

// Setting names, in one place so typos are compile errors.
pub const SETUP_COMPLETED: &str = "setup_completed";
pub const ADMIN_PASSWORD_HASH: &str = "admin_password_hash";
pub const PLAYLIST_PASSWORD_HASH: &str = "playlist_password_hash";
/// Bumped on every admin password change/reset; invalidates all admin sessions.
pub const SESSION_GENERATION: &str = "session_generation";
pub const PROVIDER_SERVER: &str = "provider_server";
pub const MOVIES_DIR: &str = "movies_dir";
pub const SERIES_DIR: &str = "series_dir";
pub const JELLYFIN_ENABLED: &str = "jellyfin_enabled";
pub const JELLYFIN_URL: &str = "jellyfin_url";
pub const PLAYLIST_OUTPUT: &str = "playlist_output";
pub const PLAYLIST_REFRESH_HOURS: &str = "playlist_refresh_hours";
pub const CUSTOM_GROUP_TITLE: &str = "custom_group_title";
/// First line of the provider's M3U (#EXTM3U plus attributes like url-tvg), kept for the output.
pub const M3U_HEADER: &str = "m3u_header";
pub const VOD_SCHEDULER_ENABLED: &str = "vod_scheduler_enabled";
pub const VOD_SCAN_HOURS: &str = "vod_scan_hours";
pub const OVERWRITE_MOVIES: &str = "overwrite_movies";
pub const OVERWRITE_SERIES: &str = "overwrite_series";
pub const MATCH_TYPE: &str = "match_type";
pub const TMDB_ENRICH_ENABLED: &str = "tmdb_enrich_enabled";
pub const TMDB_ENRICH_TIME: &str = "tmdb_enrich_time";
pub const CAST_ENRICH_ENABLED: &str = "cast_enrich_enabled";
pub const CAST_ENRICH_TIME: &str = "cast_enrich_time";
pub const BACKUP_ENABLED: &str = "backup_enabled";
pub const BACKUP_TIME: &str = "backup_time";
pub const BACKUP_KEEP: &str = "backup_keep";
pub const SMB_ENABLED: &str = "smb_enabled";
pub const SMB_HOST: &str = "smb_host";
pub const SMB_SHARE: &str = "smb_share";
pub const SMB_PATH: &str = "smb_path";
pub const SMB_USERNAME: &str = "smb_username";

// Secret names (encrypted, see crypto.rs).
pub const SECRET_PROVIDER_USERNAME: &str = "provider_username";
pub const SECRET_PROVIDER_PASSWORD: &str = "provider_password";
/// Old single playlist-URL secret (early test builds). Read once as a fallback, then removed.
pub const LEGACY_SECRET_PROVIDER_URL: &str = "provider_url";
pub const SECRET_JELLYFIN_API_KEY: &str = "jellyfin_api_key";
pub const SECRET_TMDB_API_KEY: &str = "tmdb_api_key";
pub const SECRET_SMB_PASSWORD: &str = "smb_password";
pub const ALL_SECRETS: &[(&str, &str)] = &[
    (SECRET_PROVIDER_USERNAME, "Provider / proxy username"),
    (SECRET_PROVIDER_PASSWORD, "Provider / proxy password"),
    (SECRET_JELLYFIN_API_KEY, "Jellyfin API key"),
    (SECRET_TMDB_API_KEY, "TMDB API key"),
    (SECRET_SMB_PASSWORD, "SMB password"),
];

pub async fn get(pool: &SqlitePool, name: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

pub async fn get_all(pool: &SqlitePool) -> anyhow::Result<Vec<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT name, value FROM settings ORDER BY name")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn set<'e, E>(ex: E, name: &str, value: &str) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        "INSERT INTO settings (name, value) VALUES (?, ?)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value,
         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(name)
    .bind(value)
    .execute(ex)
    .await?;
    Ok(())
}

pub async fn setup_completed(pool: &SqlitePool) -> anyhow::Result<bool> {
    Ok(get(pool, SETUP_COMPLETED).await?.as_deref() == Some("1"))
}

pub async fn session_generation(pool: &SqlitePool) -> anyhow::Result<i64> {
    Ok(get(pool, SESSION_GENERATION)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// Increment the session generation: every existing admin session becomes invalid.
pub async fn bump_session_generation<'e, E>(ex: E) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        "INSERT INTO settings (name, value) VALUES (?, '1')
         ON CONFLICT(name) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT),
         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(SESSION_GENERATION)
    .execute(ex)
    .await?;
    Ok(())
}

pub async fn set_secret<'e, E>(
    ex: E,
    key: &MasterKey,
    name: &str,
    value: &str,
) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        "INSERT INTO secrets (name, blob) VALUES (?, ?)
         ON CONFLICT(name) DO UPDATE SET blob = excluded.blob,
         updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(name)
    .bind(key.encrypt(name, value.as_bytes()))
    .execute(ex)
    .await?;
    Ok(())
}

pub async fn delete_secret<'e, E>(ex: E, name: &str) -> anyhow::Result<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    sqlx::query("DELETE FROM secrets WHERE name = ?")
        .bind(name)
        .execute(ex)
        .await?;
    Ok(())
}

pub async fn get_secret(
    pool: &SqlitePool,
    key: &MasterKey,
    name: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT blob FROM secrets WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    match row {
        None => Ok(None),
        Some((blob,)) => {
            let plain = key
                .decrypt(name, &blob)
                .map_err(|_| anyhow::anyhow!("secret '{name}' cannot be decrypted"))?;
            Ok(Some(String::from_utf8(plain)?))
        }
    }
}
