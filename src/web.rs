//! HTTP layer. For now: the normal app placeholder and the "blocked" pages that
//! are shown when the key is missing or wrong. The setup wizard comes next.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::version::{self, SharedStatus, UpdateStatus};

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

#[derive(Clone)]
struct AppState {
    #[allow(dead_code)]
    db: SqlitePool,
    update: SharedStatus,
}

pub fn app_router(db: SqlitePool, update: SharedStatus) -> Router {
    Router::new()
        .route("/healthcheck", get(|| async { "ok" }))
        .route("/api/version", get(api_version))
        .route("/", get(home))
        .with_state(AppState { db, update })
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

async fn home(State(st): State<AppState>) -> Html<String> {
    let u = st.update.read().await.clone();
    let banner = match (&u.latest, &u.release_url) {
        (Some(latest), Some(url)) if u.update_available => format!(
            r#"<p class="update">Update available: <strong>{}</strong> (you run {}). <a href="{}">Release notes</a></p>"#,
            escape(latest),
            version::VERSION,
            escape(url)
        ),
        _ => String::new(),
    };
    Html(layout(
        "M3Usort",
        &format!("{banner}<h1>M3Usort</h1><p>Key OK. The setup wizard is the next milestone.</p>"),
    ))
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
    layout("M3Usort – action needed", &body)
}

fn layout(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{}</title><style>
body{{font-family:system-ui,sans-serif;background:#16181d;color:#e6e6e6;max-width:46rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}
code,pre{{background:#23262e;border-radius:4px;padding:.1rem .3rem}} pre.key{{padding:1rem;user-select:all;overflow-x:auto}}
h1{{color:#7cc4ff;font-size:1.4rem}} a{{color:#7cc4ff}}
.update{{background:#1f3a24;border:1px solid #3d7a4a;border-radius:6px;padding:.6rem .8rem}}
footer{{margin-top:3rem;color:#777;font-size:.8rem}}
</style></head><body>{}<footer>{}</footer></body></html>"#,
        escape(title),
        body,
        escape(&version::version_line())
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
