//! The .strm library: adding movies/series, wanted search terms, adopting existing
//! folders, adding new episodes, and the VOD job that ties it together.
//!
//! Compatible with folders written by the Python version:
//!   movies:  <movies_dir>/<Name>/<Name>.strm  →  <server>/movie/<user>/<pass>/<stream_id>.mkv
//!   series:  <series_dir>/<Name>/<Name> S01E02.strm  →  <server>/series/<user>/<pass>/<episode_id>.mkv
//! Existing folders are adopted (matched to the catalog by name), never renamed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;

use crate::catalog::{self, now_secs};
use crate::integrations::{self, Xtream};
use crate::settings as s;
use crate::web::AppState;

/// Same pause between per-series API calls as the Python version (avoids HTTP 429).
const SERIES_CALL_DELAY: Duration = Duration::from_millis(500);
/// A string-match wanted term that hits more titles than this is not added
/// (the Python version would add all of them, e.g. "the" → thousands of folders).
const MAX_STRING_MATCHES: usize = 25;
const FUZZY_THRESHOLD: f64 = 75.0;
const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "avi", "m4v"];

// ------------------------------------------------------------------ run summary

/// What a VOD run did. Details go to the log line by line; the UI gets counts.
#[derive(Default)]
pub struct RunLog {
    adopted_movies: usize,
    adopted_series: usize,
    wanted_found: Vec<String>,
    episodes: usize,
    series_updated: usize,
    series_failed: usize,
    series_switched: usize,
    jellyfin: Option<String>,
}

impl RunLog {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.adopted_movies + self.adopted_series > 0 {
            parts.push(format!(
                "adopted {} movie and {} series folder(s)",
                self.adopted_movies, self.adopted_series
            ));
        }
        if !self.wanted_found.is_empty() {
            parts.push(format!("wanted found: {}", self.wanted_found.join(", ")));
        }
        if self.episodes > 0 {
            parts.push(format!(
                "+{} episode(s) in {} series",
                self.episodes, self.series_updated
            ));
        }
        if self.series_switched > 0 {
            parts.push(format!(
                "{} series switched to a working catalog entry",
                self.series_switched
            ));
        }
        if self.series_failed > 0 {
            parts.push(format!(
                "{} series failed (see Series in library)",
                self.series_failed
            ));
        }
        if let Some(j) = &self.jellyfin {
            parts.push(j.clone());
        }
        if parts.is_empty() {
            "nothing new".into()
        } else {
            parts.join("; ")
        }
    }
}

// ------------------------------------------------------------------ names and paths

/// Turn a provider title into one safe folder/file name component. Same as the
/// title for normal names (Python compatibility); characters that would create
/// sub-folders or escape the base folder are replaced. None if nothing usable remains.
pub fn safe_name(title: &str) -> Option<String> {
    let mut out: String = title
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    // Leave room for " S01E01.strm" within the usual 255-byte file name limit.
    while out.len() > 200 {
        out.pop();
    }
    let out = out.trim_end().to_string();
    if out.is_empty() || out.chars().all(|c| c == '.') {
        None
    } else {
        Some(out)
    }
}

const QUALITY_TAGS: &[&str] = &[
    "4k", "hdr", "sdr", "uhd", "bluray", "bdrip", "brrip", "web-dl", "webdl", "webrip", "dvdrip",
    "remux", "hevc", "x264", "x265", "h264", "h.264", "h265", "h.265", "dts", "aac", "atmos",
];

/// Drop everything from the first quality tag ("4K", "HDR", "x265", …) on. Keeps case.
pub fn cut_quality_tags(name: &str) -> String {
    name.split_whitespace()
        .take_while(|w| {
            let w = w.to_lowercase();
            !QUALITY_TAGS
                .contains(&w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '.'))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Python's normalize_movie_name: drop everything from the first quality tag on,
/// then a trailing "(year)"; lowercase.
pub fn normalize_title(name: &str) -> String {
    strip_year(&cut_quality_tags(name)).0.trim().to_lowercase()
}

/// "Title (2010)" → ("Title", Some(2010)).
pub fn strip_year(name: &str) -> (String, Option<i32>) {
    let t = name.trim_end();
    if t.ends_with(')') && t.len() >= 6 {
        let open = t.len() - 6;
        if t.is_char_boundary(open)
            && &t[open..open + 1] == "("
            && let Ok(y) = t[open + 1..t.len() - 1].parse::<i32>()
        {
            return (t[..open].trim_end().to_string(), Some(y));
        }
    }
    (t.to_string(), None)
}

fn enc(s: &str) -> String {
    // Encode only what would break the URL path; normal user/passwords stay as in Python.
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
        .replace("%40", "@")
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
        .replace("%7E", "~")
}

pub fn movie_url(x: &Xtream, stream_id: i64) -> String {
    format!(
        "{}/movie/{}/{}/{stream_id}.mkv",
        x.server,
        enc(&x.username),
        enc(&x.password)
    )
}

pub fn episode_url(x: &Xtream, episode_id: &str) -> String {
    format!(
        "{}/series/{}/{}/{}.mkv",
        x.server,
        enc(&x.username),
        enc(&x.password),
        enc(episode_id)
    )
}

/// Write via a temp file + rename, so Jellyfin never sees a half-written .strm.
async fn write_strm(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("strm.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await
}

struct Dirs {
    movies: Option<PathBuf>,
    series: Option<PathBuf>,
    overwrite_movies: bool,
    overwrite_series: bool,
}

async fn dirs(st: &AppState) -> anyhow::Result<Dirs> {
    let get = |k| s::get(&st.db, k);
    let path = |v: Option<String>| v.filter(|p| !p.is_empty()).map(PathBuf::from);
    Ok(Dirs {
        movies: path(get(s::MOVIES_DIR).await?),
        series: path(get(s::SERIES_DIR).await?),
        overwrite_movies: get(s::OVERWRITE_MOVIES).await?.as_deref() == Some("1"),
        overwrite_series: get(s::OVERWRITE_SERIES).await?.as_deref() == Some("1"),
    })
}

async fn xtream(st: &AppState) -> anyhow::Result<Xtream> {
    catalog::load_xtream(st)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider is not configured"))
}

// ------------------------------------------------------------------ movies

/// Add one movie from the catalog. Returns a short message for the UI.
pub async fn add_movie(st: &AppState, stream_id: i64, via: &str) -> anyhow::Result<String> {
    let d = dirs(st).await?;
    let base = d
        .movies
        .ok_or_else(|| anyhow::anyhow!("no movies folder set (Admin → Settings → Folders)"))?;
    let x = xtream(st).await?;
    let name: String = sqlx::query_scalar("SELECT name FROM movies WHERE stream_id = ?")
        .bind(stream_id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("movie {stream_id} is not in the catalog"))?;
    let folder =
        safe_name(&name).ok_or_else(|| anyhow::anyhow!("'{name}' has no usable file name"))?;
    let dir = base.join(&folder);
    ensure_no_video(&dir).await?;
    let file = dir.join(format!("{folder}.strm"));
    let existed = file.exists();
    if !existed || d.overwrite_movies {
        tokio::fs::create_dir_all(&dir).await?;
        write_strm(&file, &movie_url(&x, stream_id)).await?;
        tracing::info!("created {}", file.display());
    }
    record_movie(st, stream_id, &folder, via).await?;
    Ok(if existed && !d.overwrite_movies {
        format!("'{name}' was already there")
    } else {
        format!("Added '{name}'")
    })
}

async fn record_movie(
    st: &AppState,
    stream_id: i64,
    folder: &str,
    via: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO library_movies (stream_id, folder, added_at, via) VALUES (?, ?, ?, ?)
         ON CONFLICT (stream_id) DO UPDATE SET folder = excluded.folder",
    )
    .bind(stream_id)
    .bind(folder)
    .bind(now_secs())
    .bind(via)
    .execute(&st.db)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------ series

pub struct Episode {
    pub id: String,
    pub season: i64,
    pub episode: i64,
}

/// Parse get_series_info → episodes. Handles both shapes providers use:
/// {"episodes": {"1": [..], "2": [..]}} and {"episodes": [[..], [..]]}.
pub fn parse_episodes(info: &Value) -> Vec<Episode> {
    let mut out = Vec::new();
    let mut push = |season_key: Option<i64>, ep: &Value| {
        let id = ep
            .get("id")
            .map(integrations::as_text)
            .filter(|s| !s.is_empty());
        let num = |k: &str| match ep.get(k) {
            Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64),
            _ => None,
        };
        if let (Some(id), Some(e)) = (id, num("episode_num")) {
            out.push(Episode {
                id,
                season: num("season").or(season_key).unwrap_or(1),
                episode: e,
            });
        }
    };
    match info.get("episodes") {
        Some(Value::Object(seasons)) => {
            for (k, eps) in seasons {
                for ep in eps.as_array().into_iter().flatten() {
                    push(k.parse().ok(), ep);
                }
            }
        }
        Some(Value::Array(seasons)) => {
            for eps in seasons {
                for ep in eps.as_array().into_iter().flatten() {
                    push(None, ep);
                }
            }
        }
        _ => {}
    }
    out
}

/// Python's file name: "<Name> S01E02.strm" (zero-padded to at least 2 digits).
pub fn episode_file(folder: &str, e: &Episode) -> String {
    format!("{folder} S{:02}E{:02}.strm", e.season, e.episode)
}

async fn fetch_episodes(st: &AppState, x: &Xtream, series_id: i64) -> anyhow::Result<Vec<Episode>> {
    let mut url = x.api_url("get_series_info").map_err(anyhow::Error::msg)?;
    url.query_pairs_mut()
        .append_pair("series_id", &series_id.to_string());
    let resp = st
        .http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            integrations::provider_user_agent(),
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "get_series_info answered HTTP {}",
        resp.status()
    );
    let body = resp.text().await?;
    let info: Value = serde_json::from_str(&body).map_err(|_| {
        anyhow::anyhow!("series {series_id}: answer is not JSON: {}", snippet(&body))
    })?;
    // Episodes are what we need; some providers leave "info" empty but still send them.
    let eps = parse_episodes(&info);
    anyhow::ensure!(
        !eps.is_empty(),
        "provider returned no episodes for series {series_id}: {}",
        snippet(&body)
    );
    Ok(eps)
}

/// First part of a provider answer, for error messages (credentials never appear in it).
fn snippet(body: &str) -> String {
    let t: String = body.trim().chars().take(160).collect();
    if body.trim().chars().count() > 160 {
        format!("{t}…")
    } else if t.is_empty() {
        "(empty answer)".into()
    } else {
        t
    }
}

/// Write the episodes that are missing (or all, with overwrite). Returns files written.
async fn write_episodes(
    dir: &Path,
    folder: &str,
    eps: &[Episode],
    x: &Xtream,
    overwrite: bool,
) -> anyhow::Result<usize> {
    tokio::fs::create_dir_all(dir).await?;
    let mut written = 0;
    for e in eps {
        let file = dir.join(episode_file(folder, e));
        if overwrite || !file.exists() {
            write_strm(&file, &episode_url(x, &e.id)).await?;
            written += 1;
        }
    }
    if written > 0 {
        tracing::info!("{folder}: {written} episode file(s) written");
    }
    Ok(written)
}

/// Add a series from the catalog: one get_series_info call, then all episodes.
pub async fn add_series(
    st: &AppState,
    series_id: i64,
    via: &str,
) -> anyhow::Result<(String, usize)> {
    let d = dirs(st).await?;
    let base = d
        .series
        .ok_or_else(|| anyhow::anyhow!("no series folder set (Admin → Settings → Folders)"))?;
    let x = xtream(st).await?;
    let (name, version): (String, Option<i64>) =
        sqlx::query_as("SELECT name, added FROM series WHERE series_id = ?")
            .bind(series_id)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("series {series_id} is not in the catalog"))?;
    let folder = match existing_series_folder(st, series_id).await? {
        Some(f) => f, // keep the folder it already has
        None => {
            safe_name(&name).ok_or_else(|| anyhow::anyhow!("'{name}' has no usable folder name"))?
        }
    };
    ensure_no_video(&base.join(&folder)).await?;
    let eps = fetch_episodes(st, &x, series_id).await?;
    let written =
        write_episodes(&base.join(&folder), &folder, &eps, &x, d.overwrite_series).await?;
    sqlx::query(
        "INSERT INTO library_series (series_id, folder, added_at, via, synced_version, last_sync, episodes, last_error)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT (series_id) DO UPDATE SET synced_version = excluded.synced_version,
           last_sync = excluded.last_sync, episodes = excluded.episodes, last_error = NULL",
    )
    .bind(series_id)
    .bind(&folder)
    .bind(now_secs())
    .bind(via)
    .bind(version)
    .bind(now_secs())
    .bind(eps.len() as i64)
    .execute(&st.db)
    .await?;
    let msg = if written == 0 {
        format!("'{name}': all {} episodes were already there", eps.len())
    } else {
        format!(
            "Added '{name}': {written} of {} episode(s) written",
            eps.len()
        )
    };
    Ok((msg, written))
}

async fn existing_series_folder(st: &AppState, series_id: i64) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT folder FROM library_series WHERE series_id = ?")
            .bind(series_id)
            .fetch_optional(&st.db)
            .await?,
    )
}

/// New episodes for every series in the library. Only series whose catalog
/// "last_modified" changed since the last sync are fetched, 0.5 s apart.
async fn sync_series(st: &AppState, log: &mut RunLog) -> anyhow::Result<usize> {
    let d = dirs(st).await?;
    let Some(base) = d.series else { return Ok(0) };
    let x = xtream(st).await?;
    let rows: Vec<(i64, String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT l.series_id, l.folder, l.synced_version, s.added
         FROM library_series l LEFT JOIN series s ON s.series_id = l.series_id",
    )
    .fetch_all(&st.db)
    .await?;
    let mut written = 0;
    let mut calls = 0;
    let mut alt_index: Option<NameIndex> = None; // built only when needed
    let mut in_library: HashSet<i64> = rows.iter().map(|r| r.0).collect();
    for (id, folder, synced, current) in rows {
        // Gone from the catalog: still try (other entries with the same name may work).
        let gone = current.is_none();
        let current = current.unwrap_or(0);
        let dir = base.join(&folder);
        if let Err(e) = ensure_no_video(&dir).await {
            set_series_error(st, id, &format!("{e:#}")).await?;
            continue;
        }
        let has_files = dir.is_dir();
        if !gone && synced == Some(current) && has_files {
            continue; // unchanged at the provider and folder still there
        }
        if calls > 0 {
            tokio::time::sleep(SERIES_CALL_DELAY).await;
        }
        calls += 1;
        // Try the series' own id first; if the provider has nothing for it, try other
        // catalog entries with the same name (stale duplicates are common).
        let mut tried = vec![id];
        let mut result = fetch_episodes(st, &x, id).await.map(|eps| (id, eps));
        if result.is_err() {
            let index = match &alt_index {
                Some(i) => i,
                None => {
                    let all: Vec<(i64, String, Option<i64>)> =
                        sqlx::query_as("SELECT series_id, name, added FROM series")
                            .fetch_all(&st.db)
                            .await?;
                    alt_index.insert(NameIndex::new(&all))
                }
            };
            for alt in index.find_all(&folder) {
                if tried.contains(&alt) || in_library.contains(&alt) || tried.len() >= 4 {
                    continue;
                }
                tried.push(alt);
                tokio::time::sleep(SERIES_CALL_DELAY).await;
                if let Ok(eps) = fetch_episodes(st, &x, alt).await {
                    result = Ok((alt, eps));
                    break;
                }
            }
        }
        match result {
            Ok((used_id, eps)) => {
                let n = write_episodes(&dir, &folder, &eps, &x, d.overwrite_series).await?;
                written += n;
                if n > 0 {
                    log.episodes += n;
                    log.series_updated += 1;
                }
                if used_id != id {
                    tracing::info!(
                        "{folder}: series {id} has no episodes at the provider; switched to {used_id}"
                    );
                    log.series_switched += 1;
                    in_library.insert(used_id);
                }
                let version: Option<i64> =
                    sqlx::query_scalar("SELECT added FROM series WHERE series_id = ?")
                        .bind(used_id)
                        .fetch_optional(&st.db)
                        .await?
                        .flatten();
                sqlx::query(
                    "UPDATE library_series SET series_id = ?, synced_version = ?, last_sync = ?, episodes = ?, last_error = NULL
                     WHERE series_id = ?",
                )
                .bind(used_id)
                .bind(version.unwrap_or(current))
                .bind(now_secs())
                .bind(eps.len() as i64)
                .bind(id)
                .execute(&st.db)
                .await?;
            }
            Err(e) => {
                let note = if tried.len() > 1 {
                    format!(
                        "{e:#} (also tried {} other catalog entries with this name)",
                        tried.len() - 1
                    )
                } else {
                    format!("{e:#}")
                };
                tracing::warn!("{folder}: {note}");
                log.series_failed += 1;
                set_series_error(st, id, &note).await?;
            }
        }
    }
    if calls > 0 {
        tracing::info!("series sync: {calls} series fetched, {written} episode file(s) written");
    }
    Ok(written)
}

async fn set_series_error(st: &AppState, id: i64, err: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE library_series SET last_error = ? WHERE series_id = ?")
        .bind(err)
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(())
}

// ------------------------------------------------------------------ adopting existing folders

enum Content {
    Empty,
    StrmOnly,
    VideoOnly,
    Mixed,
}

async fn folder_content(dir: &Path) -> std::io::Result<Content> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let (mut strm, mut video, mut any) = (false, false, false);
    while let Some(e) = rd.next_entry().await? {
        any = true;
        let name = e.file_name().to_string_lossy().to_lowercase();
        let ext = name.rsplit('.').next().unwrap_or("");
        if ext == "strm" {
            strm = true;
        } else if VIDEO_EXTS.contains(&ext) {
            video = true;
        }
    }
    Ok(match (any, strm, video) {
        (false, _, _) => Content::Empty,
        (_, true, true) => Content::Mixed,
        (_, false, true) => Content::VideoOnly,
        _ => Content::StrmOnly,
    })
}

/// Refuse to write .strm files next to real video files (Jellyfin would show both).
async fn ensure_no_video(dir: &Path) -> anyhow::Result<()> {
    if dir.is_dir()
        && matches!(
            folder_content(dir).await?,
            Content::VideoOnly | Content::Mixed
        )
    {
        anyhow::bail!(
            "'{}' already contains video files; not adding .strm files there (would show up twice in Jellyfin)",
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        );
    }
    Ok(())
}

async fn subfolders(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(base).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                let n = e.file_name().to_string_lossy().to_string();
                if !n.starts_with('.') {
                    out.push(n);
                }
            }
        }
    }
    out.sort();
    out
}

/// Match folders that aren't in the library yet (e.g. made by the Python version)
/// to the catalog by name: exact, case-insensitive, then Python's normalized name.
/// Folders with real video files are left alone (no duplicates in Jellyfin).
async fn adopt_folders(st: &AppState, log: &mut RunLog) -> anyhow::Result<usize> {
    let d = dirs(st).await?;
    let mut written = 0;

    if let Some(base) = &d.movies {
        let known: HashSet<String> = sqlx::query_scalar("SELECT folder FROM library_movies")
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
        let catalog: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT stream_id, name, added FROM movies")
                .fetch_all(&st.db)
                .await?;
        let index = NameIndex::new(&catalog);
        let x = catalog::load_xtream(st).await?;
        for folder in subfolders(base)
            .await
            .into_iter()
            .filter(|f| !known.contains(f))
        {
            let dir = base.join(&folder);
            let Some(id) = index.find(&folder) else {
                continue;
            };
            match folder_content(&dir).await? {
                Content::VideoOnly | Content::Mixed => continue,
                Content::Empty | Content::StrmOnly => {}
            }
            let file = dir.join(format!("{folder}.strm"));
            if let Some(x) = &x
                && (!file.exists() || d.overwrite_movies)
            {
                write_strm(&file, &movie_url(x, id)).await?;
                written += 1;
            }
            record_movie(st, id, &folder, "adopted").await?;
            tracing::info!("adopted movie folder '{folder}'");
            log.adopted_movies += 1;
        }
    }

    if let Some(base) = &d.series {
        let known: HashSet<String> = sqlx::query_scalar("SELECT folder FROM library_series")
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
        let catalog: Vec<(i64, String, Option<i64>)> =
            sqlx::query_as("SELECT series_id, name, added FROM series")
                .fetch_all(&st.db)
                .await?;
        let index = NameIndex::new(&catalog);
        for folder in subfolders(base)
            .await
            .into_iter()
            .filter(|f| !known.contains(f))
        {
            let Some(id) = index.find(&folder) else {
                continue;
            };
            match folder_content(&base.join(&folder)).await? {
                Content::VideoOnly | Content::Mixed => continue,
                Content::Empty | Content::StrmOnly => {}
            }
            // synced_version NULL: the next series sync fetches its episodes.
            let res = sqlx::query(
                "INSERT INTO library_series (series_id, folder, added_at, via) VALUES (?, ?, ?, 'adopted')
                 ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(&folder)
            .bind(now_secs())
            .execute(&st.db)
            .await?;
            if res.rows_affected() > 0 {
                tracing::info!("adopted series folder '{folder}'");
                log.adopted_series += 1;
            }
        }
    }
    Ok(written)
}

/// Name → ids lookup: exact, then case-insensitive, then normalized (Python rules).
/// A title can be in the catalog more than once (e.g. an NL and an EN category, or
/// a stale entry); all candidates are kept, most recently changed first.
struct NameIndex {
    exact: HashMap<String, Vec<(i64, i64)>>,
    lower: HashMap<String, Vec<(i64, i64)>>,
    normalized: HashMap<String, Vec<(i64, i64)>>,
}

impl NameIndex {
    /// items: (id, name, added/last_modified)
    fn new(items: &[(i64, String, Option<i64>)]) -> Self {
        let mut idx = Self {
            exact: HashMap::new(),
            lower: HashMap::new(),
            normalized: HashMap::new(),
        };
        for (id, name, added) in items {
            let e = (*id, added.unwrap_or(0));
            idx.exact.entry(name.clone()).or_default().push(e);
            if let Some(safe) = safe_name(name).filter(|s| s != name) {
                idx.exact.entry(safe).or_default().push(e);
            }
            idx.lower.entry(name.to_lowercase()).or_default().push(e);
            idx.normalized
                .entry(normalize_title(name))
                .or_default()
                .push(e);
        }
        for map in [&mut idx.exact, &mut idx.lower, &mut idx.normalized] {
            for v in map.values_mut() {
                v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                v.dedup_by_key(|e| e.0);
            }
        }
        idx
    }

    /// All candidate ids for a folder name, best first. Empty when nothing matches.
    fn find_all(&self, folder: &str) -> Vec<i64> {
        self.exact
            .get(folder)
            .or_else(|| self.lower.get(&folder.to_lowercase()))
            .or_else(|| self.normalized.get(&normalize_title(folder)))
            .map(|v| v.iter().map(|e| e.0).collect())
            .unwrap_or_default()
    }

    fn find(&self, folder: &str) -> Option<i64> {
        self.find_all(folder).first().copied()
    }
}

// ------------------------------------------------------------------ wanted terms

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// fuzzywuzzy's token_sort_ratio, approximated with normalized Levenshtein (0..100).
pub fn token_sort_ratio(a: &str, b: &str) -> f64 {
    let sorted = |s: &str| {
        let mut w = words(s);
        w.sort();
        w.join(" ")
    };
    strsim::normalized_levenshtein(&sorted(a), &sorted(b)) * 100.0
}

/// Python's matching: "string" = every title containing the term (case-insensitive);
/// "fuzzy" = the single best title that contains all words of the term and scores
/// ≥ 75, ties → most recent year, never a year in the future.
pub fn match_term(term: &str, catalog: &[(i64, String)], fuzzy: bool) -> Vec<(i64, String)> {
    if !fuzzy {
        let t = term.to_lowercase();
        return catalog
            .iter()
            .filter(|(_, n)| n.to_lowercase().contains(&t))
            .cloned()
            .collect();
    }
    let this_year = 1970 + (now_secs() / 31_556_952) as i32;
    let want: HashSet<String> = words(term).into_iter().collect();
    let mut best: Option<(f64, i32, &(i64, String))> = None;
    for item in catalog {
        let (bare, year) = strip_year(&item.1);
        if year.is_some_and(|y| y > this_year) {
            continue;
        }
        let have: HashSet<String> = words(&bare).into_iter().collect();
        if !want.is_subset(&have) {
            continue;
        }
        let score = token_sort_ratio(term, &bare);
        if score < FUZZY_THRESHOLD {
            continue;
        }
        let y = year.unwrap_or(0);
        if best.is_none_or(|(bs, by, _)| score > bs || (score == bs && y > by)) {
            best = Some((score, y, item));
        }
    }
    best.map(|(_, _, i)| vec![i.clone()]).unwrap_or_default()
}

async fn process_wanted(st: &AppState, log: &mut RunLog) -> anyhow::Result<usize> {
    let fuzzy = s::get(&st.db, s::MATCH_TYPE).await?.as_deref() == Some("fuzzy");
    let pending: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, kind, term FROM wanted WHERE found_at IS NULL ORDER BY id")
            .fetch_all(&st.db)
            .await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let movies: Vec<(i64, String)> = sqlx::query_as("SELECT stream_id, name FROM movies")
        .fetch_all(&st.db)
        .await?;
    let series: Vec<(i64, String)> = sqlx::query_as("SELECT series_id, name FROM series")
        .fetch_all(&st.db)
        .await?;
    let mut written = 0;
    for (id, kind, term) in pending {
        let is_movie = kind == "movie";
        let matches = match_term(&term, if is_movie { &movies } else { &series }, fuzzy);
        let mut note = None;
        if matches.len() > MAX_STRING_MATCHES {
            note = Some(format!(
                "{} matches: too many, make the term more specific",
                matches.len()
            ));
        } else if !matches.is_empty() {
            let mut names = Vec::new();
            for (item_id, name) in &matches {
                let r = if is_movie {
                    add_movie(st, *item_id, "wanted").await.map(|_| 1)
                } else {
                    let r = add_series(st, *item_id, "wanted").await.map(|(_, n)| n);
                    tokio::time::sleep(SERIES_CALL_DELAY).await;
                    r
                };
                match r {
                    Ok(n) => {
                        written += n;
                        names.push(name.clone());
                    }
                    Err(e) => tracing::warn!("wanted '{term}' → '{name}': {e:#}"),
                }
            }
            if !names.is_empty() {
                sqlx::query(
                    "UPDATE wanted SET found_at = ?, found_names = ?, last_check = ? WHERE id = ?",
                )
                .bind(now_secs())
                .bind(names.join(" | "))
                .bind(now_secs())
                .bind(id)
                .execute(&st.db)
                .await?;
                tracing::info!("wanted '{term}' found: {}", names.join(", "));
                log.wanted_found.push(term.clone());
                continue;
            }
        }
        sqlx::query("UPDATE wanted SET last_check = ?, found_names = ? WHERE id = ?")
            .bind(now_secs())
            .bind(note)
            .bind(id)
            .execute(&st.db)
            .await?;
    }
    Ok(written)
}

// ------------------------------------------------------------------ the VOD job

/// One VOD run: adopt folders → wanted terms → new episodes → Jellyfin refresh.
pub async fn run_vod_job(st: &AppState) -> anyhow::Result<String> {
    if st.vod_running.swap(true, Ordering::SeqCst) {
        anyhow::bail!("the VOD job is already running");
    }
    let started = now_secs();
    let run_id: i64 = sqlx::query_scalar("INSERT INTO vod_runs (started) VALUES (?) RETURNING id")
        .bind(started)
        .fetch_one(&st.db)
        .await?;
    let mut log = RunLog::default();
    let result = async {
        let mut written = adopt_folders(st, &mut log).await?;
        written += process_wanted(st, &mut log).await?;
        written += sync_series(st, &mut log).await?;
        if written > 0 {
            refresh_jellyfin(st, &mut log).await;
        }
        anyhow::Ok(written)
    }
    .await;
    st.vod_running.store(false, Ordering::SeqCst);

    let (ok, written, msg) = match &result {
        Ok(n) => (true, *n as i64, log.summary()),
        Err(e) => (false, 0, format!("{e:#}")),
    };
    sqlx::query("UPDATE vod_runs SET finished = ?, ok = ?, written = ?, message = ? WHERE id = ?")
        .bind(now_secs())
        .bind(i64::from(ok))
        .bind(written)
        .bind(&msg)
        .bind(run_id)
        .execute(&st.db)
        .await?;
    sqlx::query("DELETE FROM vod_runs WHERE id <= (SELECT MAX(id) - 100 FROM vod_runs)")
        .execute(&st.db)
        .await?;
    if ok {
        tracing::info!("VOD job: {written} file(s) written; {msg}");
        Ok(format!("{written} file(s) written; {msg}"))
    } else {
        tracing::error!("VOD job failed: {msg}");
        anyhow::bail!(msg)
    }
}

pub async fn refresh_jellyfin(st: &AppState, log: &mut RunLog) {
    let enabled = s::get(&st.db, s::JELLYFIN_ENABLED)
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    let url = s::get(&st.db, s::JELLYFIN_URL)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let key = s::get_secret(&st.db, &st.key, s::SECRET_JELLYFIN_API_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if !enabled || url.is_empty() || key.is_empty() {
        return;
    }
    match integrations::jellyfin_refresh(&st.http, &url, &key).await {
        Ok(()) => log.jellyfin = Some("Jellyfin refreshed".into()),
        Err(e) => {
            tracing::warn!("Jellyfin refresh: {e}");
            log.jellyfin = Some(format!("Jellyfin refresh failed: {e}"));
        }
    }
}

/// Run the VOD job when it is enabled and the last run is older than the interval.
pub async fn maybe_run_vod(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done()
        || st.vod_running.load(Ordering::SeqCst)
        || st.catalog_running.load(Ordering::SeqCst)
    {
        return Ok(());
    }
    if s::get(&st.db, s::VOD_SCHEDULER_ENABLED).await?.as_deref() != Some("1") {
        return Ok(());
    }
    let have_catalog: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM catalog_runs WHERE ok = 1")
        .fetch_one(&st.db)
        .await?;
    if have_catalog == 0 {
        return Ok(()); // nothing to match against yet
    }
    let hours: i64 = s::get(&st.db, s::VOD_SCAN_HOURS)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(started) FROM vod_runs")
        .fetch_one(&st.db)
        .await?;
    if last.is_none_or(|t| now_secs() - t >= hours * 3600) {
        let _ = run_vod_job(st).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(
            safe_name("Inception (2010)").as_deref(),
            Some("Inception (2010)")
        );
        assert_eq!(safe_name("AC/DC: Live").as_deref(), Some("AC DC: Live"));
        assert_eq!(safe_name("../../etc").as_deref(), Some(".. .. etc"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("  "), None);
        assert!(safe_name(&"x".repeat(400)).unwrap().len() <= 200);
    }

    #[test]
    fn python_compatible_urls_and_names() {
        let x = Xtream {
            server: "http://iptvproxy.local:8080".into(),
            username: "user".into(),
            password: "pass".into(),
        };
        assert_eq!(
            movie_url(&x, 123),
            "http://iptvproxy.local:8080/movie/user/pass/123.mkv"
        );
        assert_eq!(
            episode_url(&x, "4567"),
            "http://iptvproxy.local:8080/series/user/pass/4567.mkv"
        );
        let e = Episode {
            id: "1".into(),
            season: 1,
            episode: 2,
        };
        assert_eq!(episode_file("Bitterzoet", &e), "Bitterzoet S01E02.strm");
        let e = Episode {
            id: "1".into(),
            season: 3,
            episode: 104,
        };
        assert_eq!(episode_file("Show", &e), "Show S03E104.strm");
        let odd = Xtream {
            password: "a/b c".into(),
            ..x
        };
        assert_eq!(
            movie_url(&odd, 1),
            "http://iptvproxy.local:8080/movie/user/a%2Fb%20c/1.mkv"
        );
    }

    #[test]
    fn normalization_like_python() {
        assert_eq!(normalize_title("Inception (2010)"), "inception");
        assert_eq!(normalize_title("Dune Part Two 4K HDR"), "dune part two");
        assert_eq!(
            normalize_title("The Batman (2022) WEB-DL x265"),
            "the batman"
        );
        assert_eq!(strip_year("Title (2010)"), ("Title".into(), Some(2010)));
        assert_eq!(strip_year("No year"), ("No year".into(), None));
    }

    #[test]
    fn episode_parsing_both_shapes() {
        let obj = serde_json::json!({"info": {"name": "X"}, "episodes": {
            "1": [{"id": "11", "episode_num": "1", "season": 1}, {"id": 12, "episode_num": 2}],
            "2": [{"id": "21", "episode_num": 1.0}]}});
        let mut e = parse_episodes(&obj);
        e.sort_by_key(|e| (e.season, e.episode));
        assert_eq!(
            e.iter()
                .map(|e| (e.season, e.episode, e.id.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, 1, "11"), (1, 2, "12"), (2, 1, "21")]
        );
        let arr = serde_json::json!({"episodes": [[{"id": "5", "episode_num": 1, "season": 3}]]});
        assert_eq!(parse_episodes(&arr)[0].season, 3);
    }

    #[test]
    fn matching_like_python() {
        let cat: Vec<(i64, String)> = vec![
            (1, "The Batman (2022)".into()),
            (2, "Batman Begins (2005)".into()),
            (3, "The Batman (1989)".into()),
            (4, "Batman (2099)".into()),
            (5, "Inception (2010)".into()),
        ];
        // string: every title containing the term
        assert_eq!(match_term("batman", &cat, false).len(), 4);
        // fuzzy: best single match, newest year on a tie, no future years
        assert_eq!(
            match_term("the batman", &cat, true),
            vec![(1, "The Batman (2022)".to_string())]
        );
        assert!(
            match_term("batman", &cat, true)
                .iter()
                .all(|(id, _)| *id != 4)
        );
        assert!(
            match_term("incep", &cat, true).is_empty(),
            "words must be complete in fuzzy mode"
        );
        assert!(token_sort_ratio("batman the", "The Batman") > 99.0);
    }
}
