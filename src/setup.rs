//! First-run setup wizard, and the admin "Settings" sections that reuse its steps.
//!
//! Steps: admin → provider → folders → Jellyfin → TMDB → schedules → summary.
//! Only the admin password and the provider/proxy are required; after the provider
//! step you can jump to Finish and set the rest later under Admin → Settings.
//! Every step validates (and tests live connections) before it is accepted.
//! Wizard answers stay in memory until Finish, which saves everything in one
//! transaction. Step 1 requires the one-time setup code from the container log.

use std::collections::HashMap;
use std::net::SocketAddr;

use askama::Template;
use axum::Form;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use sqlx::{Sqlite, Transaction};

use crate::auth::{self, ADMIN_COOKIE, SESSION_TTL};
use crate::integrations::{self, Xtream};
use crate::settings as s;
use crate::web::{AppState, Page};

pub const SETUP_COOKIE: &str = "m3usort_setup";
pub const LAST_STEP: u8 = 7;
/// Provider is the last required step; after it, Finish is allowed.
const REQUIRED_STEPS: u8 = 2;

const SUGGEST_MOVIES: &str = "/data/media/movies";
const SUGGEST_SERIES: &str = "/data/media/tv";

/// Everything the wizard collects. Defaults match the Python version's config.sample.
#[derive(Clone)]
pub struct Draft {
    admin_hash: Option<String>,
    playlist_hash: Option<String>,
    provider_server: String,
    provider_username: String,
    provider_password: String,
    provider_result: String,
    movies_dir: String,
    series_dir: String,
    jellyfin_enabled: bool,
    jellyfin_url: String,
    jellyfin_api_key: String,
    jellyfin_result: String,
    tmdb_api_key: String,
    tmdb_result: String,
    playlist_output: String,
    playlist_refresh_hours: String,
    custom_group_title: String,
    vod_scheduler_enabled: bool,
    vod_scan_hours: String,
    overwrite_movies: bool,
    overwrite_series: bool,
    match_type: String,
    tmdb_enrich_enabled: bool,
    tmdb_enrich_time: String,
    cast_enrich_enabled: bool,
    cast_enrich_time: String,
    backup_enabled: bool,
    backup_time: String,
    backup_keep: String,
    smb_enabled: bool,
    smb_host: String,
    smb_share: String,
    smb_path: String,
    smb_username: String,
    smb_password: String,
    smb_result: String,
    /// Highest step completed, so you can't skip the required ones.
    done: u8,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            admin_hash: None,
            playlist_hash: None,
            provider_server: String::new(),
            provider_username: String::new(),
            provider_password: String::new(),
            provider_result: String::new(),
            movies_dir: String::new(),
            series_dir: String::new(),
            jellyfin_enabled: false,
            jellyfin_url: String::new(),
            jellyfin_api_key: String::new(),
            jellyfin_result: String::new(),
            tmdb_api_key: String::new(),
            tmdb_result: String::new(),
            playlist_output: "sorted.m3u".into(),
            playlist_refresh_hours: "4".into(),
            custom_group_title: "Custom".into(),
            vod_scheduler_enabled: true,
            vod_scan_hours: "10".into(),
            overwrite_movies: false,
            overwrite_series: false,
            match_type: "string".into(),
            tmdb_enrich_enabled: false,
            tmdb_enrich_time: "03:00".into(),
            cast_enrich_enabled: false,
            cast_enrich_time: "04:00".into(),
            backup_enabled: true,
            backup_time: crate::backup::DEFAULT_TIME.into(),
            backup_keep: crate::backup::DEFAULT_KEEP.to_string(),
            smb_enabled: false,
            smb_host: String::new(),
            smb_share: String::new(),
            smb_path: String::new(),
            smb_username: String::new(),
            smb_password: String::new(),
            smb_result: String::new(),
            done: 0,
        }
    }
}

impl Draft {
    fn xtream(&self) -> Xtream {
        Xtream {
            server: self.provider_server.clone(),
            username: self.provider_username.clone(),
            password: self.provider_password.clone(),
        }
    }

    /// Current saved configuration, for editing a section under Admin → Settings.
    pub async fn load(st: &AppState) -> anyhow::Result<Self> {
        let all: HashMap<String, String> = s::get_all(&st.db).await?.into_iter().collect();
        let txt = |k: &str, def: &str| all.get(k).cloned().unwrap_or_else(|| def.to_string());
        let flag = |k: &str| all.get(k).map(String::as_str) == Some("1");
        let secret = |name: &'static str| async move {
            s::get_secret(&st.db, &st.key, name)
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let d = Draft::default();
        // Early test builds stored one playlist URL; split it if that's all there is.
        let legacy = Xtream::from_playlist_url(&secret(s::LEGACY_SECRET_PROVIDER_URL).await);
        let user = secret(s::SECRET_PROVIDER_USERNAME).await;
        let (server, user, pass) = match legacy {
            Some(x) if user.is_empty() => (x.server, x.username, x.password),
            _ => (
                txt(s::PROVIDER_SERVER, ""),
                user,
                secret(s::SECRET_PROVIDER_PASSWORD).await,
            ),
        };
        Ok(Self {
            admin_hash: all.get(s::ADMIN_PASSWORD_HASH).cloned(),
            playlist_hash: all
                .get(s::PLAYLIST_PASSWORD_HASH)
                .cloned()
                .filter(|h| !h.is_empty()),
            provider_server: server,
            provider_username: user,
            provider_password: pass,
            provider_result: String::new(),
            movies_dir: txt(s::MOVIES_DIR, ""),
            series_dir: txt(s::SERIES_DIR, ""),
            jellyfin_enabled: flag(s::JELLYFIN_ENABLED),
            jellyfin_url: txt(s::JELLYFIN_URL, ""),
            jellyfin_api_key: secret(s::SECRET_JELLYFIN_API_KEY).await,
            jellyfin_result: String::new(),
            tmdb_api_key: secret(s::SECRET_TMDB_API_KEY).await,
            tmdb_result: String::new(),
            playlist_output: txt(s::PLAYLIST_OUTPUT, &d.playlist_output),
            playlist_refresh_hours: txt(s::PLAYLIST_REFRESH_HOURS, &d.playlist_refresh_hours),
            custom_group_title: txt(s::CUSTOM_GROUP_TITLE, &d.custom_group_title),
            vod_scheduler_enabled: all
                .get(s::VOD_SCHEDULER_ENABLED)
                .map_or(d.vod_scheduler_enabled, |v| v == "1"),
            vod_scan_hours: txt(s::VOD_SCAN_HOURS, &d.vod_scan_hours),
            overwrite_movies: flag(s::OVERWRITE_MOVIES),
            overwrite_series: flag(s::OVERWRITE_SERIES),
            match_type: txt(s::MATCH_TYPE, &d.match_type),
            tmdb_enrich_enabled: flag(s::TMDB_ENRICH_ENABLED),
            tmdb_enrich_time: txt(s::TMDB_ENRICH_TIME, &d.tmdb_enrich_time),
            cast_enrich_enabled: flag(s::CAST_ENRICH_ENABLED),
            cast_enrich_time: txt(s::CAST_ENRICH_TIME, &d.cast_enrich_time),
            backup_enabled: all
                .get(s::BACKUP_ENABLED)
                .map_or(d.backup_enabled, |v| v == "1"),
            backup_time: txt(s::BACKUP_TIME, &d.backup_time),
            backup_keep: txt(s::BACKUP_KEEP, &d.backup_keep),
            smb_enabled: flag(s::SMB_ENABLED),
            smb_host: txt(s::SMB_HOST, ""),
            smb_share: txt(s::SMB_SHARE, ""),
            smb_path: txt(s::SMB_PATH, ""),
            smb_username: txt(s::SMB_USERNAME, ""),
            smb_password: secret(s::SECRET_SMB_PASSWORD).await,
            smb_result: String::new(),
            done: LAST_STEP,
        })
    }
}

// ------------------------------------------------------------------ form model

pub enum Kind {
    Text,
    Password,
    Number { min: u32, max: u32 },
    Time,
    Checkbox,
    Select(Vec<(&'static str, &'static str)>),
}

pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    pub help: String,
    pub kind: Kind,
    pub value: String,
    pub checked: bool,
}

impl Field {
    fn new(name: &'static str, label: &'static str, help: &str, kind: Kind, value: &str) -> Self {
        Self {
            name,
            label,
            help: help.into(),
            kind,
            value: value.into(),
            checked: false,
        }
    }
    fn text(name: &'static str, label: &'static str, help: &str, value: &str) -> Self {
        Self::new(name, label, help, Kind::Text, value)
    }
    fn password(name: &'static str, label: &'static str, help: &str, value: &str) -> Self {
        Self::new(name, label, help, Kind::Password, value)
    }
    fn number(name: &'static str, label: &'static str, value: &str, min: u32, max: u32) -> Self {
        Self::new(name, label, "", Kind::Number { min, max }, value)
    }
    fn time(name: &'static str, label: &'static str, value: &str) -> Self {
        Self::new(name, label, "", Kind::Time, value)
    }
    fn checkbox(name: &'static str, label: &'static str, help: &str, checked: bool) -> Self {
        Self {
            checked,
            ..Self::new(name, label, help, Kind::Checkbox, "")
        }
    }
    pub fn is_password(&self) -> bool {
        matches!(self.kind, Kind::Password)
    }
    pub fn is_time(&self) -> bool {
        matches!(self.kind, Kind::Time)
    }
    pub fn is_checkbox(&self) -> bool {
        matches!(self.kind, Kind::Checkbox)
    }
    pub fn is_select(&self) -> bool {
        matches!(self.kind, Kind::Select(_))
    }
    pub fn number_range(&self) -> Option<(u32, u32)> {
        match self.kind {
            Kind::Number { min, max } => Some((min, max)),
            _ => None,
        }
    }
    pub fn options(&self) -> Vec<(&'static str, &'static str, bool)> {
        match &self.kind {
            Kind::Select(o) => o.iter().map(|(v, l)| (*v, *l, *v == self.value)).collect(),
            _ => Vec::new(),
        }
    }
}

pub struct StepInfo {
    pub title: &'static str,
    pub intro: &'static str,
}

pub fn step_info(step: u8) -> StepInfo {
    match step {
        1 => StepInfo {
            title: "Admin",
            intro: "Enter the setup code from the container log (Arcane → container → Logs) and choose the admin password. \
                    The playlist password is optional: without it, playlist downloads stay off until you set one under Admin.",
        },
        2 => StepInfo {
            title: "Provider / proxy",
            intro: "The Xtream Codes login of your iptv-proxy (or provider). Required. The login is tested against the Xtream API before you can continue.",
        },
        3 => StepInfo {
            title: "Folders (optional)",
            intro: "Where .strm files are written, as seen inside the container. Filled-in folders must exist and be writable. \
                    Leave empty to set later.",
        },
        4 => StepInfo {
            title: "Jellyfin (optional)",
            intro: "Used to refresh the library after new files and to show what is already in Jellyfin.",
        },
        5 => StepInfo {
            title: "TMDB (optional)",
            intro: "Used for posters, cast and actor search. Free key at themoviedb.org → Settings → API. Leave empty to skip.",
        },
        6 => StepInfo {
            title: "Schedules (optional)",
            intro: "How often playlists and VOD are refreshed. The defaults are fine; you can change them later.",
        },
        _ => StepInfo {
            title: "Backups (optional)",
            intro: "A daily copy of the database (settings, encrypted credentials, library, wanted list) in /data/backups. \
                    Optionally also copied to an SMB share (NAS). Restoring needs the same M3USORT_KEY.",
        },
    }
}

fn fields(step: u8, d: &Draft, in_wizard: bool) -> Vec<Field> {
    let keep = "Already set. Leave empty to keep it.";
    match step {
        1 => vec![
            Field::text(
                "setup_code",
                "Setup code",
                "Shown in the container log as \"Setup code: …\"",
                "",
            ),
            Field::password(
                "admin_password",
                "Admin password",
                if d.admin_hash.is_some() { keep } else { "" },
                "",
            ),
            Field::password("admin_password2", "Admin password (again)", "", ""),
            Field::password(
                "playlist_password",
                "Playlist password (optional)",
                if d.playlist_hash.is_some() {
                    keep
                } else {
                    "IPTV players use this to download your sorted playlist."
                },
                "",
            ),
            Field::password("playlist_password2", "Playlist password (again)", "", ""),
        ],
        2 => vec![
            Field::text(
                "provider_server",
                "Server",
                "Your iptv-proxy (or provider), e.g. http://iptvproxy.local:8080. \
                 Pasting a full playlist URL (…/get.php?username=…&password=…) also works: it is split automatically.",
                &d.provider_server,
            ),
            Field::text("provider_username", "Username", "", &d.provider_username),
            if !in_wizard && !d.provider_password.is_empty() {
                Field::password(
                    "provider_password",
                    "Password",
                    "Set. Leave empty to keep it (it is tested again).",
                    "",
                )
            } else {
                Field::password("provider_password", "Password", "", &d.provider_password)
            },
        ],
        3 => {
            // Suggest the usual mounts on the first visit in the wizard only.
            let first = in_wizard && d.done < 3;
            let m = if first && d.movies_dir.is_empty() {
                SUGGEST_MOVIES
            } else {
                &d.movies_dir
            };
            let t = if first && d.series_dir.is_empty() {
                SUGGEST_SERIES
            } else {
                &d.series_dir
            };
            vec![
                Field::text(
                    "movies_dir",
                    "Movies folder",
                    "Leave empty to set later.",
                    m,
                ),
                Field::text(
                    "series_dir",
                    "Series folder",
                    "Leave empty to set later.",
                    t,
                ),
            ]
        }
        4 => vec![
            Field::checkbox("jellyfin_enabled", "Use Jellyfin", "", d.jellyfin_enabled),
            Field::text(
                "jellyfin_url",
                "Jellyfin URL",
                "e.g. http://192.168.1.10:8096",
                &d.jellyfin_url,
            ),
            if !in_wizard && !d.jellyfin_api_key.is_empty() {
                Field::password(
                    "jellyfin_api_key",
                    "Jellyfin API key",
                    "Set. Leave empty to keep it. Untick \"Use Jellyfin\" to remove it.",
                    "",
                )
            } else {
                Field::password(
                    "jellyfin_api_key",
                    "Jellyfin API key",
                    "Jellyfin → Dashboard → API Keys",
                    &d.jellyfin_api_key,
                )
            },
        ],
        5 if !in_wizard && !d.tmdb_api_key.is_empty() => vec![
            Field::password(
                "tmdb_api_key",
                "TMDB API key",
                "Set. Leave empty to keep it (it is tested again).",
                "",
            ),
            Field::checkbox("tmdb_remove", "Remove the TMDB key", "", false),
        ],
        5 => vec![Field::password(
            "tmdb_api_key",
            "TMDB API key",
            "The \"API Key\" (v3) from themoviedb.org, same as the Python version uses. A v4 read access token also works.",
            &d.tmdb_api_key,
        )],
        6 => vec![
            Field::number(
                "playlist_refresh_hours",
                "Refresh playlist every (hours)",
                &d.playlist_refresh_hours,
                1,
                24,
            ),
            Field::text(
                "playlist_output",
                "Playlist file name",
                "",
                &d.playlist_output,
            ),
            Field::text(
                "custom_group_title",
                "Custom group title",
                "Always the first group in your sorted playlist",
                &d.custom_group_title,
            ),
            Field::checkbox(
                "vod_scheduler_enabled",
                "VOD scheduler",
                "Create .strm files for your wanted movies and series on a schedule",
                d.vod_scheduler_enabled,
            ),
            Field::number(
                "vod_scan_hours",
                "VOD scan every (hours)",
                &d.vod_scan_hours,
                1,
                24,
            ),
            Field::checkbox(
                "overwrite_movies",
                "Overwrite existing movie files",
                "",
                d.overwrite_movies,
            ),
            Field::checkbox(
                "overwrite_series",
                "Overwrite existing episode files",
                "",
                d.overwrite_series,
            ),
            Field::new(
                "match_type",
                "Matching method",
                "How wanted titles are matched to the provider's titles",
                Kind::Select(vec![
                    ("string", "Exact (string comparison)"),
                    ("fuzzy", "Fuzzy"),
                ]),
                &d.match_type,
            ),
            Field::checkbox(
                "tmdb_enrich_enabled",
                "Daily TMDB ID enrichment",
                "",
                d.tmdb_enrich_enabled,
            ),
            Field::time(
                "tmdb_enrich_time",
                "TMDB enrichment time",
                &d.tmdb_enrich_time,
            ),
            Field::checkbox(
                "cast_enrich_enabled",
                "Daily cast enrichment",
                "",
                d.cast_enrich_enabled,
            ),
            Field::time(
                "cast_enrich_time",
                "Cast enrichment time",
                &d.cast_enrich_time,
            ),
        ],
        _ => vec![
            Field::checkbox(
                "backup_enabled",
                "Daily backup",
                "To /data/backups inside the container",
                d.backup_enabled,
            ),
            Field::time("backup_time", "Backup time", &d.backup_time),
            Field::number("backup_keep", "Keep backups", &d.backup_keep, 1, 365),
            Field::checkbox(
                "smb_enabled",
                "Also copy to an SMB share",
                "After every backup, with the same number kept there",
                d.smb_enabled,
            ),
            Field::text(
                "smb_host",
                "SMB host",
                "Name or IP of the NAS, e.g. 192.168.0.20 (port 445; add :port for another)",
                &d.smb_host,
            ),
            Field::text("smb_share", "SMB share", "e.g. backups", &d.smb_share),
            Field::text(
                "smb_path",
                "Folder in the share (optional)",
                "e.g. m3usort. Created when missing.",
                &d.smb_path,
            ),
            Field::text("smb_username", "SMB username", "", &d.smb_username),
            if !in_wizard && !d.smb_password.is_empty() {
                Field::password(
                    "smb_password",
                    "SMB password",
                    "Set. Leave empty to keep it. Untick the SMB copy to remove it.",
                    "",
                )
            } else {
                Field::password("smb_password", "SMB password", "", &d.smb_password)
            },
        ],
    }
}

// ------------------------------------------------------------------ shared step logic

/// Read one step's form into the draft, validate and test it. Steps 2..=6.
/// With `keep_secrets` (admin settings), an empty secret field keeps the stored value.
pub async fn apply_step(
    st: &AppState,
    step: u8,
    d: &mut Draft,
    form: &HashMap<String, String>,
    keep_secrets: bool,
) -> Result<(), String> {
    let f = |k: &str| {
        form.get(k)
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let on = |k: &str| form.contains_key(k);
    match step {
        2 => {
            let server_in = f("provider_server");
            let user_in = f("provider_username");
            // Password is used as typed (no trim): spaces can be part of it.
            let pass_in = form.get("provider_password").cloned().unwrap_or_default();
            // A pasted playlist URL fills in whatever was left empty.
            let pasted = Xtream::from_playlist_url(&server_in);
            d.provider_server = Xtream::normalize_server(&server_in)?;
            d.provider_username = match (&pasted, user_in.is_empty()) {
                (Some(x), true) => x.username.clone(),
                _ => user_in,
            };
            if let (Some(x), true) = (&pasted, pass_in.is_empty()) {
                d.provider_password = x.password.clone();
            } else if !(keep_secrets && pass_in.is_empty()) {
                d.provider_password = pass_in;
            }
            if d.provider_username.is_empty() || d.provider_password.is_empty() {
                return Err("Server, username and password are all required.".into());
            }
            d.provider_result = integrations::test_provider(&st.http, &d.xtream()).await?;
            Ok(())
        }
        3 => {
            d.movies_dir = f("movies_dir");
            d.series_dir = f("series_dir");
            if !d.movies_dir.is_empty() {
                integrations::test_folder(&d.movies_dir)
                    .await
                    .map_err(|e| format!("Movies folder: {e}"))?;
            }
            if !d.series_dir.is_empty() {
                integrations::test_folder(&d.series_dir)
                    .await
                    .map_err(|e| format!("Series folder: {e}"))?;
            }
            Ok(())
        }
        4 => {
            d.jellyfin_enabled = on("jellyfin_enabled");
            d.jellyfin_url = f("jellyfin_url").trim_end_matches('/').to_string();
            let new = f("jellyfin_api_key");
            if !(keep_secrets && new.is_empty()) {
                d.jellyfin_api_key = new;
            }
            if !d.jellyfin_enabled {
                d.jellyfin_result = "Not used".into();
                return Ok(());
            }
            if d.jellyfin_url.is_empty() || d.jellyfin_api_key.is_empty() {
                return Err(
                    "Fill in the Jellyfin URL and API key, or untick \"Use Jellyfin\".".into(),
                );
            }
            d.jellyfin_result =
                integrations::test_jellyfin(&st.http, &d.jellyfin_url, &d.jellyfin_api_key).await?;
            Ok(())
        }
        5 => {
            let new = f("tmdb_api_key");
            if on("tmdb_remove") {
                d.tmdb_api_key.clear();
            } else if !(keep_secrets && new.is_empty()) {
                d.tmdb_api_key = new;
            }
            if d.tmdb_api_key.is_empty() {
                d.tmdb_result = "Not used".into();
                return Ok(());
            }
            d.tmdb_result = integrations::test_tmdb(&st.http, &d.tmdb_api_key).await?;
            Ok(())
        }
        6 => {
            d.playlist_refresh_hours = f("playlist_refresh_hours");
            d.playlist_output = f("playlist_output");
            d.custom_group_title = f("custom_group_title");
            d.vod_scheduler_enabled = on("vod_scheduler_enabled");
            d.vod_scan_hours = f("vod_scan_hours");
            d.overwrite_movies = on("overwrite_movies");
            d.overwrite_series = on("overwrite_series");
            d.match_type = if f("match_type") == "fuzzy" {
                "fuzzy".into()
            } else {
                "string".into()
            };
            d.tmdb_enrich_enabled = on("tmdb_enrich_enabled");
            d.tmdb_enrich_time = f("tmdb_enrich_time");
            d.cast_enrich_enabled = on("cast_enrich_enabled");
            d.cast_enrich_time = f("cast_enrich_time");
            validate_schedules(d)
        }
        7 => {
            d.backup_enabled = on("backup_enabled");
            d.backup_time = f("backup_time");
            d.backup_keep = f("backup_keep");
            d.smb_enabled = on("smb_enabled");
            d.smb_host = f("smb_host");
            d.smb_share = f("smb_share");
            d.smb_path = f("smb_path");
            d.smb_username = f("smb_username");
            // Password used as typed (no trim).
            let pass_in = form.get("smb_password").cloned().unwrap_or_default();
            if !(keep_secrets && pass_in.is_empty()) {
                d.smb_password = pass_in;
            }
            if !is_hh_mm(&d.backup_time) {
                return Err("Backup time must be HH:MM (24-hour).".into());
            }
            if !d
                .backup_keep
                .parse::<u32>()
                .is_ok_and(|n| (1..=365).contains(&n))
            {
                return Err("Keep backups must be a number from 1 to 365.".into());
            }
            if !d.smb_enabled {
                d.smb_result = "Not used".into();
                return Ok(());
            }
            if d.smb_host.is_empty() || d.smb_share.is_empty() {
                return Err("Fill in the SMB host and share, or untick the SMB copy.".into());
            }
            d.smb_result = crate::backup::smb_test(&crate::backup::smb_config(
                &d.smb_host,
                &d.smb_share,
                &d.smb_path,
                &d.smb_username,
                &d.smb_password,
            ))
            .await?;
            Ok(())
        }
        _ => Err("Unknown step".into()),
    }
}

fn validate_schedules(d: &Draft) -> Result<(), String> {
    let hours = |v: &str, what: &str| match v.parse::<u32>() {
        Ok(h) if (1..=24).contains(&h) => Ok(()),
        _ => Err(format!(
            "{what} must be a whole number of hours from 1 to 24."
        )),
    };
    hours(&d.playlist_refresh_hours, "Playlist refresh")?;
    hours(&d.vod_scan_hours, "VOD scan")?;
    let name_ok = !d.playlist_output.is_empty()
        && !d.playlist_output.contains(['/', '\\'])
        && !d.playlist_output.starts_with('.')
        && d.playlist_output.ends_with(".m3u");
    if !name_ok {
        return Err(
            "Playlist file name must be a plain file name ending in .m3u (no folders).".into(),
        );
    }
    if d.custom_group_title.is_empty() {
        return Err("Custom group title can't be empty.".into());
    }
    for (t, what) in [
        (&d.tmdb_enrich_time, "TMDB enrichment time"),
        (&d.cast_enrich_time, "Cast enrichment time"),
    ] {
        if !is_hh_mm(t) {
            return Err(format!("{what} must be HH:MM (24-hour)."));
        }
    }
    Ok(())
}

fn is_hh_mm(t: &str) -> bool {
    let Some((h, m)) = t.split_once(':') else {
        return false;
    };
    h.len() == 2
        && m.len() == 2
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60)
}

/// Write all settings and secrets of the draft. Passwords only when `with_passwords`.
/// Optional secrets that are empty or disabled are removed.
pub async fn save_draft(
    tx: &mut Transaction<'_, Sqlite>,
    st: &AppState,
    d: &Draft,
    with_passwords: bool,
) -> anyhow::Result<()> {
    let b = |v: bool| if v { "1".to_string() } else { "0".to_string() };
    if with_passwords {
        let admin = d.admin_hash.clone().unwrap_or_default();
        anyhow::ensure!(!admin.is_empty(), "admin password missing");
        s::set(&mut **tx, s::ADMIN_PASSWORD_HASH, &admin).await?;
        s::set(
            &mut **tx,
            s::PLAYLIST_PASSWORD_HASH,
            d.playlist_hash.as_deref().unwrap_or(""),
        )
        .await?;
    }
    let plain = [
        (s::MOVIES_DIR, d.movies_dir.clone()),
        (s::SERIES_DIR, d.series_dir.clone()),
        (s::JELLYFIN_ENABLED, b(d.jellyfin_enabled)),
        (s::JELLYFIN_URL, d.jellyfin_url.clone()),
        (s::PLAYLIST_OUTPUT, d.playlist_output.clone()),
        (s::PLAYLIST_REFRESH_HOURS, d.playlist_refresh_hours.clone()),
        (s::CUSTOM_GROUP_TITLE, d.custom_group_title.clone()),
        (s::VOD_SCHEDULER_ENABLED, b(d.vod_scheduler_enabled)),
        (s::VOD_SCAN_HOURS, d.vod_scan_hours.clone()),
        (s::OVERWRITE_MOVIES, b(d.overwrite_movies)),
        (s::OVERWRITE_SERIES, b(d.overwrite_series)),
        (s::MATCH_TYPE, d.match_type.clone()),
        (s::TMDB_ENRICH_ENABLED, b(d.tmdb_enrich_enabled)),
        (s::TMDB_ENRICH_TIME, d.tmdb_enrich_time.clone()),
        (s::CAST_ENRICH_ENABLED, b(d.cast_enrich_enabled)),
        (s::CAST_ENRICH_TIME, d.cast_enrich_time.clone()),
        (s::BACKUP_ENABLED, b(d.backup_enabled)),
        (s::BACKUP_TIME, d.backup_time.clone()),
        (s::BACKUP_KEEP, d.backup_keep.clone()),
        (s::SMB_ENABLED, b(d.smb_enabled)),
        (s::SMB_HOST, d.smb_host.clone()),
        (s::SMB_SHARE, d.smb_share.clone()),
        (s::SMB_PATH, d.smb_path.clone()),
        (s::SMB_USERNAME, d.smb_username.clone()),
    ];
    for (name, value) in &plain {
        s::set(&mut **tx, name, value).await?;
    }
    anyhow::ensure!(
        !d.provider_server.is_empty()
            && !d.provider_username.is_empty()
            && !d.provider_password.is_empty(),
        "provider server, username or password missing"
    );
    s::set(&mut **tx, s::PROVIDER_SERVER, &d.provider_server).await?;
    s::set_secret(
        &mut **tx,
        &st.key,
        s::SECRET_PROVIDER_USERNAME,
        &d.provider_username,
    )
    .await?;
    s::set_secret(
        &mut **tx,
        &st.key,
        s::SECRET_PROVIDER_PASSWORD,
        &d.provider_password,
    )
    .await?;
    s::delete_secret(&mut **tx, s::LEGACY_SECRET_PROVIDER_URL).await?;
    if d.jellyfin_enabled && !d.jellyfin_api_key.is_empty() {
        s::set_secret(
            &mut **tx,
            &st.key,
            s::SECRET_JELLYFIN_API_KEY,
            &d.jellyfin_api_key,
        )
        .await?;
    } else {
        s::delete_secret(&mut **tx, s::SECRET_JELLYFIN_API_KEY).await?;
    }
    if d.tmdb_api_key.is_empty() {
        s::delete_secret(&mut **tx, s::SECRET_TMDB_API_KEY).await?;
    } else {
        s::set_secret(&mut **tx, &st.key, s::SECRET_TMDB_API_KEY, &d.tmdb_api_key).await?;
    }
    if d.smb_enabled && !d.smb_password.is_empty() {
        s::set_secret(&mut **tx, &st.key, s::SECRET_SMB_PASSWORD, &d.smb_password).await?;
    } else {
        s::delete_secret(&mut **tx, s::SECRET_SMB_PASSWORD).await?;
    }
    Ok(())
}

// ------------------------------------------------------------------ templates

#[derive(Template)]
#[template(path = "setup_start.html")]
struct StartTpl {
    page: Page,
    ask_code: bool,
    restore_url: &'static str,
}

/// One form page. Used by the wizard and by Admin → Settings sections.
#[derive(Template)]
#[template(path = "setup_step.html")]
pub struct StepTpl {
    pub page: Page,
    pub progress: Option<(u8, u8)>,
    pub heading: String,
    pub intro: &'static str,
    pub fields: Vec<Field>,
    pub error: String,
    pub success: String,
    pub action: String,
    pub back: String,
    pub submit_label: &'static str,
    /// "Skip to finish" link (wizard, after the required steps).
    pub finish_link: bool,
}

#[derive(Template)]
#[template(path = "setup_summary.html")]
struct SummaryTpl {
    page: Page,
    rows: Vec<(String, String)>,
    error: String,
}

pub fn render(t: impl Template) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e}"),
        )
            .into_response(),
    }
}

fn step_page(step: u8, d: &Draft, error: String) -> Response {
    let info = step_info(step);
    render(StepTpl {
        page: Page::new(&format!("Setup – {}", info.title)),
        progress: Some((step, LAST_STEP)),
        heading: format!("Step {step} of {LAST_STEP}: {}", info.title),
        intro: info.intro,
        fields: fields(step, d, true),
        error,
        success: String::new(),
        action: format!("/setup/{step}"),
        back: if step > 1 {
            format!("/setup/{}", step - 1)
        } else {
            "/setup".into()
        },
        submit_label: match step {
            2 | 4 | 5 => "Test & next →",
            _ => "Next →",
        },
        finish_link: step > REQUIRED_STEPS && d.done >= REQUIRED_STEPS,
    })
}

/// Admin → Settings section page (steps 2..=6 of the wizard, saved immediately).
fn section_page(step: u8, d: &Draft, error: String, success: String) -> Response {
    let info = step_info(step);
    render(StepTpl {
        page: Page::admin(&format!("Settings – {}", info.title)),
        progress: None,
        heading: format!("Settings: {}", info.title),
        intro: info.intro,
        fields: fields(step, d, false),
        error,
        success,
        action: format!("/admin/settings/{step}"),
        back: "/admin".into(),
        submit_label: match step {
            2 | 4 | 5 => "Test & save",
            _ => "Save",
        },
        finish_link: false,
    })
}

// ------------------------------------------------------------------ wizard handlers

pub async fn start(State(st): State<AppState>) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    render(StartTpl {
        page: Page::new("Setup"),
        ask_code: true,
        restore_url: "/setup/restore",
    })
}

/// First run: restore a backup instead of a new install. Needs the setup code
/// (as the wizard), and only works while setup isn't finished.
pub async fn restore(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let reply = |ok: bool, message: String| {
        axum::Json(serde_json::json!({"ok": ok, "message": message})).into_response()
    };
    if st.is_setup_done() {
        return reply(
            false,
            "Setup is already finished; restore under Admin → Backups.".into(),
        );
    }
    let code = headers
        .get("x-setup-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim();
    if code.is_empty() || !code.eq_ignore_ascii_case(&st.setup_code) {
        return reply(
            false,
            "Wrong setup code. It is in the container log as \"Setup code: …\".".into(),
        );
    }
    crate::web::restore_and_restart(body.to_vec(), crate::web::without_credentials(&headers)).await
}

/// Current draft for this browser, if the setup cookie matches.
fn draft_for(st: &AppState, headers: &HeaderMap) -> Option<Draft> {
    let token = auth::read_cookie(headers, SETUP_COOKIE)?;
    let guard = st.draft.lock().expect("draft lock");
    match guard.as_ref() {
        Some((t, d)) if t == token => Some(d.clone()),
        _ => None,
    }
}

fn store_draft(st: &AppState, headers: &HeaderMap, d: Draft) {
    let Some(token) = auth::read_cookie(headers, SETUP_COOKIE) else {
        return;
    };
    let mut guard = st.draft.lock().expect("draft lock");
    if let Some((t, _)) = guard.as_ref()
        && t == token
    {
        *guard = Some((t.clone(), d));
    }
}

/// A step may be opened when the step before it is done, or once all required
/// steps are done (optional steps can be visited in any order or skipped).
fn may_open(d: &Draft, step: u8) -> bool {
    d.done + 1 >= step || d.done >= REQUIRED_STEPS
}

pub async fn show_step(
    State(st): State<AppState>,
    Path(step): Path<u8>,
    headers: HeaderMap,
) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    if !(1..=LAST_STEP).contains(&step) {
        return Redirect::to("/setup").into_response();
    }
    let d = if step == 1 {
        draft_for(&st, &headers).unwrap_or_default()
    } else {
        match draft_for(&st, &headers) {
            Some(d) if may_open(&d, step) => d,
            _ => return Redirect::to("/setup/1").into_response(),
        }
    };
    step_page(step, &d, String::new())
}

pub async fn submit_step(
    State(st): State<AppState>,
    Path(step): Path<u8>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    if step == 1 {
        return submit_step1(&st, addr, &headers, &form).await;
    }
    let Some(mut d) = draft_for(&st, &headers).filter(|d| may_open(d, step)) else {
        return Redirect::to("/setup/1").into_response();
    };
    match apply_step(&st, step, &mut d, &form, false).await {
        Err(e) => {
            store_draft(&st, &headers, d.clone()); // keep what was typed
            step_page(step, &d, e)
        }
        Ok(()) => {
            d.done = d.done.max(step);
            store_draft(&st, &headers, d);
            let next = if step == LAST_STEP {
                "/setup/summary".to_string()
            } else {
                format!("/setup/{}", step + 1)
            };
            Redirect::to(&next).into_response()
        }
    }
}

async fn submit_step1(
    st: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    let ip = addr.ip();
    let raw = |k: &str| form.get(k).cloned().unwrap_or_default();
    let mut d = draft_for(st, headers).unwrap_or_default();
    if let Err(min) = st.limiter.check(ip).await {
        return step_page(
            1,
            &d,
            format!(
                "Too many wrong setup codes. Try again in {min} minutes, or run: m3usort reset-ip {ip}"
            ),
        );
    }
    if !raw("setup_code")
        .trim()
        .eq_ignore_ascii_case(&st.setup_code)
    {
        st.limiter.fail(ip).await;
        return step_page(
            1,
            &d,
            "Wrong setup code. It is in the container log.".into(),
        );
    }
    st.limiter.reset(ip).await;

    let (a1, a2) = (raw("admin_password"), raw("admin_password2"));
    let (p1, p2) = (raw("playlist_password"), raw("playlist_password2"));
    if a1 != a2 {
        return step_page(1, &d, "The admin passwords don't match.".into());
    }
    if p1 != p2 {
        return step_page(1, &d, "The playlist passwords don't match.".into());
    }
    if a1.is_empty() && d.admin_hash.is_none() {
        return step_page(
            1,
            &d,
            "Choose an admin password. It protects the admin pages and can't be skipped.".into(),
        );
    }
    if !a1.is_empty() {
        d.admin_hash = Some(hash_off_thread(a1).await);
    }
    if !p1.is_empty() {
        d.playlist_hash = Some(hash_off_thread(p1).await);
    }
    d.done = d.done.max(1);

    // New token for every successful step 1: replaces any other browser's draft.
    let token = auth::random_code(32);
    *st.draft.lock().expect("draft lock") = Some((token.clone(), d));
    let mut resp = Redirect::to("/setup/2").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        auth::set_cookie_header(SETUP_COOKIE, &token, std::time::Duration::from_secs(3600))
            .parse()
            .expect("valid header"),
    );
    resp
}

fn or_later(v: &str) -> String {
    if v.is_empty() {
        "Not set (set later under Admin → Settings)".into()
    } else {
        v.to_string()
    }
}

fn yes(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

fn summary_rows(d: &Draft) -> Vec<(String, String)> {
    let on_off = |on: bool, text: String| if on { text } else { "Off".into() };
    vec![
        (
            "Admin password".into(),
            "Set (stored as Argon2id hash)".into(),
        ),
        (
            "Playlist password".into(),
            if d.playlist_hash.is_some() {
                "Set (stored as Argon2id hash)".into()
            } else {
                "Not set: playlist downloads are off".into()
            },
        ),
        ("Provider server".into(), d.provider_server.clone()),
        ("Provider username".into(), d.provider_username.clone()),
        ("Provider password".into(), "Set (stored encrypted)".into()),
        ("Provider test".into(), d.provider_result.clone()),
        ("Movies folder".into(), or_later(&d.movies_dir)),
        ("Series folder".into(), or_later(&d.series_dir)),
        (
            "Jellyfin".into(),
            if d.jellyfin_enabled {
                format!("{} — {}", d.jellyfin_url, d.jellyfin_result)
            } else {
                "Not used".into()
            },
        ),
        (
            "TMDB".into(),
            if d.tmdb_api_key.is_empty() {
                "Not used".into()
            } else {
                d.tmdb_result.clone()
            },
        ),
        (
            "Playlist refresh".into(),
            format!(
                "every {} h → {}",
                d.playlist_refresh_hours, d.playlist_output
            ),
        ),
        ("Custom group".into(), d.custom_group_title.clone()),
        (
            "VOD scheduler".into(),
            on_off(
                d.vod_scheduler_enabled,
                format!("every {} h", d.vod_scan_hours),
            ),
        ),
        (
            "Overwrite movies / episodes".into(),
            format!("{} / {}", yes(d.overwrite_movies), yes(d.overwrite_series)),
        ),
        ("Matching".into(), d.match_type.clone()),
        (
            "TMDB enrichment".into(),
            on_off(
                d.tmdb_enrich_enabled,
                format!("daily at {}", d.tmdb_enrich_time),
            ),
        ),
        (
            "Cast enrichment".into(),
            on_off(
                d.cast_enrich_enabled,
                format!("daily at {}", d.cast_enrich_time),
            ),
        ),
        (
            "Backups".into(),
            on_off(
                d.backup_enabled,
                format!("daily at {}, keep {}", d.backup_time, d.backup_keep),
            ),
        ),
        (
            "SMB copy".into(),
            if d.smb_enabled {
                d.smb_result.clone()
            } else {
                "Off".into()
            },
        ),
    ]
}

pub async fn summary(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    match draft_for(&st, &headers) {
        Some(d) if d.done >= REQUIRED_STEPS => render(SummaryTpl {
            page: Page::new("Setup – Summary"),
            rows: summary_rows(&d),
            error: String::new(),
        }),
        _ => Redirect::to("/setup/1").into_response(),
    }
}

pub async fn finish(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    let Some(d) = draft_for(&st, &headers).filter(|d| d.done >= REQUIRED_STEPS) else {
        return Redirect::to("/setup/1").into_response();
    };
    let saved = async {
        let mut tx = st.db.begin().await?;
        save_draft(&mut tx, &st, &d, true).await?;
        s::set(&mut *tx, s::SESSION_GENERATION, "1").await?;
        s::set(&mut *tx, s::SETUP_COMPLETED, "1").await?;
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = saved {
        tracing::error!("setup: saving failed: {e:#}");
        return render(SummaryTpl {
            page: Page::new("Setup – Summary"),
            rows: summary_rows(&d),
            error: format!("Saving failed, nothing was stored: {e}"),
        });
    }
    st.mark_setup_done();
    *st.draft.lock().expect("draft lock") = None;
    tracing::info!("setup finished");
    spawn_account_check(&st);

    // Log the admin in straight away.
    let generation = s::session_generation(&st.db).await.unwrap_or(0);
    let cookie = st.signer.issue(generation);
    let mut resp = Redirect::to("/admin?welcome=1").into_response();
    let h = resp.headers_mut();
    h.append(
        header::SET_COOKIE,
        auth::set_cookie_header(ADMIN_COOKIE, &cookie, SESSION_TTL)
            .parse()
            .expect("header"),
    );
    h.append(
        header::SET_COOKIE,
        auth::clear_cookie_header(SETUP_COOKIE)
            .parse()
            .expect("header"),
    );
    resp
}

// ------------------------------------------------------------------ admin settings sections

pub async fn admin_section(State(st): State<AppState>, Path(step): Path<u8>) -> Response {
    if !(2..=LAST_STEP).contains(&step) {
        return Redirect::to("/admin").into_response();
    }
    match Draft::load(&st).await {
        Ok(d) => section_page(step, &d, String::new(), String::new()),
        Err(e) => section_page(
            step,
            &Draft::default(),
            format!("Cannot load settings: {e}"),
            String::new(),
        ),
    }
}

/// Read the provider account (expiry date) in the background after the
/// provider settings change.
fn spawn_account_check(st: &AppState) {
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::account::check(&st).await {
            tracing::debug!("account check: {e:#}");
        }
    });
}

pub async fn admin_section_save(
    State(st): State<AppState>,
    Path(step): Path<u8>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if !(2..=LAST_STEP).contains(&step) {
        return Redirect::to("/admin").into_response();
    }
    let mut d = match Draft::load(&st).await {
        Ok(d) => d,
        Err(e) => {
            return section_page(
                step,
                &Draft::default(),
                format!("Cannot load settings: {e}"),
                String::new(),
            );
        }
    };
    if let Err(e) = apply_step(&st, step, &mut d, &form, true).await {
        return section_page(step, &d, e, String::new());
    }
    let saved = async {
        let mut tx = st.db.begin().await?;
        save_draft(&mut tx, &st, &d, false).await?;
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    match saved {
        Ok(()) => {
            tracing::info!("settings saved: {}", step_info(step).title);
            if step == 2 {
                spawn_account_check(&st);
            }
            let result = match step {
                2 => d.provider_result.clone(),
                4 => d.jellyfin_result.clone(),
                5 => d.tmdb_result.clone(),
                7 => d.smb_result.clone(),
                _ => String::new(),
            };
            let msg = if result.is_empty() {
                "Saved.".to_string()
            } else {
                format!("Saved. {result}")
            };
            section_page(step, &d, String::new(), msg)
        }
        Err(e) => section_page(
            step,
            &d,
            format!("Saving failed, nothing was changed: {e}"),
            String::new(),
        ),
    }
}

pub async fn hash_off_thread(pw: String) -> String {
    tokio::task::spawn_blocking(move || auth::hash_password(&pw))
        .await
        .expect("hashing task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_validation() {
        let mut d = Draft::default();
        assert!(validate_schedules(&d).is_ok());
        d.playlist_output = "../etc/passwd".into();
        assert!(validate_schedules(&d).is_err());
        d = Draft::default();
        d.playlist_output = ".m3u".into();
        assert!(validate_schedules(&d).is_err());
        d = Draft::default();
        d.vod_scan_hours = "0".into();
        assert!(validate_schedules(&d).is_err());
        d = Draft::default();
        d.cast_enrich_time = "24:00".into();
        assert!(validate_schedules(&d).is_err());
    }

    #[test]
    fn finish_allowed_after_provider() {
        let mut d = Draft {
            done: 1,
            ..Default::default()
        };
        assert!(may_open(&d, 2));
        assert!(!may_open(&d, 4), "can't skip the provider step");
        d.done = 2;
        assert!(
            may_open(&d, 6),
            "after the provider, any optional step is reachable"
        );
    }
}
