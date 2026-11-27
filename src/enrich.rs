//! Metadata enrichment. Only real TMDB ids are used, never a guess from a
//! name search ("tmdb_source" records where each id came from):
//!   1. the provider's bulk list (get_vod_streams / get_series), when it has one;
//!   2. the provider's get_vod_info / get_series_info ("TMDB ID enrichment",
//!      one request per second, one at a time);
//!   3. Jellyfin's match for items in the library (ProviderIds.Tmdb), read at
//!      the start of "Cast enrichment" (Jellyfin matches are fixed with Identify).
//!
//! "Cast enrichment" then fetches TMDB details (cast, poster, plot, seasons,
//! status) by id: once per TMDB id, a few in parallel, capped app-wide in tmdb.rs.
//! Titles without an id keep the provider's own poster, plot and rating.
//!
//! "checked" timestamps mean "asked, even if nothing came back", so nothing is
//! asked twice. Network errors don't set them, so those are retried later.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sqlx::SqliteConnection;

use crate::catalog::{self, Kind, now_secs};
use crate::integrations::{self, as_text};
use crate::settings as s;
use crate::tmdb;
use crate::web::AppState;

/// At most one provider request per second, one at a time (as Python).
const PROVIDER_DELAY: Duration = Duration::from_secs(1);
/// Provider answers per kind to look at before deciding it never sends TMDB ids.
const PROBE_LOOKUPS: usize = 50;
/// Parallel TMDB detail requests. TMDB allows 20 connections per IP; the request
/// rate itself is capped in tmdb.rs (40/s, under TMDB's ~50/s).
pub const TMDB_PARALLEL: usize = 8;
/// Stop a job after this many network errors in a row (provider or TMDB down).
const MAX_ERRORS_IN_A_ROW: usize = 20;

pub const JOB_PROVIDER: &str = "provider";
pub const JOB_TMDB: &str = "tmdb";

pub const SRC_LIST: &str = "list";
pub const SRC_PROVIDER: &str = "provider";
pub const SRC_JELLYFIN: &str = "jellyfin";
pub const SRC_UNVERIFIED: &str = "unverified";

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

/// A TMDB id as providers and Jellyfin send it: digits, not 0.
pub fn clean_tmdb_id(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty()
        && v.chars().all(|c| c.is_ascii_digit())
        && !v.trim_start_matches('0').is_empty())
    .then(|| v.trim_start_matches('0').to_string())
}

/// Copy a valid TMDB id from the provider's bulk list into tmdb_id. Never
/// overwrites a provider id; replaces a Jellyfin or unverified one only when it
/// differs (then the details are fetched again). Returns how many changed.
pub async fn adopt_bulk_ids(db: &sqlx::SqlitePool) -> anyhow::Result<u64> {
    let mut n = 0;
    for kind in [Kind::Movie, Kind::Series] {
        let (table, id_col, _, _) = kind_parts(kind);
        let rows: Vec<(i64, String, Option<String>, Option<String>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {id_col}, provider_tmdb, NULLIF(tmdb_id, ''), tmdb_source FROM {table}
                 WHERE trim(provider_tmdb) GLOB '[1-9]*' AND trim(provider_tmdb) NOT GLOB '*[^0-9]*'
                   AND (tmdb_source IS NULL OR tmdb_source NOT IN ('list', 'provider'))"
            )))
            .fetch_all(db)
            .await?;
        let mut tx = db.begin().await?;
        for (id, bulk, cur, _) in rows {
            let Some(bulk) = clean_tmdb_id(&bulk) else {
                continue;
            };
            set_id(&mut tx, kind, id, &bulk, cur.as_deref(), SRC_LIST).await?;
            n += 1;
        }
        tx.commit().await?;
    }
    Ok(n)
}

/// Set an item's TMDB id and its source. When the id changes, everything TMDB
/// filled in for the old one is cleared so the Cast job fetches it again.
pub(crate) async fn set_id(
    conn: &mut SqliteConnection,
    kind: Kind,
    id: i64,
    new: &str,
    current: Option<&str>,
    source: &str,
) -> anyhow::Result<()> {
    let (table, id_col, _, _) = kind_parts(kind);
    if current != Some(new) && current.is_some() {
        clear_tmdb_details(conn, kind, id, true).await?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET tmdb_id = ?, tmdb_source = ? WHERE {id_col} = ?"
    )))
    .bind(new)
    .bind(source)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Remove what TMDB filled in for an item (poster, IMDb id, seasons, status,
/// cast and, when asked, the plot), so it is fetched again or left empty.
pub async fn clear_tmdb_details(
    conn: &mut SqliteConnection,
    kind: Kind,
    id: i64,
    plot: bool,
) -> anyhow::Result<()> {
    let (table, id_col, _, is_movie) = kind_parts(kind);
    let extra = match (is_movie, plot) {
        (true, true) => ", plot = NULL",
        (true, false) => "",
        (false, true) => ", plot = NULL, seasons = NULL, status = NULL",
        (false, false) => ", seasons = NULL, status = NULL",
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET poster = NULL, imdb_id = NULL, tmdb_checked = NULL{extra} WHERE {id_col} = ?"
    )))
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM cast_members WHERE kind = ? AND item_id = ?")
        .bind(kind.as_str())
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Where the TMDB ids come from, for the Library page.
#[derive(Default)]
pub struct IdCounts {
    pub movies: i64,
    pub movies_provider: i64,
    pub movies_jellyfin: i64,
    pub movies_unverified: i64,
    pub series: i64,
    pub series_provider: i64,
    pub series_jellyfin: i64,
    pub series_unverified: i64,
}

pub async fn id_counts(db: &sqlx::SqlitePool) -> anyhow::Result<IdCounts> {
    let q = |table: &str| {
        sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*),
                    COALESCE(SUM(tmdb_source IN ('list', 'provider')), 0),
                    COALESCE(SUM(tmdb_source = 'jellyfin'), 0),
                    COALESCE(SUM(tmdb_source = 'unverified'), 0) FROM {table}"
        ))
    };
    let (m, mp, mj, mu): (i64, i64, i64, i64) = sqlx::query_as(q("movies")).fetch_one(db).await?;
    let (s, sp, sj, su): (i64, i64, i64, i64) = sqlx::query_as(q("series")).fetch_one(db).await?;
    Ok(IdCounts {
        movies: m,
        movies_provider: mp,
        movies_jellyfin: mj,
        movies_unverified: mu,
        series: s,
        series_provider: sp,
        series_jellyfin: sj,
        series_unverified: su,
    })
}

// ------------------------------------------------------------------ one item

/// Provider info (get_vod_info / get_series_info): TMDB id, IMDb id, plot, rating.
/// The provider's TMDB id wins over any other source. An unverified id the
/// provider doesn't confirm is removed with what TMDB filled in for it.
/// Ok(None) on a network error (not marked as checked, retried later);
/// Ok(Some(had_id)) when the provider answered.
async fn provider_info(st: &AppState, kind: Kind, id: i64) -> anyhow::Result<Option<bool>> {
    let (table, id_col, action, is_movie) = kind_parts(kind);
    let Some(x) = catalog::load_xtream(st).await? else {
        return Ok(None);
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
            return Ok(None);
        }
        Err(e) => {
            tracing::debug!("{action} {id}: {e}");
            return Ok(None);
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
    let tmdb_id = pick(&["tmdb_id", "tmdb"]).and_then(|v| clean_tmdb_id(&v));
    let imdb_id = pick(&["imdb_id", "imdb"]);
    let plot = pick(&["plot", "description", "overview"]);
    let rating = pick(&["rating"])
        .and_then(|r| r.parse::<f64>().ok())
        .filter(|r| *r > 0.0);

    let (cur, src): (Option<String>, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(
        format!("SELECT NULLIF(tmdb_id, ''), tmdb_source FROM {table} WHERE {id_col} = ?"),
    ))
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    let mut tx = st.db.begin().await?;
    match &tmdb_id {
        Some(t) => set_id(&mut tx, kind, id, t, cur.as_deref(), SRC_PROVIDER).await?,
        None if src.as_deref() == Some(SRC_UNVERIFIED) => {
            // Not confirmed: it came from a name search. Remove it.
            clear_tmdb_details(&mut tx, kind, id, true).await?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET tmdb_id = NULL, tmdb_source = NULL WHERE {id_col} = ?"
            )))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        None => {}
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET imdb_id = COALESCE(?, imdb_id), plot = COALESCE(NULLIF(plot, ''), ?),
         rating = COALESCE(NULLIF(rating, 0), ?), provider_checked = ? WHERE {id_col} = ?"
    )))
    .bind(imdb_id)
    .bind(plot)
    .bind(rating)
    .bind(now_secs())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(tmdb_id.is_some()))
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
            "UPDATE {table} SET poster = COALESCE(?, poster), plot = COALESCE(NULLIF(plot, ''), ?),
             rating = COALESCE(NULLIF(rating, 0), ?), imdb_id = COALESCE(imdb_id, ?), tmdb_checked = ?
             WHERE {id_col} = ? AND tmdb_id = ?"
        )))
        .bind(&d.poster)
        .bind(&d.plot)
        .bind(d.rating)
        .bind(&d.imdb_id)
        .bind(now_secs())
        .bind(id)
        .bind(tmdb_id)
        .execute(&mut *tx)
        .await?;
        if !is_movie {
            sqlx::query(
                "UPDATE series SET seasons = ?, status = ? WHERE series_id = ? AND tmdb_id = ?",
            )
            .bind(d.seasons)
            .bind(&d.status)
            .bind(id)
            .bind(tmdb_id)
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

/// Called when a detail page opens: ask the provider if it hasn't been asked,
/// then TMDB details when there is a real id. Bounded in time so a slow
/// provider or TMDB never hangs the page.
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
            let tmdb_id: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT NULLIF(tmdb_id, '') FROM {table} WHERE {id_col} = ?"
            )))
            .bind(id)
            .fetch_one(&st.db)
            .await
            .ok()
            .flatten();
            if let Some(t) = tmdb_id {
                let _ = fetch_details(st, &key, kind, &t, &[id]).await;
            }
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

/// Provider job: items without an id, or with an unverified one. Library items
/// first, then unverified ones, then newest.
async fn provider_todo(st: &AppState) -> anyhow::Result<Vec<(Kind, i64)>> {
    let mut out = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        let (table, id_col, _, _) = kind_parts(kind);
        let ids: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t.{id_col} FROM {table} t
             WHERE t.provider_checked IS NULL
               AND (t.tmdb_id IS NULL OR t.tmdb_id = '' OR t.tmdb_source = 'unverified')
             ORDER BY EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col}) DESC,
                      t.tmdb_source = 'unverified' DESC, t.added DESC"
        )))
        .fetch_all(&st.db)
        .await?;
        out.extend(ids.into_iter().map(|i| (kind, i)));
    }
    Ok(out)
}

/// One TMDB details request, written to every catalog item with that id.
struct Group {
    kind: Kind,
    tmdb_id: String,
    ids: Vec<i64>,
}

/// Cast job: items with an id and no details yet, one request per TMDB id,
/// library items first, then newest.
async fn details_groups(st: &AppState) -> anyhow::Result<Vec<Group>> {
    let mut out: Vec<Group> = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        let (table, id_col, _, _) = kind_parts(kind);
        let rows: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT t.{id_col}, t.tmdb_id FROM {table} t
             WHERE t.tmdb_checked IS NULL AND t.tmdb_id IS NOT NULL AND t.tmdb_id != ''
             ORDER BY EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col}) DESC, t.added DESC"
        )))
        .fetch_all(&st.db)
        .await?;
        let mut index: HashMap<String, usize> = HashMap::new();
        for (id, t) in rows {
            match index.get(&t) {
                Some(&i) => out[i].ids.push(id),
                None => {
                    index.insert(t.clone(), out.len());
                    out.push(Group {
                        kind,
                        tmdb_id: t,
                        ids: vec![id],
                    });
                }
            }
        }
    }
    Ok(out)
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
    /// Items that got an answer (even "no id").
    done: AtomicI64,
    errors_in_a_row: AtomicUsize,
    stopped: AtomicBool,
}

impl Progress {
    fn ok(&self, items: usize) {
        self.errors_in_a_row.store(0, Ordering::SeqCst);
        self.processed.fetch_add(items as i64, Ordering::SeqCst);
        self.done.fetch_add(items as i64, Ordering::SeqCst);
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
    let mut notes: Vec<String> = Vec::new();
    // Provider job: per item. Cast job: Jellyfin ids first, then per TMDB id.
    let (provider_items, groups) = if job == JOB_PROVIDER {
        (provider_todo(st).await?, Vec::new())
    } else {
        match crate::jellyfin::sync_ids(st).await {
            Ok(Some((changed, seen))) => {
                tracing::info!(
                    "Jellyfin: {seen} library item(s) with a TMDB id, {changed} new or changed"
                );
                if changed > 0 {
                    notes.push(format!("{changed} id(s) from Jellyfin"));
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("Jellyfin ids: {e}");
                notes.push(format!("Jellyfin ids not read: {e}"));
            }
        }
        (Vec::new(), details_groups(st).await?)
    };
    let items = provider_items.len() + groups.iter().map(|g| g.ids.len()).sum::<usize>();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO enrich_runs (job, started, total) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(job)
    .bind(now_secs())
    .bind(items as i64)
    .fetch_one(&st.db)
    .await?;
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
    let mut found = 0usize;
    let result = if job == JOB_PROVIDER {
        run_provider(st, provider_items, &p).await.map(|(s, f)| {
            skipped = s;
            found = f;
        })
    } else {
        run_tmdb_pool(st, &key.unwrap_or_default(), groups, &p).await
    };
    writer.abort();
    p.save(&st.db).await;
    result?;
    let done = p.done.load(Ordering::SeqCst);
    let mut msg = if p.stopped.load(Ordering::SeqCst) {
        format!(
            "{done} of {items} done; stopped after {MAX_ERRORS_IN_A_ROW} failed requests in a row (rest is retried next run)"
        )
    } else {
        format!("{done} of {items} done")
    };
    if job == JOB_PROVIDER && done > 0 {
        msg.push_str(&format!("; {found} with a TMDB id"));
    }
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
        msg.push_str(&format!(
            "; skipped {}: no TMDB id in the first {PROBE_LOOKUPS} provider answers",
            parts.join(" and ")
        ));
    }
    for n in &notes {
        msg.push_str("; ");
        msg.push_str(n);
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

/// Provider lookups: one request per second, one at a time, every item asked
/// itself (no sharing between titles with the same name: only the provider's
/// own answer counts).
///
/// If the first PROBE_LOOKUPS answers for a kind hold no TMDB id at all, the rest
/// of that kind is skipped for this run: the provider doesn't send it there (e.g.
/// get_series_info without a series TMDB id), so thousands of calls would gain
/// nothing. The next run probes again with new titles. Unverified ids are never
/// skipped: they must be confirmed or removed.
/// Returns the skipped kinds with their counts, and how many answers had an id.
async fn run_provider(
    st: &AppState,
    items: Vec<(Kind, i64)>,
    p: &Progress,
) -> anyhow::Result<(Vec<(Kind, usize)>, usize)> {
    // The second counts from the start of the previous request, so a slow
    // answer (proxy + VPN) isn't followed by another full second of waiting.
    let mut next_start = tokio::time::Instant::now();
    let mut probe: HashMap<&'static str, (usize, usize)> = HashMap::new();
    let mut skipped: Vec<(Kind, usize)> = Vec::new();
    let mut found = 0usize;
    for (kind, id) in items {
        if p.stopped.load(Ordering::SeqCst) {
            break;
        }
        let (answers, with_id) = probe.get(kind.as_str()).copied().unwrap_or_default();
        if answers >= PROBE_LOOKUPS && with_id == 0 && !is_unverified(st, kind, id).await? {
            match skipped.iter_mut().find(|(k, _)| *k == kind) {
                Some(s) => s.1 += 1,
                None => {
                    tracing::info!(
                        "provider sent no TMDB id in {answers} {} lookups; skipping the rest of them this run",
                        kind.as_str()
                    );
                    skipped.push((kind, 1));
                }
            }
            p.processed.fetch_add(1, Ordering::SeqCst);
            continue;
        }
        tokio::time::sleep_until(next_start).await;
        next_start = tokio::time::Instant::now() + PROVIDER_DELAY;
        match provider_info(st, kind, id).await? {
            None => p.failed(1),
            Some(had_id) => {
                p.ok(1);
                let c = probe.entry(kind.as_str()).or_default();
                c.0 += 1;
                if had_id {
                    c.1 += 1;
                    found += 1;
                }
            }
        }
    }
    Ok((skipped, found))
}

async fn is_unverified(st: &AppState, kind: Kind, id: i64) -> anyhow::Result<bool> {
    let (table, id_col, _, _) = kind_parts(kind);
    let src: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT tmdb_source FROM {table} WHERE {id_col} = ?"
    )))
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    Ok(src.as_deref() == Some(SRC_UNVERIFIED))
}

/// TMDB detail requests on a few parallel workers.
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
                match fetch_details(&st, &key, g.kind, &g.tmdb_id, &g.ids).await? {
                    true => p.ok(g.ids.len()),
                    false => p.failed(g.ids.len()),
                }
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

    async fn db() -> (std::path::PathBuf, sqlx::SqlitePool) {
        let dir = std::env::temp_dir().join(format!("m3usort-enr-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        (dir, db)
    }

    #[test]
    fn tmdb_ids_are_cleaned() {
        assert_eq!(clean_tmdb_id(" 508965 "), Some("508965".into()));
        assert_eq!(clean_tmdb_id("0"), None);
        assert_eq!(clean_tmdb_id(""), None);
        assert_eq!(clean_tmdb_id("tt123"), None);
        assert_eq!(clean_tmdb_id("00155"), Some("155".into()));
    }

    #[tokio::test]
    async fn bulk_ids_respect_sources() {
        let (dir, db) = db().await;
        sqlx::query(
            "INSERT INTO movies (stream_id, name, provider_tmdb, tmdb_id, tmdb_source, poster, tmdb_checked, first_seen, last_seen) VALUES
             (1, 'a', '155', NULL, NULL, NULL, NULL, 0, 0),
             (2, 'b', '0', NULL, NULL, NULL, NULL, 0, 0),
             (3, 'c', 'tt1', NULL, NULL, NULL, NULL, 0, 0),
             (4, 'd', '27205', '42', 'provider', 'p', 1, 0, 0),
             (5, 'e', '27205', '99', 'unverified', 'p', 1, 0, 0),
             (6, 'f', '27205', '27205', 'jellyfin', 'p', 1, 0, 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(adopt_bulk_ids(&db).await.unwrap(), 3);
        let rows: Vec<(Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT tmdb_id, tmdb_source, poster FROM movies ORDER BY stream_id")
                .fetch_all(&db)
                .await
                .unwrap();
        let s = |v: &Option<String>| v.clone().unwrap_or_default();
        let got: Vec<(String, String, String)> =
            rows.iter().map(|(a, b, c)| (s(a), s(b), s(c))).collect();
        let w = |a: &str, b: &str, c: &str| (a.to_string(), b.to_string(), c.to_string());
        assert_eq!(
            got,
            vec![
                w("155", "list", ""),
                w("", "", ""),
                w("", "", ""),
                w("42", "provider", "p"), // provider id never overwritten
                w("27205", "list", ""),   // differing unverified id replaced, details cleared
                w("27205", "list", "p"),  // same id: details kept
            ]
        );
        let c = id_counts(&db).await.unwrap();
        assert_eq!(
            (
                c.movies,
                c.movies_provider,
                c.movies_jellyfin,
                c.movies_unverified
            ),
            (6, 4, 0, 0)
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// The migration's classification of ids stored by earlier versions.
    #[tokio::test]
    async fn migration_classifies_old_ids() {
        let dir = std::env::temp_dir().join(format!("m3usort-mig-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        // Build the schema up to 0008, add old-style rows, then run 0009.
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
        for f in [
            "0001_init",
            "0002_settings",
            "0003_login_failures",
            "0004_catalog",
            "0005_library",
            "0006_playlist",
            "0007_enrichment",
            "0008_bulk_tmdb",
        ] {
            let sql = std::fs::read_to_string(format!("migrations/{f}.sql")).unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO movies (stream_id, name, provider_tmdb, tmdb_id, provider_checked, tmdb_checked, poster, plot, first_seen, last_seen) VALUES
             (1, 'list',     '155', '155', NULL, 5, 'p', 'x', 0, 0),
             (2, 'provider', NULL,  '42',  1,    NULL, NULL, 'x', 0, 0),
             (3, 'unsure',   NULL,  '43',  1,    5, 'p', 'x', 0, 0),
             (4, 'guess',    NULL,  '44',  NULL, 5, 'p', 'x', 0, 0),
             (5, 'notfound', NULL,  NULL,  1,    5, NULL, 'x', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO cast_members (kind, item_id, person_id, name, ord) VALUES ('movie', 4, 1, 'A', 0), ('movie', 3, 1, 'A', 0)")
            .execute(&pool)
            .await
            .unwrap();
        let sql = std::fs::read_to_string("migrations/0009_tmdb_source.sql").unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&pool)
            .await
            .unwrap();
        type Row = (
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT tmdb_id, tmdb_source, provider_checked, tmdb_checked, poster, plot FROM movies ORDER BY stream_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let o = |v: &str| Some(v.to_string());
        assert_eq!(
            rows[0],
            (o("155"), o("list"), None, Some(5), o("p"), o("x"))
        );
        assert_eq!(
            rows[1],
            (o("42"), o("provider"), Some(1), None, None, o("x"))
        );
        assert_eq!(
            rows[2],
            (o("43"), o("unverified"), None, Some(5), o("p"), o("x"))
        );
        assert_eq!(rows[3], (None, None, None, None, None, None));
        assert_eq!(rows[4], (None, None, Some(1), None, None, o("x")));
        let cast: Vec<i64> =
            sqlx::query_scalar("SELECT item_id FROM cast_members ORDER BY item_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(cast, vec![3]);
        std::fs::remove_dir_all(dir).ok();
    }
}
