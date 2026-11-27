//! HTTP layer: routing, guards, guest/admin pages, and the "blocked" pages shown
//! when the key is missing or wrong.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::{self, ADMIN_COOKIE, LoginLimiter, SESSION_TTL, SessionSigner};
use crate::crypto::MasterKey;
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
        .route("/api/version", get(api_version))
        .route("/", get(home))
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
}

async fn home(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let u = st.update.read().await.clone();
    let update_banner = match (u.latest, u.release_url) {
        (Some(latest), Some(url)) if u.update_available => Some((latest, url)),
        _ => None,
    };
    render(HomeTpl {
        page: Page::nav("M3Usort", st.is_admin(&headers).await),
        update_banner,
    })
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
<title>M3Usort – action needed</title><style>
body{{font-family:system-ui,sans-serif;background:#16181d;color:#e6e6e6;max-width:46rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}
code,pre{{background:#23262e;border-radius:4px;padding:.1rem .3rem}} pre.key{{padding:1rem;user-select:all;overflow-x:auto}}
h1{{color:#7cc4ff;font-size:1.4rem}} footer{{margin-top:3rem;color:#777;font-size:.8rem}}
</style></head><body>{body}<footer>{}</footer></body></html>"#,
        version::version_line()
    )
}
