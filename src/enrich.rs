//! Metadata enrichment, as in the Python version:
//! - on demand when a detail page is opened (provider info, then TMDB), and
//! - two optional daily jobs (Settings → Schedules):
//!   "TMDB ID enrichment": get_vod_info / get_series_info at the provider, 1 s apart,
//!   only for titles without a TMDB id from the provider's bulk list;
//!   "Cast enrichment": TMDB search + details (cast, poster, plot, seasons, status),
//!   several requests in parallel, paced well under TMDB's limit.
//!
//! Titles that appear more than once (same name and year in NL, EN and 4K
//! categories) are looked up once and the answer is shared.
//!
//! "checked" timestamps mean "asked, even if nothing came back", so nothing is
//! asked twice. Network errors don't set them, so those are retried later.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::catalog::{self, Kind, now_secs};
use crate::integrations::{self, as_text};
use crate::library::{cut_quality_tags, strip_year};
use crate::settings as s;
use crate::tmdb;
use crate::web::AppState;

const PROVIDER_DELAY: Duration = Duration::from_secs(1); // as Python
/// Parallel TMDB lookups. TMDB allows 20 connections per IP; the request rate
/// itself is capped in tmdb.rs (40/s, under TMDB's ~50/s).
/// Provider answers per kind to look at before deciding it never sends TMDB ids.
const PROBE_LOOKUPS: usize = 50;
pub const TMDB_PARALLEL: usize = 8;
/// Stop a job after this many network errors in a row (provider or TMDB down).
const MAX_ERRORS_IN_A_ROW: usize = 20;

pub const JOB_PROVIDER: &str = "provider";
pub const JOB_TMDB: &str = "tmdb";

fn kind_parts(kind: Kind) -> (&'static str, &'static str, &'static str, bool) {
    match kind {
        Kind::Movie => ("movies", "stream_id", "get_vod_info", true),
        Kind::Series => ("series", "series_id", "get_series_info", false),
    }
}

async fn tmdb_key(st: &AppState) -> Option<String> {
    s::get_secret(&st.db, &st.key, s::SECRET_TMDB_API_KEY)
        .await
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
}

/// Copy a valid TMDB id from the provider's bulk list (get_vod_streams /
/// get_series) into tmdb_id, so those titles skip the slow provider lookup.
/// Never overwrites an id that is already there. Returns how many got one.
pub async fn adopt_bulk_ids(db: &sqlx::SqlitePool) -> anyhow::Result<u64> {
    let mut n = 0;
    for table in ["movies", "series"] {
        n += sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET tmdb_id = trim(provider_tmdb)
             WHERE (tmdb_id IS NULL OR tmdb_id = '')
               AND trim(provider_tmdb) GLOB '[1-9]*' AND trim(provider_tmdb) NOT GLOB '*[^0-9]*'"
        )))
        .execute(db)
        .await?
        .rows_affected();
    }
    Ok(n)
}

/// How many titles have a TMDB id from the provider's bulk list (for the Library page).
pub async fn bulk_id_counts(db: &sqlx::SqlitePool) -> anyhow::Result<BulkCounts> {
    let q = |table: &str| {
        sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*), COALESCE(SUM(trim(provider_tmdb) GLOB '[1-9]*' AND trim(provider_tmdb) NOT GLOB '*[^0-9]*'), 0),
                    COALESCE(SUM(tmdb_id IS NOT NULL AND tmdb_id != ''), 0) FROM {table}"
        ))
    };
    let (mt, mb, mi): (i64, i64, i64) = sqlx::query_as(q("movies")).fetch_one(db).await?;
    let (st, sb, si): (i64, i64, i64) = sqlx::query_as(q("series")).fetch_one(db).await?;
    Ok(BulkCounts {
        movies: mt,
        movies_bulk: mb,
        movies_with_id: mi,
        series: st,
        series_bulk: sb,
        series_with_id: si,
    })
}

#[derive(Default)]
pub struct BulkCounts {
    pub movies: i64,
    pub movies_bulk: i64,
    pub movies_with_id: i64,
    pub series: i64,
    pub series_bulk: i64,
    pub series_with_id: i64,
}

/// Strip provider prefixes like "NL | ", "EN: " or "4K - " before searching TMDB.
pub fn search_title(name: &str) -> (String, Option<i32>) {
    let mut t = name.trim();
    for sep in [" | ", "| ", ": ", " - "] {
        if let Some((pre, rest)) = t.split_once(sep)
            && pre.len() <= 4
            && pre
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            t = rest.trim();
            break;
        }
    }
    let (bare, year) = strip_year(&cut_quality_tags(t));
    let bare = bare.trim().to_string();
    (if bare.is_empty() { t.to_string() } else { bare }, year)
}

// ------------------------------------------------------------------ one item

/// Provider info (get_vod_info / get_series_info): TMDB id, IMDb id, plot, rating.
/// Ok(false) on a network error (not marked as checked, retried later).
async fn provider_info(st: &AppState, kind: Kind, id: i64) -> anyhow::Result<bool> {
    let (table, id_col, action, is_movie) = kind_parts(kind);
    let Some(x) = catalog::load_xtream(st).await? else {
        return Ok(false);
    };
    let mut url = x.api_url(action).map_err(anyhow::Error::msg)?;
    url.query_pairs_mut().append_pair(
        if is_movie { "vod_id" } else { "series_id" },
        &id.to_string(),
    );
    let resp = match st
        .http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            integrations::provider_user_agent(),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!("{action} {id}: HTTP {}", r.status());
            return Ok(false);
        }
        Err(e) => {
            tracing::debug!("{action} {id}: {e}");
            return Ok(false);
        }
    };
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let info = body
        .get("info")
        .filter(|i| i.is_object())
        .cloned()
        .unwrap_or(Value::Null);
    let pick = |keys: &[&str]| {
        keys.iter().find_map(|k| {
            info.get(*k)
                .map(as_text)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty() && v != "0")
        })
    };
    let tmdb_id = pick(&["tmdb_id", "tmdb"]).filter(|v| v.chars().all(|c| c.is_ascii_digit()));
    let imdb_id = pick(&["imdb_id", "imdb"]);
    let plot = pick(&["plot", "description", "overview"]);
    let rating = pick(&["rating"])
        .and_then(|r| r.parse::<f64>().ok())
        .filter(|r| *r > 0.0);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET tmdb_id = COALESCE(tmdb_id, ?), imdb_id = COALESCE(imdb_id, ?),
         plot = COALESCE(NULLIF(plot, ''), ?), rating = COALESCE(NULLIF(rating, 0), ?), provider_checked = ?
         WHERE {id_col} = ?"
    )))
    .bind(tmdb_id)
    .bind(imdb_id)
    .bind(plot)
    .bind(rating)
    .bind(now_secs())
    .bind(id)
    .execute(&st.db)
    .await?;
    Ok(true)
}

/// TMDB: search by name when there is no id yet, then details + cast.
/// Ok(false) on a network error.
async fn tmdb_info(st: &AppState, key: &str, kind: Kind, id: i64) -> anyhow::Result<bool> {
    let (table, id_col, _, is_movie) = kind_parts(kind);
    let (name, tmdb_id): (String, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT name, tmdb_id FROM {table} WHERE {id_col} = ?"
    )))
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    let tmdb_id = match tmdb_id.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => {
            let (title, year) = search_title(&name);
            match tmdb::search(
                &st.http,
                &integrations::tmdb_base(),
                key,
                is_movie,
                &title,
                year,
            )
            .await
            {
                Ok(Some((found, _))) => found.to_string(),
                Ok(None) => {
                    mark_tmdb_checked(st, kind, &[id]).await?; // nothing to find; don't ask again
                    return Ok(true);
                }
                Err(e) => {
                    tracing::debug!("TMDB search '{title}': {e}");
                    return Ok(false);
                }
            }
        }
    };
    fetch_details(st, key, kind, &tmdb_id, &[id]).await
}

/// TMDB details for one TMDB id, written to every catalog item that has it.
/// Ok(false) on a network error.
async fn fetch_details(
    st: &AppState,
    key: &str,
    kind: Kind,
    tmdb_id: &str,
    ids: &[i64],
) -> anyhow::Result<bool> {
    let (table, id_col, _, is_movie) = kind_parts(kind);
    let d = match tmdb::details(&st.http, &integrations::tmdb_base(), key, is_movie, tmdb_id).await
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            mark_tmdb_checked(st, kind, ids).await?;
            return Ok(true);
        }
        Err(e) => {
            tracing::debug!("TMDB details {tmdb_id}: {e}");
            return Ok(false);
        }
    };
    let mut tx = st.db.begin().await?;
    for &id in ids {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET tmdb_id = ?, poster = COALESCE(?, poster), plot = COALESCE(NULLIF(plot, ''), ?),
             rating = COALESCE(NULLIF(rating, 0), ?), imdb_id = COALESCE(imdb_id, ?), tmdb_checked = ? WHERE {id_col} = ?"
        )))
        .bind(tmdb_id)
        .bind(&d.poster)
        .bind(&d.plot)
        .bind(d.rating)
        .bind(&d.imdb_id)
        .bind(now_secs())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if !is_movie {
            sqlx::query("UPDATE series SET seasons = ?, status = ? WHERE series_id = ?")
                .bind(d.seasons)
                .bind(&d.status)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM cast_members WHERE kind = ? AND item_id = ?")
            .bind(kind.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (i, p) in d.cast.iter().enumerate() {
            sqlx::query("INSERT OR IGNORE INTO cast_members (kind, item_id, person_id, name, character, ord) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(kind.as_str())
                .bind(id)
                .bind(p.id)
                .bind(&p.name)
                .bind(&p.character)
                .bind(i as i64)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(true)
}

async fn mark_tmdb_checked(st: &AppState, kind: Kind, ids: &[i64]) -> anyhow::Result<()> {
    let (table, id_col, _, _) = kind_parts(kind);
    let mut tx = st.db.begin().await?;
    for &id in ids {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET tmdb_checked = ? WHERE {id_col} = ?"
        )))
        .bind(now_secs())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Called when a detail page opens: fill in whatever hasn't been asked yet.
/// Bounded in time so a slow provider or TMDB never hangs the page.
pub async fn ensure_details(st: &AppState, kind: Kind, id: i64) {
    let work = async {
        let (table, id_col, _, _) = kind_parts(kind);
        let row: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT provider_checked, tmdb_checked FROM {table} WHERE {id_col} = ?"
        )))
        .bind(id)
        .fetch_optional(&st.db)
        .await
        .ok()
        .flatten();
        let Some((provider_checked, tmdb_checked)) = row else {
            return;
        };
        if provider_checked.is_none() {
            let _ = provider_info(st, kind, id).await;
        }
        if tmdb_checked.is_none()
            && let Some(key) = tmdb_key(st).await
        {
            let _ = tmdb_info(st, &key, kind, id).await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(12), work)
        .await
        .is_err()
    {
        tracing::debug!("detail enrichment for {} {id} timed out", kind.as_str());
    }
}

// ------------------------------------------------------------------ background jobs

#[derive(Default, Clone, Serialize)]
pub struct JobStatus {
    pub running: bool,
    pub processed: i64,
    pub total: i64,
    pub last: String,
    pub message: String,
    pub failed: bool,
}

/// (started, finished, ok, processed, total, message)
type RunRow = (i64, Option<i64>, i64, i64, i64, Option<String>);

pub async fn job_status(st: &AppState, job: &str) -> JobStatus {
    let row: Option<RunRow> = sqlx::query_as(
        "SELECT started, finished, ok, processed, total, message FROM enrich_runs WHERE job = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(job)
    .fetch_optional(&st.db)
    .await
    .ok()
    .flatten();
    let running = running_flag(st, job).load(Ordering::SeqCst);
    match row {
        None => JobStatus {
            running,
            last: "never".into(),
            ..Default::default()
        },
        Some((started, finished, ok, processed, total, message)) => JobStatus {
            running,
            processed,
            total,
            last: catalog::ago(finished.unwrap_or(started)),
            message: message.unwrap_or_default(),
            failed: finished.is_some() && ok == 0,
        },
    }
}

fn running_flag<'a>(st: &'a AppState, job: &str) -> &'a AtomicBool {
    if job == JOB_PROVIDER {
        &st.enrich_provider_running
    } else {
        &st.enrich_tmdb_running
    }
}

/// One lookup that covers one or more catalog items of the same kind.
struct Group {
    kind: Kind,
    /// Search: (title, year). Details: the TMDB id (title unused).
    title: String,
    year: Option<i32>,
    tmdb_id: Option<String>,
    /// Catalog ids, library items first, then newest.
    ids: Vec<i64>,
}

/// (id, name, tmdb_id) of items still to do, library items first, then newest.
async fn todo_rows(
    st: &AppState,
    kind: Kind,
    cond: &str,
) -> anyhow::Result<Vec<(i64, String, Option<String>)>> {
    let (table, id_col, _, _) = kind_parts(kind);
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT t.{id_col}, t.name, NULLIF(t.tmdb_id, '') FROM {table} t
         WHERE {cond}
         ORDER BY EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col}) DESC, t.added DESC"
    )))
    .fetch_all(&st.db)
    .await?)
}

/// Group rows by a key, keeping the order of first appearance.
fn group_by<K: std::hash::Hash + Eq>(
    rows: impl IntoIterator<Item = (K, i64, Group)>,
) -> Vec<Group> {
    let mut index: HashMap<K, usize> = HashMap::new();
    let mut out: Vec<Group> = Vec::new();
    for (key, id, g) in rows {
        match index.get(&key) {
            Some(&i) => out[i].ids.push(id),
            None => {
                index.insert(key, out.len());
                out.push(g);
            }
        }
    }
    out
}

/// Search groups: same kind + cleaned title + year = same TMDB query = same answer.
async fn search_groups(st: &AppState) -> anyhow::Result<Vec<Group>> {
    let mut rows = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        for (id, name, _) in todo_rows(
            st,
            kind,
            "t.tmdb_checked IS NULL AND (t.tmdb_id IS NULL OR t.tmdb_id = '')",
        )
        .await?
        {
            let (title, year) = search_title(&name);
            let key = (kind.as_str(), title.to_lowercase(), year);
            rows.push((
                key,
                id,
                Group {
                    kind,
                    title,
                    year,
                    tmdb_id: None,
                    ids: vec![id],
                },
            ));
        }
    }
    Ok(group_by(rows))
}

/// Details groups: one request per TMDB id.
async fn details_groups(st: &AppState) -> anyhow::Result<Vec<Group>> {
    let mut rows = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        for (id, _, tmdb_id) in todo_rows(
            st,
            kind,
            "t.tmdb_checked IS NULL AND t.tmdb_id IS NOT NULL AND t.tmdb_id != ''",
        )
        .await?
        {
            let Some(t) = tmdb_id else { continue };
            let key = (kind.as_str(), t.clone());
            rows.push((
                key,
                id,
                Group {
                    kind,
                    title: String::new(),
                    year: None,
                    tmdb_id: Some(t),
                    ids: vec![id],
                },
            ));
        }
    }
    Ok(group_by(rows))
}

/// Provider groups. Only titles with a year are grouped ("Dune (2021)" in NL and
/// EN is one film; a bare "Dune" could be 1984 or 2021). Titles with a TMDB id
/// (from the bulk list or earlier) are skipped.
async fn provider_groups(st: &AppState) -> anyhow::Result<Vec<Group>> {
    let mut rows = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        for (id, name, _) in todo_rows(
            st,
            kind,
            "t.provider_checked IS NULL AND (t.tmdb_id IS NULL OR t.tmdb_id = '')",
        )
        .await?
        {
            let (title, year) = search_title(&name);
            let key = match year {
                Some(y) => (kind.as_str(), title.to_lowercase(), y, 0),
                None => (kind.as_str(), String::new(), 0, id),
            };
            rows.push((
                key,
                id,
                Group {
                    kind,
                    title,
                    year,
                    tmdb_id: None,
                    ids: vec![id],
                },
            ));
        }
    }
    Ok(group_by(rows))
}

/// Run one job to completion. Only one run per job at a time.
pub async fn run_job(st: &AppState, job: &'static str) -> anyhow::Result<String> {
    let flag = running_flag(st, job);
    if flag.swap(true, Ordering::SeqCst) {
        anyhow::bail!("already running");
    }
    let result = run_job_inner(st, job).await;
    flag.store(false, Ordering::SeqCst);
    result
}

/// Shared state of one job run, for the workers and the progress writer.
struct Progress {
    run_id: i64,
    /// Items handled (done or failed); shown as "processed / total".
    processed: AtomicI64,
    /// Items that got an answer (even "not found").
    done: AtomicI64,
    errors_in_a_row: AtomicUsize,
    stopped: AtomicBool,
}

impl Progress {
    fn ok(&self, items: usize, counted: bool) {
        self.errors_in_a_row.store(0, Ordering::SeqCst);
        if counted {
            self.processed.fetch_add(items as i64, Ordering::SeqCst);
            self.done.fetch_add(items as i64, Ordering::SeqCst);
        }
    }
    fn failed(&self, items: usize) {
        self.processed.fetch_add(items as i64, Ordering::SeqCst);
        if self.errors_in_a_row.fetch_add(1, Ordering::SeqCst) + 1 >= MAX_ERRORS_IN_A_ROW {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }
    async fn save(&self, db: &sqlx::SqlitePool) {
        let _ = sqlx::query("UPDATE enrich_runs SET processed = ? WHERE id = ?")
            .bind(self.processed.load(Ordering::SeqCst))
            .bind(self.run_id)
            .execute(db)
            .await;
    }
}

async fn run_job_inner(st: &AppState, job: &'static str) -> anyhow::Result<String> {
    let key = tmdb_key(st).await;
    if job == JOB_TMDB && key.is_none() {
        anyhow::bail!("no TMDB API key set");
    }
    adopt_bulk_ids(&st.db).await?;
    let (first, items) = if job == JOB_PROVIDER {
        let g = provider_groups(st).await?;
        let n: usize = g.iter().map(|g| g.ids.len()).sum();
        (g, n)
    } else {
        // Total = everything without TMDB details yet; searches that find an id
        // are counted when their details arrive.
        let g = search_groups(st).await?;
        let with_id: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM movies WHERE tmdb_checked IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != '')
                  + (SELECT COUNT(*) FROM series WHERE tmdb_checked IS NULL AND tmdb_id IS NOT NULL AND tmdb_id != '')",
        )
        .fetch_one(&st.db)
        .await?;
        let n = g.iter().map(|g| g.ids.len()).sum::<usize>() + with_id as usize;
        (g, n)
    };
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO enrich_runs (job, started, total) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(job)
    .bind(now_secs())
    .bind(items as i64)
    .fetch_one(&st.db)
    .await?;
    let lookups = first.len();
    tracing::info!(
        "{} enrichment: {items} item(s) to do",
        if job == JOB_PROVIDER {
            "TMDB ID"
        } else {
            "TMDB details"
        },
    );
    let p = Arc::new(Progress {
        run_id,
        processed: AtomicI64::new(0),
        done: AtomicI64::new(0),
        errors_in_a_row: AtomicUsize::new(0),
        stopped: AtomicBool::new(false),
    });
    let writer = {
        let (db, p) = (st.db.clone(), p.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                p.save(&db).await;
            }
        })
    };
    let mut skipped = Vec::new();
    let result = if job == JOB_PROVIDER {
        run_provider(st, first, &p).await.map(|s| skipped = s)
    } else {
        let key = key.unwrap_or_default();
        match run_tmdb_pool(st, &key, first, &p).await {
            Ok(()) if !p.stopped.load(Ordering::SeqCst) => {
                let second = details_groups(st).await?;
                run_tmdb_pool(st, &key, second, &p).await
            }
            r => r,
        }
    };
    writer.abort();
    p.save(&st.db).await;
    result?;
    let done = p.done.load(Ordering::SeqCst);
    let mut msg = if p.stopped.load(Ordering::SeqCst) {
        format!(
            "{done} of {items} done; stopped after {MAX_ERRORS_IN_A_ROW} failed requests in a row (rest is retried next run)"
        )
    } else if job == JOB_PROVIDER && lookups < items {
        format!("{done} of {items} done ({lookups} lookups; duplicates shared)")
    } else {
        format!("{done} of {items} done")
    };
    if !skipped.is_empty() {
        let parts: Vec<String> = skipped
            .iter()
            .map(|(k, n)| {
                format!(
                    "{n} {}",
                    if *k == Kind::Movie {
                        "movies"
                    } else {
                        "series"
                    }
                )
            })
            .collect();
        msg = format!(
            "{done} of {items} done; skipped {}: no TMDB id in the first {PROBE_LOOKUPS} provider answers (the Cast job finds them by name)",
            parts.join(" and ")
        );
    }
    sqlx::query("UPDATE enrich_runs SET finished = ?, ok = ?, message = ? WHERE id = ?")
        .bind(now_secs())
        .bind(i64::from(!p.stopped.load(Ordering::SeqCst)))
        .bind(&msg)
        .bind(run_id)
        .execute(&st.db)
        .await?;
    sqlx::query("DELETE FROM enrich_runs WHERE id <= (SELECT MAX(id) - 100 FROM enrich_runs)")
        .execute(&st.db)
        .await?;
    tracing::info!("enrichment '{job}': {msg}");
    Ok(msg)
}

/// Provider lookups, one per second (as Python). Per group: ask the first item;
/// if the provider has no TMDB id for it, try the next one; share the answer.
///
/// If the first PROBE_LOOKUPS answers for a kind hold no TMDB id at all, the rest
/// of that kind is skipped for this run: the provider (or a proxy in between,
/// e.g. iptv-proxy drops the field from get_series_info) doesn't send it, so
/// thousands of 1-per-second calls would gain nothing. The next run probes again
/// with new titles, so a fixed proxy is picked up by itself.
/// Returns the kinds that were skipped and how many items each.
async fn run_provider(
    st: &AppState,
    groups: Vec<Group>,
    p: &Progress,
) -> anyhow::Result<Vec<(Kind, usize)>> {
    // At most one request per second, one at a time. The second counts from the
    // start of the previous request, so a slow answer (proxy + VPN) isn't
    // followed by another full second of waiting (Python slept 1 s after each).
    let mut next_start = tokio::time::Instant::now();
    // per kind: (answers, answers with an id)
    let mut probe: HashMap<&'static str, (usize, usize)> = HashMap::new();
    let mut skipped: Vec<(Kind, usize)> = Vec::new();
    for g in groups {
        if let Some(s) = skipped.iter_mut().find(|(k, _)| *k == g.kind) {
            s.1 += g.ids.len();
            p.processed.fetch_add(g.ids.len() as i64, Ordering::SeqCst);
            continue;
        }
        let (answers, with_id) = probe.get(g.kind.as_str()).copied().unwrap_or_default();
        if answers >= PROBE_LOOKUPS && with_id == 0 {
            tracing::info!(
                "provider sent no TMDB id in {answers} {} lookups; skipping the rest of them this run",
                g.kind.as_str()
            );
            skipped.push((g.kind, g.ids.len()));
            p.processed.fetch_add(g.ids.len() as i64, Ordering::SeqCst);
            continue;
        }
        let (table, id_col, _, _) = kind_parts(g.kind);
        let mut found: Option<(String, Option<String>)> = None;
        let mut asked = 0;
        for &id in &g.ids {
            if p.stopped.load(Ordering::SeqCst) {
                return Ok(skipped);
            }
            tokio::time::sleep_until(next_start).await;
            next_start = tokio::time::Instant::now() + PROVIDER_DELAY;
            asked += 1;
            if !provider_info(st, g.kind, id).await? {
                p.failed(1);
                continue;
            }
            p.ok(1, true);
            let counts = probe.entry(g.kind.as_str()).or_default();
            counts.0 += 1;
            let row: (Option<String>, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(
                format!("SELECT NULLIF(tmdb_id, ''), imdb_id FROM {table} WHERE {id_col} = ?"),
            ))
            .bind(id)
            .fetch_one(&st.db)
            .await?;
            if let (Some(t), imdb) = row {
                counts.1 += 1;
                found = Some((t, imdb));
                break;
            }
        }
        let rest = &g.ids[asked..];
        if let (Some((t, imdb)), false) = (found, rest.is_empty()) {
            let mut tx = st.db.begin().await?;
            for &id in rest {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE {table} SET tmdb_id = COALESCE(NULLIF(tmdb_id, ''), ?), imdb_id = COALESCE(imdb_id, ?), provider_checked = ? WHERE {id_col} = ?"
                )))
                .bind(&t)
                .bind(&imdb)
                .bind(now_secs())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            p.ok(rest.len(), true);
        }
    }
    Ok(skipped)
}

/// TMDB lookups (searches or details) on a few parallel workers. A search that finds an id isn't counted yet: its details are.
async fn run_tmdb_pool(
    st: &AppState,
    key: &str,
    groups: Vec<Group>,
    p: &Arc<Progress>,
) -> anyhow::Result<()> {
    let queue = Arc::new(Mutex::new(VecDeque::from(groups)));
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..TMDB_PARALLEL {
        let (st, key, queue, p) = (st.clone(), key.to_string(), queue.clone(), p.clone());
        workers.spawn(async move {
            loop {
                if p.stopped.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let Some(g) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else {
                    return Ok(());
                };
                tmdb_group(&st, &key, g, &p).await?;
            }
        });
    }
    let mut first_err = None;
    while let Some(r) = workers.join_next().await {
        let r = r.map_err(anyhow::Error::from).and_then(|r| r);
        if let (Err(e), None) = (r, &first_err) {
            p.stopped.store(true, Ordering::SeqCst); // stop the other workers too
            first_err = Some(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

async fn tmdb_group(st: &AppState, key: &str, g: Group, p: &Progress) -> anyhow::Result<()> {
    let (table, id_col, _, is_movie) = kind_parts(g.kind);
    let n = g.ids.len();
    if let Some(t) = &g.tmdb_id {
        match fetch_details(st, key, g.kind, t, &g.ids).await? {
            true => p.ok(n, true),
            false => p.failed(n),
        }
        return Ok(());
    }
    match tmdb::search(
        &st.http,
        &integrations::tmdb_base(),
        key,
        is_movie,
        &g.title,
        g.year,
    )
    .await
    {
        Ok(Some((found, _))) => {
            let mut tx = st.db.begin().await?;
            for &id in &g.ids {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE {table} SET tmdb_id = ? WHERE {id_col} = ?"
                )))
                .bind(found.to_string())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            p.ok(n, false); // counted when the details arrive
        }
        Ok(None) => {
            mark_tmdb_checked(st, g.kind, &g.ids).await?;
            p.ok(n, true);
        }
        Err(e) => {
            tracing::debug!("TMDB search '{}': {e}", g.title);
            p.failed(n);
        }
    }
    Ok(())
}

/// Start the daily jobs at their local time (Settings → Schedules), once per day.
pub async fn maybe_run_daily(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done() || st.catalog_running.load(Ordering::SeqCst) {
        return Ok(());
    }
    let has_catalog: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM catalog_runs WHERE ok = 1")
        .fetch_one(&st.db)
        .await?;
    if has_catalog == 0 {
        return Ok(());
    }
    for (job, enabled_key, time_key) in [
        (JOB_PROVIDER, s::TMDB_ENRICH_ENABLED, s::TMDB_ENRICH_TIME),
        (JOB_TMDB, s::CAST_ENRICH_ENABLED, s::CAST_ENRICH_TIME),
    ] {
        if s::get(&st.db, enabled_key).await?.as_deref() != Some("1")
            || running_flag(st, job).load(Ordering::SeqCst)
        {
            continue;
        }
        let at = s::get(&st.db, time_key)
            .await?
            .unwrap_or_else(|| "03:00".into());
        if crate::localtime::hh_mm() < at {
            continue;
        }
        let last: Option<i64> =
            sqlx::query_scalar("SELECT MAX(started) FROM enrich_runs WHERE job = ?")
                .bind(job)
                .fetch_one(&st.db)
                .await?;
        if last.is_some_and(|t| crate::localtime::date(t) == crate::localtime::today()) {
            continue;
        }
        let st2 = st.clone();
        tokio::spawn(async move {
            if let Err(e) = run_job(&st2, job).await {
                tracing::warn!("enrichment '{job}': {e:#}");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_titles() {
        assert_eq!(
            search_title("NL | Bitterzoet (2024)"),
            ("Bitterzoet".into(), Some(2024))
        );
        assert_eq!(
            search_title("EN: The Batman (2022) 4K HDR"),
            ("The Batman".into(), Some(2022))
        );
        assert_eq!(
            search_title("Spider-Man: No Way Home (2021)"),
            ("Spider-Man: No Way Home".into(), Some(2021))
        );
        assert_eq!(search_title("1923").0, "1923");
    }

    #[test]
    fn grouping_keeps_first_order() {
        let g = |id| Group {
            kind: Kind::Movie,
            title: String::new(),
            year: None,
            tmdb_id: None,
            ids: vec![id],
        };
        let out = group_by([
            ("b", 1, g(1)),
            ("a", 2, g(2)),
            ("b", 3, g(3)),
            ("a", 4, g(4)),
            ("c", 5, g(5)),
        ]);
        let ids: Vec<Vec<i64>> = out.iter().map(|g| g.ids.clone()).collect();
        assert_eq!(ids, vec![vec![1, 3], vec![2, 4], vec![5]]);
    }

    #[tokio::test]
    async fn bulk_ids_are_adopted_once_and_only_when_valid() {
        let dir = std::env::temp_dir().join(format!("m3usort-enr-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        sqlx::query(
            "INSERT INTO movies (stream_id, name, provider_tmdb, tmdb_id, first_seen, last_seen) VALUES
             (1, 'a', '155', NULL, 0, 0), (2, 'b', '0', NULL, 0, 0), (3, 'c', '', NULL, 0, 0),
             (4, 'd', 'tt123', NULL, 0, 0), (5, 'e', ' 27205 ', NULL, 0, 0), (6, 'f', '999', '42', 0, 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO series (series_id, name, provider_tmdb, first_seen, last_seen) VALUES (7, 's', '1396', 0, 0)")
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(adopt_bulk_ids(&db).await.unwrap(), 3);
        assert_eq!(adopt_bulk_ids(&db).await.unwrap(), 0);
        let ids: Vec<Option<String>> =
            sqlx::query_scalar("SELECT tmdb_id FROM movies ORDER BY stream_id")
                .fetch_all(&db)
                .await
                .unwrap();
        let ids: Vec<Option<&str>> = ids.iter().map(|o| o.as_deref()).collect();
        assert_eq!(
            ids,
            vec![Some("155"), None, None, None, Some("27205"), Some("42")]
        );
        let c = bulk_id_counts(&db).await.unwrap();
        assert_eq!(
            (c.movies, c.movies_bulk, c.movies_with_id, c.series_bulk),
            (6, 3, 3, 1)
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
