//! HTTP layer: routing, guards, guest/admin pages, and the "blocked" pages shown
//! when the key is missing or wrong.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::{self, ADMIN_COOKIE, LoginLimiter, SESSION_TTL, SessionSigner};
use crate::catalog;
use crate::crypto::MasterKey;
use crate::library;
use crate::settings as s;
use crate::setup::{self, Draft};
use crate::version::{self, SharedStatus, UpdateStatus};

// ------------------------------------------------------------------ state

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub key: Arc<MasterKey>,
    pub update: SharedStatus,
    pub http: reqwest::Client,
    pub signer: Arc<SessionSigner>,
    pub limiter: Arc<LoginLimiter>,
    pub setup_code: Arc<String>,
    pub draft: Arc<Mutex<Option<(String, Draft)>>>,
    pub catalog_running: Arc<AtomicBool>,
    pub vod_running: Arc<AtomicBool>,
    setup_done: Arc<AtomicBool>,
}

impl AppState {
    pub async fn new(db: SqlitePool, key: MasterKey, update: SharedStatus) -> anyhow::Result<Self> {
        let setup_done = s::setup_completed(&db).await?;
        let setup_code = auth::random_code(8);
        let limiter = Arc::new(LoginLimiter::new(db.clone()));
        if !setup_done {
            tracing::warn!("Setup not finished. Setup code: {setup_code}");
        }
        Ok(Self {
            signer: Arc::new(SessionSigner::new(*key.session_key())),
            key: Arc::new(key),
            db,
            update,
            http: crate::integrations::http_client()?,
            limiter,
            setup_code: Arc::new(setup_code),
            draft: Arc::new(Mutex::new(None)),
            catalog_running: Arc::new(AtomicBool::new(false)),
            vod_running: Arc::new(AtomicBool::new(false)),
            setup_done: Arc::new(AtomicBool::new(setup_done)),
        })
    }

    pub fn is_setup_done(&self) -> bool {
        self.setup_done.load(Ordering::SeqCst)
    }

    pub fn mark_setup_done(&self) {
        self.setup_done.store(true, Ordering::SeqCst);
    }

    pub async fn is_admin(&self, headers: &HeaderMap) -> bool {
        let Some(cookie) = auth::read_cookie(headers, ADMIN_COOKIE) else {
            return false;
        };
        let generation = s::session_generation(&self.db).await.unwrap_or(-1);
        self.signer.verify(cookie, generation)
    }
}

/// Data every page's layout needs.
pub struct Page {
    pub title: String,
    pub version: String,
    pub is_admin: bool,
    pub show_nav: bool,
}

impl Page {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.into(),
            version: version::version_line(),
            is_admin: false,
            show_nav: false,
        }
    }
    pub fn admin(title: &str) -> Self {
        Self::nav(title, true)
    }
    fn nav(title: &str, is_admin: bool) -> Self {
        Self {
            is_admin,
            show_nav: true,
            ..Self::new(title)
        }
    }
}

fn render(t: impl Template) -> Response {
    match t.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e}"),
        )
            .into_response(),
    }
}

// ------------------------------------------------------------------ routers

pub fn app_router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/", get(admin_page))
        .route("/password", post(change_admin_password))
        .route("/playlist-password", post(change_playlist_password))
        .route("/credentials", post(show_credentials))
        .route(
            "/settings/{step}",
            get(setup::admin_section).post(setup::admin_section_save),
        )
        .route("/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    Router::new()
        .route("/healthcheck", get(|| async { "ok" }))
        .route("/favicon.png", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/api/version", get(api_version))
        .route("/", get(home))
        .route("/movies", get(movies_page))
        .route("/series", get(series_page))
        .route("/new", get(new_page))
        .route("/catalog/refresh", post(catalog_refresh))
        .route("/library", get(library_page))
        .route("/library/movie/{id}", post(add_movie))
        .route("/library/series/{id}", post(add_series))
        .route("/wanted", post(wanted_add))
        .route("/wanted/{id}/delete", post(wanted_delete))
        .route("/vod/run", post(vod_run))
        .route("/setup", get(setup::start))
        .route("/setup/summary", get(setup::summary))
        .route("/setup/finish", post(setup::finish))
        .route(
            "/setup/{step}",
            get(setup::show_step).post(setup::submit_step),
        )
        .route("/login", get(login_page).post(login))
        .nest("/admin", admin)
        .layer(middleware::from_fn_with_state(state.clone(), require_setup))
        .with_state(state)
}

/// Until setup is finished, everything except the wizard redirects to it.
async fn require_setup(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let p = req.uri().path();
    let open =
        p == "/healthcheck" || p == "/api/version" || p == "/setup" || p.starts_with("/setup/");
    if !st.is_setup_done() && !open {
        return Redirect::to("/setup").into_response();
    }
    next.run(req).await
}

/// Admin pages need a valid admin session. Guests need nothing (trusted).
async fn require_admin(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if st.is_admin(req.headers()).await {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

// ------------------------------------------------------------------ guest pages

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTpl {
    page: Page,
    update_banner: Option<(String, String)>,
    stats: catalog::Stats,
    running: bool,
    message: String,
}

#[derive(Deserialize)]
struct HomeQuery {
    refreshed: Option<String>,
}

async fn home(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HomeQuery>,
) -> Response {
    let u = st.update.read().await.clone();
    let update_banner = match (u.latest, u.release_url) {
        (Some(latest), Some(url)) if u.update_available => Some((latest, url)),
        _ => None,
    };
    let stats = match catalog::stats(&st.db).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            )
                .into_response();
        }
    };
    let running = st.catalog_running.load(Ordering::SeqCst);
    let message = if q.refreshed.is_some() && running {
        "Catalog refresh started. This page shows the result when you reload it.".into()
    } else {
        String::new()
    };
    render(HomeTpl {
        page: Page::nav("M3Usort", st.is_admin(&headers).await),
        update_banner,
        stats,
        running,
        message,
    })
}

/// Start a catalog refresh in the background (guests may do this, as in Python).
async fn catalog_refresh(State(st): State<AppState>) -> Response {
    if !st.catalog_running.load(Ordering::SeqCst) {
        let st2 = st.clone();
        tokio::spawn(async move {
            let _ = catalog::refresh(&st2).await;
        });
        // Give the task a moment to set the running flag before the redirect renders.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Redirect::to("/?refreshed=1").into_response()
}

// ------------------------------------------------------------------ library (guests may use all of this, as in Python)

#[derive(Serialize)]
struct AddResult {
    ok: bool,
    message: String,
}

/// Answer JSON to the page's fetch() call, or redirect back for plain form posts.
fn add_response(headers: &HeaderMap, r: anyhow::Result<String>) -> Response {
    let (ok, message) = match r {
        Ok(m) => (true, m),
        Err(e) => (false, format!("{e:#}")),
    };
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if wants_json {
        return Json(AddResult { ok, message }).into_response();
    }
    let back = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| reqwest::Url::parse(r).ok())
        .map(|u| u.path().to_string())
        .filter(|p| p.starts_with('/') && !p.starts_with("//"))
        .unwrap_or_else(|| "/library".into());
    Redirect::to(&back).into_response()
}

async fn add_movie(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let r = library::add_movie(&st, id, "add").await;
    if r.is_ok() {
        let mut log = library::RunLog::default();
        library::refresh_jellyfin(&st, &mut log).await;
    }
    add_response(&headers, r)
}

async fn add_series(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let r = library::add_series(&st, id, "add").await;
    if matches!(r, Ok((_, n)) if n > 0) {
        let mut log = library::RunLog::default();
        library::refresh_jellyfin(&st, &mut log).await;
    }
    add_response(&headers, r.map(|(m, _)| m))
}

#[derive(Template)]
#[template(path = "library.html")]
struct LibraryTpl {
    page: Page,
    message: String,
    pending: Vec<WantedRow>,
    found: Vec<WantedRow>,
    movies: i64,
    series: Vec<SeriesRow>,
    last_run: String,
    last_run_failed: bool,
    running: bool,
    scheduler_on: bool,
    folders_missing: Vec<&'static str>,
}

/// (id, kind, term, created_at, last_check, found_at, found_names)
type WantedDbRow = (
    i64,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

pub struct WantedRow {
    pub id: i64,
    pub kind: String,
    pub term: String,
    pub created: String,
    pub last_check: String,
    pub note: String,
}

pub struct SeriesRow {
    pub folder: String,
    pub episodes: i64,
    pub last_sync: String,
    pub error: String,
}

#[derive(Deserialize)]
struct MsgQuery {
    msg: Option<String>,
}

async fn library_page(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MsgQuery>,
) -> Response {
    let r = async {
        let wanted: Vec<WantedDbRow> = sqlx::query_as(
            "SELECT id, kind, term, created_at, last_check, found_at, found_names FROM wanted ORDER BY id DESC LIMIT 300",
        )
        .fetch_all(&st.db)
        .await?;
        let date = |t: i64| crate::integrations::unix_to_date(t);
        let (mut pending, mut found) = (Vec::new(), Vec::new());
        for (id, kind, term, created, check, found_at, names) in wanted {
            let row = WantedRow {
                id,
                kind,
                term,
                created: date(created),
                last_check: check.map(catalog::ago).unwrap_or_else(|| "not yet".into()),
                note: names.unwrap_or_default(),
            };
            if let Some(f) = found_at {
                found.push(WantedRow { last_check: date(f), ..row });
            } else {
                pending.push(row);
            }
        }
        let movies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM library_movies").fetch_one(&st.db).await?;
        let series: Vec<(String, i64, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT folder, episodes, last_sync, last_error FROM library_series ORDER BY last_error IS NULL, folder COLLATE NOCASE",
        )
        .fetch_all(&st.db)
        .await?;
        let series = series
            .into_iter()
            .map(|(folder, episodes, sync, err)| SeriesRow {
                folder,
                episodes,
                last_sync: sync.map(catalog::ago).unwrap_or_else(|| "pending".into()),
                error: err.unwrap_or_default(),
            })
            .collect();
        let last: Option<(i64, i64, Option<String>)> =
            sqlx::query_as("SELECT started, ok, message FROM vod_runs WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1")
                .fetch_optional(&st.db)
                .await?;
        let mut folders_missing = Vec::new();
        if s::get(&st.db, s::MOVIES_DIR).await?.unwrap_or_default().is_empty() {
            folders_missing.push("movies");
        }
        if s::get(&st.db, s::SERIES_DIR).await?.unwrap_or_default().is_empty() {
            folders_missing.push("series");
        }
        let scheduler_on = s::get(&st.db, s::VOD_SCHEDULER_ENABLED).await?.as_deref() == Some("1");
        anyhow::Ok((pending, found, movies, series, last, folders_missing, scheduler_on))
    }
    .await;
    let (pending, found, movies, series, last, folders_missing, scheduler_on) = match r {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            )
                .into_response();
        }
    };
    render(LibraryTpl {
        page: Page::nav("Library", st.is_admin(&headers).await),
        message: q.msg.unwrap_or_default(),
        pending,
        found,
        movies,
        series,
        last_run: last
            .as_ref()
            .map(|(t, _, m)| {
                format!(
                    "{} — {}",
                    catalog::ago(*t),
                    short(&m.clone().unwrap_or_default(), 300)
                )
            })
            .unwrap_or_else(|| "never".into()),
        last_run_failed: last.is_some_and(|l| l.1 == 0),
        running: st.vod_running.load(Ordering::SeqCst),
        scheduler_on,
        folders_missing,
    })
}

#[derive(Deserialize)]
struct WantedForm {
    kind: String,
    term: String,
}

async fn wanted_add(State(st): State<AppState>, Form(f): Form<WantedForm>) -> Response {
    let term: String = f.term.trim().chars().take(200).collect();
    let kind = if f.kind == "series" {
        "series"
    } else {
        "movie"
    };
    let msg = if term.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
        "Type at least 2 letters or digits.".to_string()
    } else {
        match sqlx::query(
            "INSERT INTO wanted (kind, term, created_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(kind)
        .bind(&term)
        .bind(catalog::now_secs())
        .execute(&st.db)
        .await
        {
            Ok(r) if r.rows_affected() == 0 => format!("'{term}' is already on the wanted list."),
            Ok(_) => format!(
                "'{term}' added. It is searched on the next VOD run (or click Start download now)."
            ),
            Err(e) => format!("Saving failed: {e}"),
        }
    };
    redirect_with_msg("/library", &msg)
}

async fn wanted_delete(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let _ = sqlx::query("DELETE FROM wanted WHERE id = ?")
        .bind(id)
        .execute(&st.db)
        .await;
    redirect_with_msg("/library", "Removed from the wanted list.")
}

/// "Start download now" (guests may, as in Python). Refused while a run is busy
/// or within 5 minutes of the last one (Python had a 300 s cooldown in the browser).
async fn vod_run(State(st): State<AppState>) -> Response {
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(started) FROM vod_runs")
        .fetch_one(&st.db)
        .await
        .unwrap_or(None);
    let wait = last
        .map(|t| 300 - (catalog::now_secs() - t))
        .filter(|w| *w > 0);
    if st.vod_running.load(Ordering::SeqCst) {
        return redirect_with_msg("/library", "A VOD run is already busy.");
    }
    if let Some(w) = wait {
        return redirect_with_msg(
            "/library",
            &format!(
                "Last run was less than 5 minutes ago. Try again in {} s.",
                w
            ),
        );
    }
    let st2 = st.clone();
    tokio::spawn(async move {
        let _ = library::run_vod_job(&st2).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    redirect_with_msg(
        "/library",
        "VOD run started. Reload this page to see the result.",
    )
}

/// Cap a stored run message for display (details are in the container log).
fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!(
            "{}… (full details in the container log)",
            s.chars().take(max).collect::<String>()
        )
    }
}

fn redirect_with_msg(path: &str, msg: &str) -> Response {
    let q: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("msg", msg)
        .finish();
    Redirect::to(&format!("{path}?{q}")).into_response()
}

// ------------------------------------------------------------------ catalog pages

#[derive(Template)]
#[template(path = "catalog.html")]
struct CatalogTpl {
    page: Page,
    kind: &'static str,
    path: &'static str,
    items: Vec<catalog::Item>,
    total: i64,
    q: String,
    category: String,
    sort: String,
    nsfw: bool,
    page_no: i64,
    pages: i64,
    categories: Vec<(String, String)>,
    empty_catalog: bool,
}

impl CatalogTpl {
    /// Link to another page with the same filters.
    fn page_link(&self, p: i64) -> String {
        let mut u = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in [
            ("q", self.q.as_str()),
            ("category", self.category.as_str()),
            ("sort", self.sort.as_str()),
        ] {
            if !v.is_empty() {
                u.append_pair(k, v);
            }
        }
        if self.nsfw {
            u.append_pair("nsfw", "1");
        }
        u.append_pair("page", &p.to_string());
        format!("{}?{}", self.path, u.finish())
    }
}

#[derive(Deserialize, Default)]
struct ListParams {
    q: Option<String>,
    category: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
    nsfw: Option<String>,
}

async fn catalog_page(
    st: AppState,
    headers: HeaderMap,
    kind: catalog::Kind,
    p: ListParams,
) -> Response {
    let lq = catalog::ListQuery {
        q: p.q.unwrap_or_default().trim().chars().take(100).collect(),
        category: p.category.unwrap_or_default(),
        sort: p.sort.unwrap_or_default(),
        page: (p.page.unwrap_or(1) - 1).max(0),
        nsfw: p.nsfw.is_some_and(|v| v == "1" || v == "on"),
    };
    let result = async {
        let (items, total) = catalog::list(&st.db, kind, &lq).await?;
        let categories = catalog::categories(&st.db, kind, lq.nsfw).await?;
        let any: i64 = sqlx::query_scalar(match kind {
            catalog::Kind::Movie => "SELECT COUNT(*) FROM movies",
            catalog::Kind::Series => "SELECT COUNT(*) FROM series",
        })
        .fetch_one(&st.db)
        .await?;
        anyhow::Ok((items, total, categories, any == 0))
    }
    .await;
    let (items, total, categories, empty_catalog) = match result {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e}"),
            )
                .into_response();
        }
    };
    let (title, path) = match kind {
        catalog::Kind::Movie => ("Movies", "/movies"),
        catalog::Kind::Series => ("Series", "/series"),
    };
    render(CatalogTpl {
        page: Page::nav(title, st.is_admin(&headers).await),
        kind: title,
        path,
        items,
        total,
        pages: ((total + catalog::PAGE_SIZE - 1) / catalog::PAGE_SIZE).max(1),
        page_no: lq.page + 1,
        q: lq.q,
        category: lq.category,
        sort: lq.sort,
        nsfw: lq.nsfw,
        categories,
        empty_catalog,
    })
}

async fn movies_page(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> Response {
    catalog_page(st, headers, catalog::Kind::Movie, p).await
}

async fn series_page(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<ListParams>,
) -> Response {
    catalog_page(st, headers, catalog::Kind::Series, p).await
}

#[derive(Template)]
#[template(path = "new.html")]
struct NewTpl {
    page: Page,
    movies: Vec<catalog::Item>,
    series: Vec<catalog::Item>,
}

async fn new_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let r = async {
        anyhow::Ok((
            catalog::new_items(&st.db, catalog::Kind::Movie).await?,
            catalog::new_items(&st.db, catalog::Kind::Series).await?,
        ))
    }
    .await;
    match r {
        Ok((movies, series)) => render(NewTpl {
            page: Page::nav("New this week", st.is_admin(&headers).await),
            movies,
            series,
        }),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("database error: {e}"),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct VersionInfo {
    version: &'static str,
    commit: &'static str,
    #[serde(flatten)]
    update: UpdateStatus,
}

async fn api_version(State(st): State<AppState>) -> Json<VersionInfo> {
    Json(VersionInfo {
        version: version::VERSION,
        commit: version::COMMIT,
        update: st.update.read().await.clone(),
    })
}

// ------------------------------------------------------------------ login

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTpl {
    page: Page,
    error: String,
}

async fn login_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if st.is_admin(&headers).await {
        return Redirect::to("/admin").into_response();
    }
    render(LoginTpl {
        page: Page::nav("Admin login", false),
        error: String::new(),
    })
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

/// Checks the admin password with the per-IP limiter. `Err` carries the message to show.
async fn check_admin_password(
    st: &AppState,
    addr: SocketAddr,
    password: String,
) -> Result<(), String> {
    let ip = addr.ip();
    if let Err(min) = st.limiter.check(ip).await {
        return Err(format!(
            "Too many wrong passwords. Try again in {min} minutes."
        ));
    }
    let hash = s::get(&st.db, s::ADMIN_PASSWORD_HASH)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    if ok {
        st.limiter.reset(ip).await;
        Ok(())
    } else {
        st.limiter.fail(ip).await;
        tracing::warn!("wrong admin password from {ip}");
        Err("Wrong password.".into())
    }
}

async fn login(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    match check_admin_password(&st, addr, form.password).await {
        Err(error) => render(LoginTpl {
            page: Page::nav("Admin login", false),
            error,
        }),
        Ok(()) => {
            tracing::info!("admin logged in from {}", addr.ip());
            let generation = s::session_generation(&st.db).await.unwrap_or(0);
            with_admin_cookie(
                Redirect::to("/admin").into_response(),
                &st.signer.issue(generation),
            )
        }
    }
}

fn with_admin_cookie(mut resp: Response, cookie: &str) -> Response {
    resp.headers_mut().append(
        header::SET_COOKIE,
        auth::set_cookie_header(ADMIN_COOKIE, cookie, SESSION_TTL)
            .parse()
            .expect("valid header"),
    );
    resp
}

async fn logout() -> Response {
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        auth::clear_cookie_header(ADMIN_COOKIE)
            .parse()
            .expect("valid header"),
    );
    resp
}

// ------------------------------------------------------------------ admin

/// (wizard step, title, rows) for one settings card on the admin page.
type Section = (u8, &'static str, Vec<(String, String)>);

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminTpl {
    page: Page,
    message: String,
    error: String,
    sections: Vec<Section>,
    credentials: Vec<(String, String)>,
    playlist_password_set: bool,
}

async fn admin_render(
    st: &AppState,
    message: String,
    error: String,
    credentials: Vec<(String, String)>,
) -> Response {
    let all: std::collections::HashMap<String, String> = s::get_all(&st.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let has_secret = |name: &str| {
        let db = st.db.clone();
        let name = name.to_string();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM secrets WHERE name = ?")
                .bind(name)
                .fetch_one(&db)
                .await
                .unwrap_or(0)
                > 0
        }
    };
    let v = |k: &str| {
        let x = all.get(k).cloned().unwrap_or_default();
        if x.is_empty() {
            "Not set".to_string()
        } else {
            x
        }
    };
    let yn = |k: &str| {
        if all.get(k).map(String::as_str) == Some("1") {
            "Yes"
        } else {
            "No"
        }
        .to_string()
    };
    let set_or_not = |b: bool| if b { "Set (encrypted)" } else { "Not set" }.to_string();
    let sections = vec![
        (
            2,
            setup::step_info(2).title,
            vec![
                ("Server".to_string(), v(s::PROVIDER_SERVER)),
                (
                    "Username".into(),
                    set_or_not(has_secret(s::SECRET_PROVIDER_USERNAME).await),
                ),
                (
                    "Password".into(),
                    set_or_not(has_secret(s::SECRET_PROVIDER_PASSWORD).await),
                ),
            ],
        ),
        (
            3,
            setup::step_info(3).title,
            vec![
                ("Movies".into(), v(s::MOVIES_DIR)),
                ("Series".into(), v(s::SERIES_DIR)),
            ],
        ),
        (
            4,
            setup::step_info(4).title,
            vec![
                ("Enabled".into(), yn(s::JELLYFIN_ENABLED)),
                ("URL".into(), v(s::JELLYFIN_URL)),
                (
                    "API key".into(),
                    set_or_not(has_secret(s::SECRET_JELLYFIN_API_KEY).await),
                ),
            ],
        ),
        (
            5,
            setup::step_info(5).title,
            vec![(
                "API key".into(),
                set_or_not(has_secret(s::SECRET_TMDB_API_KEY).await),
            )],
        ),
        (
            6,
            setup::step_info(6).title,
            vec![
                (
                    "Playlist".into(),
                    format!(
                        "every {} h → {}",
                        v(s::PLAYLIST_REFRESH_HOURS),
                        v(s::PLAYLIST_OUTPUT)
                    ),
                ),
                ("Custom group".into(), v(s::CUSTOM_GROUP_TITLE)),
                (
                    "VOD scheduler".into(),
                    format!(
                        "{} (every {} h)",
                        yn(s::VOD_SCHEDULER_ENABLED),
                        v(s::VOD_SCAN_HOURS)
                    ),
                ),
                (
                    "Overwrite movies / episodes".into(),
                    format!("{} / {}", yn(s::OVERWRITE_MOVIES), yn(s::OVERWRITE_SERIES)),
                ),
                ("Matching".into(), v(s::MATCH_TYPE)),
                (
                    "TMDB enrichment".into(),
                    format!(
                        "{} ({})",
                        yn(s::TMDB_ENRICH_ENABLED),
                        v(s::TMDB_ENRICH_TIME)
                    ),
                ),
                (
                    "Cast enrichment".into(),
                    format!(
                        "{} ({})",
                        yn(s::CAST_ENRICH_ENABLED),
                        v(s::CAST_ENRICH_TIME)
                    ),
                ),
            ],
        ),
    ];
    let playlist_password_set = all
        .get(s::PLAYLIST_PASSWORD_HASH)
        .is_some_and(|h| !h.is_empty());
    render(AdminTpl {
        page: Page::admin("Admin"),
        message,
        error,
        sections,
        credentials,
        playlist_password_set,
    })
}

#[derive(Deserialize)]
struct AdminQuery {
    welcome: Option<String>,
}

async fn admin_page(State(st): State<AppState>, Query(q): Query<AdminQuery>) -> Response {
    let msg = if q.welcome.is_some() {
        "Setup finished. You are logged in as admin.".into()
    } else {
        String::new()
    };
    admin_render(&st, msg, String::new(), Vec::new()).await
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
    new2: String,
}

async fn change_admin_password(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(f): Form<PasswordChange>,
) -> Response {
    if let Err(e) = check_admin_password(&st, addr, f.current).await {
        return admin_render(&st, String::new(), e, Vec::new()).await;
    }
    if f.new.is_empty() || f.new != f.new2 {
        return admin_render(
            &st,
            String::new(),
            "New passwords are empty or don't match.".into(),
            Vec::new(),
        )
        .await;
    }
    let hash = setup::hash_off_thread(f.new).await;
    let saved = async {
        let mut tx = st.db.begin().await?;
        s::set(&mut *tx, s::ADMIN_PASSWORD_HASH, &hash).await?;
        s::bump_session_generation(&mut *tx).await?;
        tx.commit().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = saved {
        return admin_render(
            &st,
            String::new(),
            format!("Saving failed: {e}"),
            Vec::new(),
        )
        .await;
    }
    tracing::info!("admin password changed; other admin sessions logged out");
    // This browser gets a fresh session; every other session is now invalid.
    let generation = s::session_generation(&st.db).await.unwrap_or(0);
    let resp = admin_render(
        &st,
        "Admin password changed. Other admin sessions were logged out.".into(),
        String::new(),
        Vec::new(),
    )
    .await;
    with_admin_cookie(resp, &st.signer.issue(generation))
}

async fn change_playlist_password(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(f): Form<PasswordChange>,
) -> Response {
    if let Err(e) = check_admin_password(&st, addr, f.current).await {
        return admin_render(&st, String::new(), e, Vec::new()).await;
    }
    if f.new.is_empty() || f.new != f.new2 {
        return admin_render(
            &st,
            String::new(),
            "New passwords are empty or don't match.".into(),
            Vec::new(),
        )
        .await;
    }
    let hash = setup::hash_off_thread(f.new).await;
    match s::set(&st.db, s::PLAYLIST_PASSWORD_HASH, &hash).await {
        Ok(()) => {
            admin_render(
                &st,
                "Playlist password changed. Update it in your IPTV players.".into(),
                String::new(),
                Vec::new(),
            )
            .await
        }
        Err(e) => {
            admin_render(
                &st,
                String::new(),
                format!("Saving failed: {e}"),
                Vec::new(),
            )
            .await
        }
    }
}

#[derive(Deserialize)]
struct Reauth {
    password: String,
}

/// Decrypt and show stored credentials, after re-entering the admin password.
async fn show_credentials(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(f): Form<Reauth>,
) -> Response {
    if let Err(e) = check_admin_password(&st, addr, f.password).await {
        return admin_render(&st, String::new(), e, Vec::new()).await;
    }
    let mut creds = Vec::new();
    for (name, label) in s::ALL_SECRETS {
        let value = match s::get_secret(&st.db, &st.key, name).await {
            Ok(Some(v)) => v,
            Ok(None) => "(not set)".into(),
            Err(e) => format!("(error: {e})"),
        };
        creds.push(((*label).to_string(), value));
    }
    tracing::info!("stored credentials were shown to admin at {}", addr.ip());
    let mut resp = admin_render(
        &st,
        "Credentials are shown below. Reload the page to hide them.".into(),
        String::new(),
        creds,
    )
    .await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("header"));
    resp
}

/// The original M3Usort icon, built into the binary.
pub async fn favicon() -> Response {
    const ICON: &[u8] = include_bytes!("../static/favicon.png");
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        ICON,
    )
        .into_response()
}

// ------------------------------------------------------------------ blocked pages

/// Why the app can't start normally. Shown as a page instead of crash-looping,
/// so it's visible in the browser and the container doesn't restart endlessly.
#[derive(Clone)]
pub enum Blocked {
    /// Fresh install without M3USORT_KEY. Carries a freshly generated key to use.
    NeedKey { suggested: String },
    /// A database exists but M3USORT_KEY is not set.
    KeyMissingForExistingDb,
    /// M3USORT_KEY does not match the key this database was set up with.
    KeyMismatch,
}

pub fn blocked_router(reason: Blocked) -> Router {
    let page = Html(render_blocked(&reason));
    Router::new()
        .route(
            "/healthcheck",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "blocked") }),
        )
        .route("/favicon.png", get(favicon))
        .route("/favicon.ico", get(favicon))
        .fallback(move || {
            let page = page.clone();
            async move { (StatusCode::SERVICE_UNAVAILABLE, page).into_response() }
        })
}

fn render_blocked(reason: &Blocked) -> String {
    let body = match reason {
        Blocked::NeedKey { suggested } => format!(
            r#"<h1>Step 1 of setup: encryption key</h1>
<p>M3Usort encrypts your credentials with a key that lives in your <code>.env</code> file, not in the database.
A key has been generated for you:</p>
<pre class="key">M3USORT_KEY={suggested}</pre>
<ol>
<li>Copy this line into the <code>.env</code> file next to your <code>docker-compose.yml</code>.</li>
<li><strong>Also store it in your password manager.</strong> Without it, a backup's credentials cannot be restored.</li>
<li>Recreate the container so it picks up the new <code>.env</code>: <code>docker compose up -d</code> (or <em>Redeploy</em> in Arcane).
A plain restart is not enough: environment variables are only read when the container is created.</li>
</ol>
<p>The setup wizard continues after that. A new key is shown on every start until one is set; any of them is fine to use.</p>
<p>Prefer your own passphrase? That works too: put it in <code>M3USORT_KEY</code> instead. It is stretched with Argon2id.
A long random key like the one above is stronger; a short passphrase can be brute-forced if someone gets hold of the database or a backup.</p>"#
        ),
        Blocked::KeyMissingForExistingDb => r#"<h1>M3USORT_KEY is missing</h1>
<p>A database already exists, but <code>M3USORT_KEY</code> is not set. Put the key you saved during setup back in <code>.env</code> and recreate the container.</p>
<p>A new key is deliberately <strong>not</strong> generated here: it could not decrypt the credentials already stored.</p>"#
            .to_string(),
        Blocked::KeyMismatch => r#"<h1>M3USORT_KEY does not match this database</h1>
<p>The key in <code>.env</code> is not the key this database was set up with. Nothing was changed.</p>
<p>Use the key from your password manager. If you just rotated the key, use the new key that <code>rotate-key</code> printed.</p>"#
            .to_string(),
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>M3Usort – action needed</title><link rel="icon" type="image/png" href="/favicon.png"><style>
body{{font-family:system-ui,sans-serif;background:#16181d;color:#e6e6e6;max-width:46rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}
code,pre{{background:#23262e;border-radius:4px;padding:.1rem .3rem}} pre.key{{padding:1rem;user-select:all;overflow-x:auto}}
h1{{color:#7cc4ff;font-size:1.4rem}} footer{{margin-top:3rem;color:#777;font-size:.8rem}}
</style></head><body>{body}<footer>{}</footer></body></html>"#,
        version::version_line()
    )
}
