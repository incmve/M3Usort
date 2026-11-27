//! Settings read from the environment (`.env` via docker compose).
//!
//! Only things that must be known before the database can be opened live here.
//! Everything else (provider URL, folders, Jellyfin, TMDB, schedules) is set in
//! the setup wizard and stored in the database.

use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Config {
    /// Raw value of M3USORT_KEY, if set. Parsed later so we can show a helpful page.
    pub key: Option<String>,
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let key = std::env::var("M3USORT_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let data_dir = PathBuf::from(env_or("M3USORT_DATA_DIR", "/data"));
        let port: u16 = env_or("M3USORT_PORT", "5050")
            .parse()
            .map_err(|_| anyhow::anyhow!("M3USORT_PORT must be a number between 1 and 65535"))?;
        Ok(Self {
            key,
            data_dir,
            listen: SocketAddr::from(([0, 0, 0, 0], port)),
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("m3usort.db")
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}
