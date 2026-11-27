//! Metadata enrichment, as in the Python version:
//! - on demand when a detail page is opened (provider info, then TMDB), and
//! - two optional daily jobs (Settings → Schedules):
//!   "TMDB ID enrichment": get_vod_info / get_series_info at the provider, 1 s apart;
//!   "Cast enrichment": TMDB details (cast, poster, plot, seasons, status), 0.1 s apart.
//!
//! "checked" timestamps mean "asked, even if nothing came back", so nothing is
//! asked twice. Network errors don't set them, so those are retried later.

use std::sync::atomic::{AtomicBool, Ordering};
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
const TMDB_DELAY: Duration = Duration::from_millis(100); // as Python
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
    let base = integrations::tmdb_base();
    let tmdb_id = match tmdb_id.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => {
            let (title, year) = search_title(&name);
            match tmdb::search(&st.http, &base, key, is_movie, &title, year).await {
                Ok(Some((found, _))) => found.to_string(),
                Ok(None) => {
                    mark_tmdb_checked(st, kind, id).await?; // nothing to find; don't ask again
                    return Ok(true);
                }
                Err(e) => {
                    tracing::debug!("TMDB search '{title}': {e}");
                    return Ok(false);
                }
            }
        }
    };
    let d = match tmdb::details(&st.http, &base, key, is_movie, &tmdb_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            mark_tmdb_checked(st, kind, id).await?;
            return Ok(true);
        }
        Err(e) => {
            tracing::debug!("TMDB details {tmdb_id}: {e}");
            return Ok(false);
        }
    };
    let mut tx = st.db.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET tmdb_id = ?, poster = COALESCE(?, poster), plot = COALESCE(NULLIF(plot, ''), ?),
         rating = COALESCE(NULLIF(rating, 0), ?), imdb_id = COALESCE(imdb_id, ?), tmdb_checked = ? WHERE {id_col} = ?"
    )))
    .bind(&tmdb_id)
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
    tx.commit().await?;
    Ok(true)
}

async fn mark_tmdb_checked(st: &AppState, kind: Kind, id: i64) -> anyhow::Result<()> {
    let (table, id_col, _, _) = kind_parts(kind);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET tmdb_checked = ? WHERE {id_col} = ?"
    )))
    .bind(now_secs())
    .bind(id)
    .execute(&st.db)
    .await?;
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

/// Items still to do, library items first, then newest.
async fn todo(st: &AppState, job: &str) -> anyhow::Result<Vec<(Kind, i64)>> {
    let mut out = Vec::new();
    for kind in [Kind::Movie, Kind::Series] {
        let (table, id_col, _, _) = kind_parts(kind);
        let cond = if job == JOB_PROVIDER {
            "t.provider_checked IS NULL AND t.tmdb_id IS NULL"
        } else {
            "t.tmdb_checked IS NULL"
        };
        let ids: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t.{id_col} FROM {table} t
             WHERE {cond}
             ORDER BY EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col}) DESC, t.added DESC"
        )))
        .fetch_all(&st.db)
        .await?;
        out.extend(ids.into_iter().map(|i| (kind, i)));
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

async fn run_job_inner(st: &AppState, job: &'static str) -> anyhow::Result<String> {
    let key = tmdb_key(st).await;
    if job == JOB_TMDB && key.is_none() {
        anyhow::bail!("no TMDB API key set");
    }
    let items = todo(st, job).await?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO enrich_runs (job, started, total) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(job)
    .bind(now_secs())
    .bind(items.len() as i64)
    .fetch_one(&st.db)
    .await?;
    tracing::info!(
        "{} enrichment: {} item(s) to do",
        if job == JOB_PROVIDER {
            "TMDB ID"
        } else {
            "TMDB details"
        },
        items.len()
    );
    let (mut done, mut errors_in_a_row, mut stopped) = (0i64, 0usize, None);
    for (i, (kind, id)) in items.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(if job == JOB_PROVIDER {
                PROVIDER_DELAY
            } else {
                TMDB_DELAY
            })
            .await;
        }
        let ok = if job == JOB_PROVIDER {
            provider_info(st, *kind, *id).await?
        } else {
            tmdb_info(st, key.as_deref().unwrap_or_default(), *kind, *id).await?
        };
        if ok {
            done += 1;
            errors_in_a_row = 0;
        } else {
            errors_in_a_row += 1;
            if errors_in_a_row >= MAX_ERRORS_IN_A_ROW {
                stopped = Some(format!(
                    "stopped after {MAX_ERRORS_IN_A_ROW} failed requests in a row"
                ));
                break;
            }
        }
        if i % 10 == 0 || i + 1 == items.len() {
            sqlx::query("UPDATE enrich_runs SET processed = ? WHERE id = ?")
                .bind(i as i64 + 1)
                .bind(run_id)
                .execute(&st.db)
                .await?;
        }
    }
    let msg = match &stopped {
        Some(s) => format!(
            "{done} of {} done; {s} (rest is retried next run)",
            items.len()
        ),
        None => format!("{done} of {} done", items.len()),
    };
    sqlx::query("UPDATE enrich_runs SET finished = ?, ok = ?, message = ? WHERE id = ?")
        .bind(now_secs())
        .bind(i64::from(stopped.is_none()))
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
}
