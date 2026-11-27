//! Version info and the update check.
//!
//! The update check asks GitHub for releases of this repo whose tag starts with
//! `rust-v` (e.g. `rust-v0.2.0`), so it never confuses Python releases with Rust
//! ones. It only reports; it never updates anything. Unauthenticated GitHub API
//! calls are limited to 60 per hour per IP, so it checks once at start (after a
//! short delay) and then every 12 hours. Disable with M3USORT_UPDATE_CHECK=false.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Git commit, baked in at build time by CI (`M3USORT_GIT_SHA`), "dev" otherwise.
pub const COMMIT: &str = match option_env!("M3USORT_GIT_SHA") {
    Some(s) => s,
    None => "dev",
};

const RELEASES_URL: &str = "https://api.github.com/repos/incmve/M3Usort/releases?per_page=30";
const TAG_PREFIX: &str = "rust-v";
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

pub fn short_commit() -> &'static str {
    &COMMIT[..COMMIT.len().min(7)]
}

pub fn version_line() -> String {
    format!("m3usort {VERSION} ({})", short_commit())
}

#[derive(Clone, Default, Serialize)]
pub struct UpdateStatus {
    pub enabled: bool,
    pub latest: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
}

pub type SharedStatus = Arc<RwLock<UpdateStatus>>;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

pub fn enabled_from_env() -> bool {
    !matches!(
        std::env::var("M3USORT_UPDATE_CHECK")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "false" | "0" | "no" | "off"
    )
}

/// Start the background check. Returns the shared status the web layer reads.
pub fn spawn_checker() -> SharedStatus {
    let enabled = enabled_from_env();
    let status: SharedStatus = Arc::new(RwLock::new(UpdateStatus {
        enabled,
        ..Default::default()
    }));
    if !enabled {
        info!("update check disabled (M3USORT_UPDATE_CHECK=false)");
        return status;
    }
    let st = status.clone();
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            let result = check_once().await;
            let mut s = st.write().await;
            s.checked_at = Some(now_utc());
            match result {
                Ok(found) => {
                    s.error = None;
                    let current = semver::Version::parse(VERSION).expect("Cargo version is semver");
                    match found {
                        Some((v, url)) => {
                            s.update_available = v > current;
                            if s.update_available {
                                info!("update available: {VERSION} -> {v} ({url})");
                            } else {
                                debug!("up to date ({VERSION}, latest release {v})");
                            }
                            s.latest = Some(v.to_string());
                            s.release_url = Some(url);
                        }
                        None => debug!("no Rust releases published yet"),
                    }
                }
                Err(e) => {
                    warn!("update check failed: {e:#}");
                    s.error = Some(format!("{e:#}"));
                }
            }
            drop(s);
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
    status
}

async fn check_once() -> anyhow::Result<Option<(semver::Version, String)>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        // GitHub's API rejects requests without a User-Agent.
        .user_agent(format!("m3usort/{VERSION}"))
        .build()?;
    let releases: Vec<Release> = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let current = semver::Version::parse(VERSION)?;
    Ok(pick_latest(&releases, !current.pre.is_empty()))
}

/// Highest `rust-vX.Y.Z` release. Pre-releases only count if we run one ourselves.
fn pick_latest(releases: &[Release], include_pre: bool) -> Option<(semver::Version, String)> {
    releases
        .iter()
        .filter(|r| !r.draft && (include_pre || !r.prerelease))
        .filter_map(|r| {
            let v = semver::Version::parse(r.tag_name.strip_prefix(TAG_PREFIX)?).ok()?;
            Some((v, r.html_url.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
}

fn now_utc() -> String {
    // RFC 3339 without pulling in a date crate.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days (Howard Hinnant), valid for all dates after 1970.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if mo <= 2 { 1 } else { 0 };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, pre: bool) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: format!("https://example/{tag}"),
            draft: false,
            prerelease: pre,
        }
    }

    #[test]
    fn picks_highest_rust_release_only() {
        let r = vec![
            rel("v2.0.6", false), // Python release: ignored
            rel("rust-v0.2.0", false),
            rel("rust-v0.10.0", false), // semver, not string order
            rel("rust-v0.11.0-beta.1", true),
            rel("Stable", false),
        ];
        assert_eq!(pick_latest(&r, false).unwrap().0.to_string(), "0.10.0");
        assert_eq!(
            pick_latest(&r, true).unwrap().0.to_string(),
            "0.11.0-beta.1"
        );
        assert!(pick_latest(&[rel("v1.0.0", false)], false).is_none());
    }

    #[test]
    fn date_format() {
        let s = now_utc();
        assert_eq!(s.len(), 20);
        assert!(s.starts_with("20") && s.ends_with('Z'));
    }
}
