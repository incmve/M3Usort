//! Jellyfin library view (admin): what is in Jellyfin, which items come from
//! M3Usort (.strm), who watched what, seasons/episodes, and removing M3Usort items.
//!
//! Compared to the Python version:
//! - a handful of requests per refresh (users, movies+series, all episodes, one
//!   per user for watched items) instead of one extra request per series;
//! - series watch status at episode level ("naomi 3/24"), not "any episode";
//! - `/Items?userId=` instead of the deprecated `/Users/{id}/Items`;
//! - posters go through this app, so the API key never reaches the browser;
//! - remove is only offered for .strm items (Jellyfin's DELETE /Items also
//!   deletes files on disk, so a real video would be gone), and it also takes the
//!   title off M3Usort's library list so the next VOD run doesn't write it again.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde_json::Value;

use crate::catalog::now_secs;
use crate::integrations::as_text;
use crate::settings as s;
use crate::web::AppState;

/// How long a snapshot is reused before Jellyfin is asked again.
const CACHE_SECS: i64 = 300;
const PAGE: usize = 1000;

#[derive(Clone)]
pub struct Conn {
    base: String,
    key: String,
}

pub async fn conn(st: &AppState) -> Option<Conn> {
    let base = s::get(&st.db, s::JELLYFIN_URL).await.ok().flatten()?;
    let key = s::get_secret(&st.db, &st.key, s::SECRET_JELLYFIN_API_KEY)
        .await
        .ok()
        .flatten()?;
    let base = base.trim().trim_end_matches('/').to_string();
    (!base.is_empty() && !key.trim().is_empty()).then(|| Conn {
        base,
        key: key.trim().to_string(),
    })
}

impl Conn {
    fn request(
        &self,
        st: &AppState,
        method: Method,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut url = Url::parse(&format!("{}{path}", self.base))
            .map_err(|e| format!("bad Jellyfin URL: {e}"))?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        Ok(st
            .http
            .request(method, url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("MediaBrowser Token=\"{}\"", self.key),
            )
            .timeout(Duration::from_secs(30)))
    }

    async fn get_json(
        &self,
        st: &AppState,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Value, String> {
        let resp = self
            .request(st, Method::GET, path, params)?
            .send()
            .await
            .map_err(|e| format!("cannot reach Jellyfin: {e}"))?;
        match resp.status() {
            StatusCode::OK => resp
                .json()
                .await
                .map_err(|e| format!("Jellyfin answer: {e}")),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err("Jellyfin refused the API key".into())
            }
            s => Err(format!("Jellyfin answered HTTP {s} for {path}")),
        }
    }

    /// All pages of an /Items query.
    async fn items(&self, st: &AppState, params: &[(&str, &str)]) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        loop {
            let start = out.len().to_string();
            let limit = PAGE.to_string();
            let mut p: Vec<(&str, &str)> = params.to_vec();
            p.extend([
                ("Recursive", "true"),
                ("StartIndex", start.as_str()),
                ("Limit", limit.as_str()),
            ]);
            let v = self.get_json(st, "/Items", &p).await?;
            let page = v
                .get("Items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = v
                .get("TotalRecordCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let n = page.len();
            out.extend(page);
            if n == 0 || n < PAGE || out.len() >= total {
                return Ok(out);
            }
        }
    }
}

// ------------------------------------------------------------------ snapshot

#[derive(Clone, Serialize)]
pub struct Episode {
    pub id: String,
    pub season: i64,
    pub index: Option<i64>,
    pub name: String,
    pub strm: bool,
    pub watched_by: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct Season {
    pub name: String,
    pub index: i64,
    pub episodes: Vec<Episode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Source {
    M3usort,
    Other,
    Mixed,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::M3usort => "m3usort",
            Source::Other => "other",
            Source::Mixed => "mixed",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Source::M3usort => "M3Usort",
            Source::Other => "Other",
            Source::Mixed => "Mixed",
        }
    }
}

/// Watch status of one user for one item.
#[derive(Clone)]
pub struct Watch {
    pub user: String,
    /// Movies: 1 when played. Series: episodes played.
    pub done: usize,
    /// Movies: 1. Series: episodes in Jellyfin.
    pub total: usize,
}

impl Watch {
    pub fn full(&self) -> bool {
        self.total > 0 && self.done >= self.total
    }
    pub fn text(&self) -> String {
        if self.total <= 1 || self.full() {
            format!("{} ✓", self.user)
        } else {
            format!("{} {}/{}", self.user, self.done, self.total)
        }
    }
}

#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub is_movie: bool,
    pub year: Option<i64>,
    pub rating: Option<f64>,
    pub added: String,
    pub added_sort: String,
    pub overview: String,
    pub path: String,
    pub source: Source,
    pub episodes: usize,
    /// Only users who watched something.
    pub watched: Vec<Watch>,
    /// Watched completely by every user.
    pub by_all: bool,
    /// Watched (fully or partly) by nobody.
    pub by_none: bool,
}

pub struct Snapshot {
    pub fetched: i64,
    pub users: Vec<String>,
    pub items: Vec<Item>,
    pub seasons: HashMap<String, Vec<Season>>,
}

/// The cached snapshot, or a fresh one when it is older than 5 minutes / forced.
pub async fn snapshot(st: &AppState, force: bool) -> Result<Arc<Snapshot>, String> {
    let mut cache = st.jellyfin.lock().await;
    if !force
        && let Some(s) = cache.as_ref()
        && now_secs() - s.fetched < CACHE_SECS
    {
        return Ok(s.clone());
    }
    let Some(c) = conn(st).await else {
        return Err(
            "Jellyfin is not set up: set its URL and API key under Admin → Settings → Jellyfin."
                .into(),
        );
    };
    let started = std::time::Instant::now();
    let snap = Arc::new(fetch(st, &c).await?);
    tracing::info!(
        "Jellyfin library: {} items, {} users in {:.1}s",
        snap.items.len(),
        snap.users.len(),
        started.elapsed().as_secs_f64()
    );
    *cache = Some(snap.clone());
    Ok(snap)
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).map(as_text).unwrap_or_default()
}

fn is_strm(path: &str) -> bool {
    path.to_lowercase().ends_with(".strm")
}

async fn fetch(st: &AppState, c: &Conn) -> Result<Snapshot, String> {
    let users_json = c.get_json(st, "/Users", &[]).await?;
    let users: Vec<(String, String)> = users_json
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|u| {
            !u.get("Policy")
                .and_then(|p| p.get("IsDisabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|u| (text(u, "Id"), text(u, "Name")))
        .collect();

    let fields = "Path,Overview,ProductionYear,DateCreated";
    let main = c
        .items(
            st,
            &[
                ("IncludeItemTypes", "Movie,Series"),
                ("Fields", fields),
                ("EnableImages", "false"),
            ],
        )
        .await?;
    let episodes = c
        .items(
            st,
            &[
                ("IncludeItemTypes", "Episode"),
                ("Fields", "Path"),
                ("EnableImages", "false"),
            ],
        )
        .await?;

    // watched[user] = set of played movie/episode ids
    let mut watched: Vec<(String, HashSet<String>)> = Vec::new();
    for (uid, name) in &users {
        let played = c
            .items(
                st,
                &[
                    ("userId", uid.as_str()),
                    ("IncludeItemTypes", "Movie,Episode"),
                    ("IsPlayed", "true"),
                    ("EnableImages", "false"),
                ],
            )
            .await?;
        watched.push((name.clone(), played.iter().map(|i| text(i, "Id")).collect()));
    }
    let who = |id: &str| -> Vec<String> {
        watched
            .iter()
            .filter(|(_, set)| set.contains(id))
            .map(|(u, _)| u.clone())
            .collect()
    };

    // Episodes per series, grouped into seasons.
    let mut by_series: HashMap<String, Vec<Episode>> = HashMap::new();
    let mut season_names: HashMap<(String, i64), String> = HashMap::new();
    for e in &episodes {
        let sid = text(e, "SeriesId");
        if sid.is_empty() {
            continue;
        }
        let id = text(e, "Id");
        let season = e
            .get("ParentIndexNumber")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let sname = text(e, "SeasonName");
        if !sname.is_empty() {
            season_names.entry((sid.clone(), season)).or_insert(sname);
        }
        by_series.entry(sid).or_default().push(Episode {
            watched_by: who(&id),
            id,
            season,
            index: e.get("IndexNumber").and_then(Value::as_i64),
            name: text(e, "Name"),
            strm: is_strm(&text(e, "Path")),
        });
    }

    let all_users: Vec<String> = users.iter().map(|(_, n)| n.clone()).collect();
    let mut items = Vec::new();
    let mut seasons_map = HashMap::new();
    for v in &main {
        let id = text(v, "Id");
        let is_movie = text(v, "Type") == "Movie";
        let path = text(v, "Path");
        let created = text(v, "DateCreated");
        let (source, n_eps, watch) = if is_movie {
            let source = if is_strm(&path) {
                Source::M3usort
            } else {
                Source::Other
            };
            let w = who(&id)
                .into_iter()
                .map(|u| Watch {
                    user: u,
                    done: 1,
                    total: 1,
                })
                .collect();
            (source, 0, w)
        } else {
            let eps = by_series.remove(&id).unwrap_or_default();
            let strm = eps.iter().filter(|e| e.strm).count();
            let source = match (strm, eps.len()) {
                (0, _) => Source::Other,
                (s, n) if s == n => Source::M3usort,
                _ => Source::Mixed,
            };
            let w: Vec<Watch> = all_users
                .iter()
                .map(|u| Watch {
                    user: u.clone(),
                    done: eps.iter().filter(|e| e.watched_by.contains(u)).count(),
                    total: eps.len(),
                })
                .filter(|w| w.done > 0)
                .collect();
            let n = eps.len();
            let mut seasons: Vec<Season> = Vec::new();
            let mut eps = eps;
            eps.sort_by_key(|e| (e.season, e.index.unwrap_or(i64::MAX)));
            for e in eps {
                if seasons.last().is_none_or(|s| s.index != e.season) {
                    let name = season_names
                        .get(&(id.clone(), e.season))
                        .cloned()
                        .unwrap_or_else(|| {
                            if e.season == 0 {
                                "Specials".into()
                            } else {
                                format!("Season {}", e.season)
                            }
                        });
                    seasons.push(Season {
                        name,
                        index: e.season,
                        episodes: Vec::new(),
                    });
                }
                if let Some(s) = seasons.last_mut() {
                    s.episodes.push(e);
                }
            }
            seasons_map.insert(id.clone(), seasons);
            (source, n, w)
        };
        let by_all = !all_users.is_empty()
            && all_users
                .iter()
                .all(|u| watch.iter().any(|w: &Watch| &w.user == u && w.full()));
        items.push(Item {
            by_none: watch.is_empty(),
            by_all,
            watched: watch,
            name: text(v, "Name"),
            is_movie,
            year: v.get("ProductionYear").and_then(Value::as_i64),
            rating: v.get("CommunityRating").and_then(Value::as_f64),
            added: created.get(..10).unwrap_or_default().to_string(),
            added_sort: created,
            overview: text(v, "Overview"),
            path,
            source,
            episodes: n_eps,
            id,
        });
    }
    items.sort_by_key(|i| i.name.to_lowercase());
    Ok(Snapshot {
        fetched: now_secs(),
        users: all_users,
        items,
        seasons: seasons_map,
    })
}

// ------------------------------------------------------------------ poster

/// Jellyfin ids are 32 hex characters (or a dashed GUID).
pub fn valid_id(id: &str) -> bool {
    (32..=36).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Primary image of an item, fetched with the API key server-side.
pub async fn poster(st: &AppState, id: &str) -> Option<(String, Vec<u8>)> {
    let c = conn(st).await?;
    let resp = c
        .request(
            st,
            Method::GET,
            &format!("/Items/{id}/Images/Primary"),
            &[("maxHeight", "300"), ("quality", "85")],
        )
        .ok()?
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("image/"))
        .unwrap_or("image/jpeg")
        .to_string();
    Some((ct, resp.bytes().await.ok()?.to_vec()))
}

// ------------------------------------------------------------------ remove

/// Remove an M3Usort item: Jellyfin entry + .strm folder + M3Usort library row.
pub async fn remove(st: &AppState, id: &str) -> Result<String, String> {
    let snap = snapshot(st, false).await?;
    let item = snap
        .items
        .iter()
        .find(|i| i.id == id)
        .cloned()
        .ok_or("item not found; reload the page")?;
    if item.source != Source::M3usort {
        return Err(format!(
            "'{}' isn't an M3Usort item (.strm only); remove it in Jellyfin itself",
            item.name
        ));
    }
    // Jellyfin's path is its own mount (e.g. /media/data/movies/X/X.strm); only the
    // folder name is shared with ours (/data/media/movies/X).
    let folder = if item.is_movie {
        Path::new(&item.path).parent().and_then(Path::file_name)
    } else {
        Path::new(&item.path).file_name()
    }
    .map(|n| n.to_string_lossy().to_string())
    .filter(|n| !n.is_empty() && n != "." && n != "..")
    .ok_or("can't work out the folder from Jellyfin's path")?;
    let base = s::get(
        &st.db,
        if item.is_movie {
            s::MOVIES_DIR
        } else {
            s::SERIES_DIR
        },
    )
    .await
    .map_err(|e| e.to_string())?
    .unwrap_or_default();
    let ours = (!base.is_empty()).then(|| Path::new(&base).join(&folder));
    if let Some(dir) = &ours
        && dir.is_dir()
        && crate::library::has_video(dir).await
    {
        return Err(format!(
            "'{folder}' holds real video files; not removing it"
        ));
    }

    let c = conn(st).await.ok_or("Jellyfin is not set up")?;
    let jf = match c.request(st, Method::DELETE, &format!("/Items/{id}"), &[]) {
        Ok(r) => match r.send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(format!("HTTP {}", r.status())),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e),
    };

    let mut notes = Vec::new();
    if let Some(dir) = &ours
        && dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(dir).await
    {
        notes.push(format!("couldn't delete {}: {e}", dir.display()));
    }
    let table = if item.is_movie {
        "library_movies"
    } else {
        "library_series"
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE folder = ?"
    )))
    .bind(&folder)
    .execute(&st.db)
    .await
    .map_err(|e| e.to_string())?
    .rows_affected();

    // Drop it from the cached snapshot so the page is right without a refetch.
    {
        let mut cache = st.jellyfin.lock().await;
        if let Some(s) = cache.as_ref() {
            let mut copy = Snapshot {
                fetched: s.fetched,
                users: s.users.clone(),
                items: s.items.iter().filter(|i| i.id != id).cloned().collect(),
                seasons: s.seasons.clone(),
            };
            copy.seasons.remove(id);
            *cache = Some(Arc::new(copy));
        }
    }

    let mut msg = format!("Removed '{}'", item.name);
    if rows > 0 {
        msg.push_str(" (also from M3Usort's library, so the VOD run won't add it again)");
    }
    if let Err(e) = jf {
        tracing::warn!("Jellyfin delete {id}: {e}");
        let mut log = crate::library::RunLog::default();
        crate::library::refresh_jellyfin(st, &mut log).await;
        msg = format!(
            "{msg}. Jellyfin couldn't delete it ({e}); the files are gone, so it drops out after Jellyfin's library scan"
        );
    }
    if !notes.is_empty() {
        msg = format!("{msg}. {}", notes.join("; "));
    }
    tracing::info!("{msg}");
    Ok(msg)
}

/// Seasons of one series from the snapshot (for the expand button).
pub async fn seasons(st: &AppState, id: &str) -> Result<Vec<Season>, String> {
    let snap = snapshot(st, false).await?;
    Ok(snap.seasons.get(id).cloned().unwrap_or_default())
}
