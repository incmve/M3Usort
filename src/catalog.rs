//! VOD catalog: import from the Xtream API into SQLite, and the queries behind
//! the Movies / Series / New pages.
//!
//! One import = 4 bulk calls (categories + streams for movies and series), the same
//! calls the Python version's save_vod_cache() makes. No per-item calls here, so a
//! refresh can't hammer the provider. Enrichment fields (tmdb_id, plot, poster) are
//! never overwritten by an import.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::{AssertSqlSafe, QueryBuilder, Sqlite, SqlitePool};

use crate::integrations::{self, Xtream, as_text};
use crate::settings as s;
use crate::web::AppState;

/// Don't delete anything if the provider suddenly returns less than this share
/// of what we already have: a half-broken provider response must not wipe the catalog.
const MIN_SHARE_FOR_REMOVAL: f64 = 0.5;
const NEW_DAYS: i64 = 7;
pub const PAGE_SIZE: i64 = 60;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Movie,
    Series,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Series => "series",
        }
    }
    fn table(self) -> &'static str {
        match self {
            Kind::Movie => "movies",
            Kind::Series => "series",
        }
    }
    fn id_col(self) -> &'static str {
        match self {
            Kind::Movie => "stream_id",
            Kind::Series => "series_id",
        }
    }
    fn image_col(self) -> &'static str {
        match self {
            Kind::Movie => "icon",
            Kind::Series => "cover",
        }
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------------------ provider

/// The Xtream login from settings, or None when not configured.
pub async fn load_xtream(st: &AppState) -> anyhow::Result<Option<Xtream>> {
    let server = s::get(&st.db, s::PROVIDER_SERVER)
        .await?
        .unwrap_or_default();
    let username = s::get_secret(&st.db, &st.key, s::SECRET_PROVIDER_USERNAME)
        .await?
        .unwrap_or_default();
    let password = s::get_secret(&st.db, &st.key, s::SECRET_PROVIDER_PASSWORD)
        .await?
        .unwrap_or_default();
    if server.is_empty() || username.is_empty() || password.is_empty() {
        return Ok(None);
    }
    Ok(Some(Xtream {
        server,
        username,
        password,
    }))
}

/// GET player_api.php?action=…, expecting a JSON list of objects.
async fn fetch_list(st: &AppState, x: &Xtream, action: &str) -> anyhow::Result<Vec<Value>> {
    let url = x.api_url(action).map_err(anyhow::Error::msg)?;
    let resp = st
        .http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            integrations::provider_user_agent(),
        )
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{action}: cannot reach provider: {e}"))?;
    let status = resp.status();
    anyhow::ensure!(
        status.is_success(),
        "{action}: provider answered HTTP {status}"
    );
    let body: Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("{action}: answer is not JSON: {e}"))?;
    match body {
        Value::Array(items) => Ok(items.into_iter().filter(Value::is_object).collect()),
        // Providers answer errors with an object instead of a list.
        other => anyhow::bail!(
            "{action}: expected a list, got: {}",
            truncate(&other.to_string(), 200)
        ),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn get_i64(v: &Value, k: &str) -> Option<i64> {
    match v.get(k)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

fn get_f64(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn get_str(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .map(as_text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn is_adult(category_name: &str) -> bool {
    let n = category_name.to_lowercase();
    n.contains("xxx") || n.contains("adult") || n.contains("18+") || n.contains("porn")
}

// ------------------------------------------------------------------ import

#[derive(Default, Debug)]
pub struct ImportResult {
    pub total: i64,
    pub new: i64,
    pub removed: i64,
    pub warning: Option<String>,
}

async fn import_categories(db: &SqlitePool, kind: Kind, cats: &[Value]) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM categories WHERE kind = ?")
        .bind(kind.as_str())
        .execute(&mut *tx)
        .await?;
    for c in cats {
        let (Some(id), Some(name)) = (get_str(c, "category_id"), get_str(c, "category_name"))
        else {
            continue;
        };
        sqlx::query(
            "INSERT OR REPLACE INTO categories (kind, id, name, adult) VALUES (?, ?, ?, ?)",
        )
        .bind(kind.as_str())
        .bind(id)
        .bind(&name)
        .bind(i64::from(is_adult(&name)))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Upsert all items, then remove the ones the provider no longer lists (unless the
/// answer looks broken). One transaction: readers see the old or the new catalog.
async fn import_items(
    db: &SqlitePool,
    kind: Kind,
    items: &[Value],
    run_at: i64,
) -> anyhow::Result<ImportResult> {
    let (table, id_col) = (kind.table(), kind.id_col());
    let before: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(db)
        .await?;

    let mut tx = db.begin().await?;
    let mut seen = 0i64;
    for it in items {
        let Some(id) = get_i64(it, id_col) else {
            continue;
        };
        let Some(name) = get_str(it, "name") else {
            continue;
        };
        seen += 1;
        match kind {
            Kind::Movie => {
                sqlx::query(
                    "INSERT INTO movies (stream_id, name, category_id, icon, rating, added, container_ext, provider_tmdb, first_seen, last_seen)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (stream_id) DO UPDATE SET
                       name = excluded.name, category_id = excluded.category_id, icon = excluded.icon,
                       rating = excluded.rating, added = excluded.added, container_ext = excluded.container_ext,
                       provider_tmdb = excluded.provider_tmdb, last_seen = excluded.last_seen",
                )
                .bind(id)
                .bind(&name)
                .bind(get_str(it, "category_id"))
                .bind(get_str(it, "stream_icon"))
                .bind(get_f64(it, "rating"))
                .bind(get_i64(it, "added"))
                .bind(get_str(it, "container_extension"))
                .bind(get_str(it, "tmdb").or_else(|| get_str(it, "tmdb_id")))
                .bind(run_at)
                .bind(run_at)
                .execute(&mut *tx)
                .await?;
            }
            Kind::Series => {
                sqlx::query(
                    "INSERT INTO series (series_id, name, category_id, cover, plot, genre, release_date, rating, added, first_seen, last_seen)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (series_id) DO UPDATE SET
                       name = excluded.name, category_id = excluded.category_id, cover = excluded.cover,
                       plot = COALESCE(excluded.plot, series.plot), genre = excluded.genre,
                       release_date = excluded.release_date, rating = excluded.rating,
                       added = excluded.added, last_seen = excluded.last_seen",
                )
                .bind(id)
                .bind(&name)
                .bind(get_str(it, "category_id"))
                .bind(get_str(it, "cover"))
                .bind(get_str(it, "plot"))
                .bind(get_str(it, "genre"))
                .bind(get_str(it, "releaseDate").or_else(|| get_str(it, "release_date")))
                .bind(get_f64(it, "rating"))
                // Series have no "added"; the provider's last_modified is the closest (as in Python).
                .bind(get_i64(it, "last_modified").or_else(|| get_i64(it, "added")))
                .bind(run_at)
                .bind(run_at)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    let mut result = ImportResult {
        total: seen,
        ..Default::default()
    };
    result.new = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {table} WHERE first_seen = ?"
    )))
    .bind(run_at)
    .fetch_one(&mut *tx)
    .await?;

    let safe = before == 0 || (seen as f64) >= (before as f64) * MIN_SHARE_FOR_REMOVAL;
    if safe {
        result.removed = sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE last_seen < ?"
        )))
        .bind(run_at)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    } else {
        result.warning = Some(format!(
            "provider listed only {seen} {table} (had {before}); nothing removed, in case the answer is incomplete"
        ));
    }
    tx.commit().await?;
    Ok(result)
}

/// Full refresh. Returns a one-line summary. Only one runs at a time.
pub async fn refresh(st: &AppState) -> anyhow::Result<String> {
    if st.catalog_running.swap(true, Ordering::SeqCst) {
        anyhow::bail!("a catalog refresh is already running");
    }
    let started = now_secs();
    let run_id: i64 =
        sqlx::query_scalar("INSERT INTO catalog_runs (started) VALUES (?) RETURNING id")
            .bind(started)
            .fetch_one(&st.db)
            .await?;
    let result = refresh_inner(st, started).await;
    st.catalog_running.store(false, Ordering::SeqCst);

    let (ok, msg, m, sr) = match &result {
        Ok((m, sr)) => {
            let mut msg = format!(
                "{} movies (+{} new, -{} removed), {} series (+{} new, -{} removed)",
                m.total, m.new, m.removed, sr.total, sr.new, sr.removed
            );
            for w in [&m.warning, &sr.warning].into_iter().flatten() {
                msg.push_str("; warning: ");
                msg.push_str(w);
            }
            (true, msg, Some(m), Some(sr))
        }
        Err(e) => (false, format!("{e:#}"), None, None),
    };
    sqlx::query(
        "UPDATE catalog_runs SET finished = ?, ok = ?, movies = ?, series = ?, new_movies = ?, new_series = ?,
         removed_movies = ?, removed_series = ?, message = ? WHERE id = ?",
    )
    .bind(now_secs())
    .bind(i64::from(ok))
    .bind(m.map(|r| r.total))
    .bind(sr.map(|r| r.total))
    .bind(m.map(|r| r.new))
    .bind(sr.map(|r| r.new))
    .bind(m.map(|r| r.removed))
    .bind(sr.map(|r| r.removed))
    .bind(&msg)
    .bind(run_id)
    .execute(&st.db)
    .await?;
    // Keep the history short.
    sqlx::query("DELETE FROM catalog_runs WHERE id <= (SELECT MAX(id) - 100 FROM catalog_runs)")
        .execute(&st.db)
        .await?;

    if ok {
        tracing::info!("catalog refreshed in {}s: {msg}", now_secs() - started);
        Ok(msg)
    } else {
        tracing::error!("catalog refresh failed: {msg}");
        anyhow::bail!(msg)
    }
}

async fn refresh_inner(st: &AppState, run_at: i64) -> anyhow::Result<(ImportResult, ImportResult)> {
    let x = load_xtream(st)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider is not configured"))?;
    // Categories are nice-to-have (as in Python): a failure there doesn't stop the import.
    match fetch_list(st, &x, "get_vod_categories").await {
        Ok(c) => import_categories(&st.db, Kind::Movie, &c).await?,
        Err(e) => tracing::warn!("movie categories: {e:#}"),
    }
    let movies = fetch_list(st, &x, "get_vod_streams").await?;
    anyhow::ensure!(
        !movies.is_empty(),
        "get_vod_streams returned an empty list; catalog left unchanged"
    );
    let m = import_items(&st.db, Kind::Movie, &movies, run_at).await?;

    match fetch_list(st, &x, "get_series_categories").await {
        Ok(c) => import_categories(&st.db, Kind::Series, &c).await?,
        Err(e) => tracing::warn!("series categories: {e:#}"),
    }
    let series = fetch_list(st, &x, "get_series").await?;
    anyhow::ensure!(
        !series.is_empty(),
        "get_series returned an empty list; series left unchanged"
    );
    let sr = import_items(&st.db, Kind::Series, &series, run_at).await?;
    Ok((m, sr))
}

/// Background loop: refresh when the last successful import is older than the VOD
/// scan interval. After a failure, wait 15 minutes before trying again.
pub fn spawn_scheduler(st: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            if let Err(e) = maybe_refresh(&st).await {
                tracing::debug!("catalog scheduler: {e:#}");
            }
            if let Err(e) = crate::library::maybe_run_vod(&st).await {
                tracing::debug!("VOD scheduler: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

async fn maybe_refresh(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done() || st.catalog_running.load(Ordering::SeqCst) {
        return Ok(());
    }
    let hours: i64 = s::get(&st.db, s::VOD_SCAN_HOURS)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let last_ok: Option<i64> =
        sqlx::query_scalar("SELECT MAX(started) FROM catalog_runs WHERE ok = 1")
            .fetch_one(&st.db)
            .await?;
    let last_any: Option<i64> = sqlx::query_scalar("SELECT MAX(started) FROM catalog_runs")
        .fetch_one(&st.db)
        .await?;
    let now = now_secs();
    let due = last_ok.is_none_or(|t| now - t >= hours * 3600);
    let backoff = last_any.is_some_and(|t| now - t < 15 * 60) && last_any != last_ok;
    if due && !backoff {
        let _ = refresh(st).await;
    }
    Ok(())
}

// ------------------------------------------------------------------ queries

/// (id, name, image, rating, added, category) as selected by list/new_items.
type ItemRow = (i64, String, String, Option<f64>, Option<i64>, String, bool);

pub struct Item {
    pub id: i64,
    pub name: String,
    pub image: String,
    pub rating: String,
    pub added: String,
    pub category: String,
    pub in_library: bool,
}

#[derive(Default)]
pub struct ListQuery {
    pub q: String,
    pub category: String,
    pub sort: String,
    pub page: i64,
    pub nsfw: bool,
}

/// Turn user input into a safe FTS5 query: every word as a quoted prefix term, AND-ed.
/// Quotes and FTS operators typed by the user can't break the query.
pub fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn push_filters(qb: &mut QueryBuilder<Sqlite>, kind: Kind, lq: &ListQuery) {
    let (table, id_col) = (kind.table(), kind.id_col());
    qb.push(" WHERE 1 = 1");
    if let Some(fts) = fts_query(&lq.q) {
        qb.push(format!(
            " AND t.{id_col} IN (SELECT rowid FROM {table}_fts WHERE {table}_fts MATCH "
        ));
        qb.push_bind(fts);
        qb.push(")");
    }
    if !lq.category.is_empty() {
        qb.push(" AND t.category_id = ")
            .push_bind(lq.category.clone());
    }
    if !lq.nsfw {
        qb.push(" AND (c.adult IS NULL OR c.adult = 0)");
    }
}

pub async fn list(db: &SqlitePool, kind: Kind, lq: &ListQuery) -> anyhow::Result<(Vec<Item>, i64)> {
    let (table, id_col, img) = (kind.table(), kind.id_col(), kind.image_col());
    let from = format!(
        " FROM {table} t LEFT JOIN categories c ON c.kind = '{}' AND c.id = t.category_id",
        kind.as_str()
    );

    let mut count = QueryBuilder::<Sqlite>::new(format!("SELECT COUNT(*){from}"));
    push_filters(&mut count, kind, lq);
    let total: i64 = count.build_query_scalar().fetch_one(db).await?;

    let mut qb = QueryBuilder::<Sqlite>::new(format!(
        "SELECT t.{id_col}, t.name, COALESCE(t.poster, t.{img}, ''), t.rating, t.added, COALESCE(c.name, ''),
         EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col}){from}"
    ));
    push_filters(&mut qb, kind, lq);
    qb.push(match lq.sort.as_str() {
        "za" => " ORDER BY t.name COLLATE NOCASE DESC",
        "new" => " ORDER BY t.added DESC NULLS LAST, t.name COLLATE NOCASE",
        "old" => " ORDER BY t.added ASC NULLS LAST, t.name COLLATE NOCASE",
        _ => " ORDER BY t.name COLLATE NOCASE",
    });
    qb.push(" LIMIT ")
        .push_bind(PAGE_SIZE)
        .push(" OFFSET ")
        .push_bind(lq.page.max(0) * PAGE_SIZE);
    let rows: Vec<ItemRow> = qb.build_query_as().fetch_all(db).await?;
    Ok((rows.into_iter().map(to_item).collect(), total))
}

fn to_item((id, name, image, rating, added, category, in_library): ItemRow) -> Item {
    Item {
        id,
        name,
        image,
        rating: rating
            .filter(|r| *r > 0.0)
            .map(|r| format!("{r:.1}"))
            .unwrap_or_default(),
        added: added
            .filter(|a| *a > 0)
            .map(integrations::unix_to_date)
            .unwrap_or_default(),
        category,
        in_library,
    }
}

/// Items added in the last 7 days, newest first (adult categories hidden).
pub async fn new_items(db: &SqlitePool, kind: Kind) -> anyhow::Result<Vec<Item>> {
    let (table, id_col, img) = (kind.table(), kind.id_col(), kind.image_col());
    let since = now_secs() - NEW_DAYS * 86_400;
    let rows: Vec<ItemRow> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT t.{id_col}, t.name, COALESCE(t.poster, t.{img}, ''), t.rating, t.added, COALESCE(c.name, ''),
                EXISTS (SELECT 1 FROM library_{table} l WHERE l.{id_col} = t.{id_col})
         FROM {table} t LEFT JOIN categories c ON c.kind = ? AND c.id = t.category_id
         WHERE t.added >= ? AND (c.adult IS NULL OR c.adult = 0)
         ORDER BY t.added DESC LIMIT 300"
    )))
    .bind(kind.as_str())
    .bind(since)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(to_item).collect())
}

pub async fn categories(
    db: &SqlitePool,
    kind: Kind,
    nsfw: bool,
) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, name FROM categories WHERE kind = ? AND (? OR adult = 0) ORDER BY name COLLATE NOCASE",
    )
    .bind(kind.as_str())
    .bind(nsfw)
    .fetch_all(db)
    .await?)
}

pub struct Stats {
    pub movies: i64,
    pub series: i64,
    pub new_movies: i64,
    pub new_series: i64,
    pub last_ok: String,
    pub last_message: String,
    pub last_failed: bool,
}

pub async fn stats(db: &SqlitePool) -> anyhow::Result<Stats> {
    let since = now_secs() - NEW_DAYS * 86_400;
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(since)
            .fetch_one(db)
            .await
    };
    let last: Option<(i64, i64, Option<String>)> =
        sqlx::query_as("SELECT started, ok, message FROM catalog_runs WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1")
            .fetch_optional(db)
            .await?;
    let last_ok: Option<i64> =
        sqlx::query_scalar("SELECT MAX(finished) FROM catalog_runs WHERE ok = 1")
            .fetch_one(db)
            .await?;
    // Same filter as the pages: adult categories don't count.
    Ok(Stats {
        movies: count(
            "SELECT COUNT(*) FROM movies t LEFT JOIN categories c ON c.kind = 'movie' AND c.id = t.category_id
             WHERE (c.adult IS NULL OR c.adult = 0) AND ? IS NOT NULL",
        )
        .await?,
        series: count(
            "SELECT COUNT(*) FROM series t LEFT JOIN categories c ON c.kind = 'series' AND c.id = t.category_id
             WHERE (c.adult IS NULL OR c.adult = 0) AND ? IS NOT NULL",
        )
        .await?,
        new_movies: count(
            "SELECT COUNT(*) FROM movies t LEFT JOIN categories c ON c.kind = 'movie' AND c.id = t.category_id
             WHERE (c.adult IS NULL OR c.adult = 0) AND t.added >= ?",
        )
        .await?,
        new_series: count(
            "SELECT COUNT(*) FROM series t LEFT JOIN categories c ON c.kind = 'series' AND c.id = t.category_id
             WHERE (c.adult IS NULL OR c.adult = 0) AND t.added >= ?",
        )
        .await?,
        last_ok: last_ok.map(ago).unwrap_or_else(|| "never".into()),
        last_message: last.as_ref().and_then(|l| l.2.clone()).unwrap_or_default(),
        last_failed: last.is_some_and(|l| l.1 == 0),
    })
}

/// "5 minutes ago" style, for the dashboard.
pub fn ago(ts: i64) -> String {
    let d = (now_secs() - ts).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn db() -> (std::path::PathBuf, SqlitePool) {
        let dir = std::env::temp_dir().join(format!("m3usort-cat-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        (dir, pool)
    }

    fn movie(id: i64, name: &str, cat: &str, added: i64) -> Value {
        // Mixed types on purpose: providers send numbers as strings.
        json!({"stream_id": id, "name": name, "category_id": cat, "stream_icon": "http://i/x.jpg",
               "rating": "7.5", "added": added.to_string(), "container_extension": "mkv"})
    }

    #[test]
    fn fts_query_is_safe() {
        assert_eq!(fts_query("the bat"), Some("\"the\"* \"bat\"*".into()));
        assert_eq!(
            fts_query("\"OR\" NEAR( -x"),
            Some("\"OR\"* \"NEAR\"* \"x\"*".into())
        );
        assert_eq!(fts_query("  !!  "), None);
        assert!(is_adult("XXX FILMS") && is_adult("Adult 18+") && !is_adult("NL Films"));
    }

    #[tokio::test]
    async fn import_search_and_safe_removal() {
        let (dir, db) = db().await;
        sqlx::query("INSERT INTO categories (kind, id, name, adult) VALUES ('movie','1','NL Films',0),('movie','145','XXX FILMS',1)")
            .execute(&db)
            .await
            .unwrap();
        let now = now_secs();
        let mut items: Vec<Value> = (1..=100)
            .map(|i| movie(i, &format!("Film {i}"), "1", now - 30 * 86_400))
            .collect();
        items.push(movie(500, "Amélie (2001)", "1", now - 86_400));
        items.push(movie(501, "Secret Adult", "145", now));
        let r = import_items(&db, Kind::Movie, &items, 1000).await.unwrap();
        assert_eq!((r.total, r.new, r.removed), (102, 102, 0));

        // Search: diacritics-insensitive prefix match; adult hidden unless asked.
        let q = |q: &str, nsfw: bool| ListQuery {
            q: q.into(),
            nsfw,
            ..Default::default()
        };
        assert_eq!(
            list(&db, Kind::Movie, &q("amel", false)).await.unwrap().1,
            1
        );
        assert_eq!(
            list(&db, Kind::Movie, &q("secret", false)).await.unwrap().1,
            0
        );
        assert_eq!(
            list(&db, Kind::Movie, &q("secret", true)).await.unwrap().1,
            1
        );
        assert_eq!(list(&db, Kind::Movie, &q("", false)).await.unwrap().1, 101);
        assert_eq!(
            new_items(&db, Kind::Movie).await.unwrap().len(),
            1,
            "only Amélie: adult hidden, rest is old"
        );

        // Enrichment survives an import.
        sqlx::query("UPDATE movies SET tmdb_id = '194' WHERE stream_id = 500")
            .execute(&db)
            .await
            .unwrap();

        // Provider suddenly lists only 10: nothing removed (looks broken).
        let r = import_items(&db, Kind::Movie, &items[..10], 2000)
            .await
            .unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.warning.is_some());

        // Normal shrink (drops 2 of 102): removed, search index follows.
        let r = import_items(&db, Kind::Movie, &items[..100], 3000)
            .await
            .unwrap();
        assert_eq!((r.removed, r.new), (2, 0));
        assert_eq!(
            list(&db, Kind::Movie, &q("amel", false)).await.unwrap().1,
            0
        );

        // Renamed item is found under its new name only.
        let mut renamed = items[..100].to_vec();
        renamed[0] = movie(1, "Totally New Title", "1", now);
        import_items(&db, Kind::Movie, &renamed, 4000)
            .await
            .unwrap();
        assert_eq!(
            list(&db, Kind::Movie, &q("totally", false))
                .await
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            list(&db, Kind::Movie, &q("film 1", false))
                .await
                .unwrap()
                .0
                .iter()
                .filter(|i| i.id == 1)
                .count(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
