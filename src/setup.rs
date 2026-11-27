//! First-run setup wizard.
//!
//! Steps: admin → provider → folders → Jellyfin → TMDB → schedules → summary.
//! Every step validates (and where possible tests a live connection) before Next.
//! Answers are kept in memory only; nothing is written until Finish, which saves
//! everything in one transaction. Step 1 requires the one-time setup code from the
//! container log, so nobody else on the LAN can claim the admin account first.

use std::collections::HashMap;
use std::net::SocketAddr;

use askama::Template;
use axum::Form;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::auth::{self, ADMIN_COOKIE, SESSION_TTL};
use crate::integrations::{self, ProviderUrl};
use crate::settings as s;
use crate::web::{AppState, Page};

pub const SETUP_COOKIE: &str = "m3usort_setup";
pub const LAST_STEP: u8 = 6;

/// Everything the wizard collects. Defaults match the Python version's config.sample.
#[derive(Clone)]
pub struct Draft {
    admin_hash: Option<String>,
    playlist_hash: Option<String>,
    provider_url: String,
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
    /// Highest step completed, so you can't skip ahead.
    done: u8,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            admin_hash: None,
            playlist_hash: None,
            provider_url: String::new(),
            provider_result: String::new(),
            movies_dir: "/data/media/movies".into(),
            series_dir: "/data/media/tv".into(),
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
            done: 0,
        }
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
    pub help: &'static str,
    pub kind: Kind,
    pub value: String,
    pub checked: bool,
}

impl Field {
    fn text(name: &'static str, label: &'static str, help: &'static str, value: &str) -> Self {
        Self {
            name,
            label,
            help,
            kind: Kind::Text,
            value: value.into(),
            checked: false,
        }
    }
    fn password(name: &'static str, label: &'static str, help: &'static str) -> Self {
        Self {
            name,
            label,
            help,
            kind: Kind::Password,
            value: String::new(),
            checked: false,
        }
    }
    fn number(
        name: &'static str,
        label: &'static str,
        help: &'static str,
        value: &str,
        min: u32,
        max: u32,
    ) -> Self {
        Self {
            name,
            label,
            help,
            kind: Kind::Number { min, max },
            value: value.into(),
            checked: false,
        }
    }
    fn time(name: &'static str, label: &'static str, value: &str) -> Self {
        Self {
            name,
            label,
            help: "",
            kind: Kind::Time,
            value: value.into(),
            checked: false,
        }
    }
    fn checkbox(
        name: &'static str,
        label: &'static str,
        help: &'static str,
        checked: bool,
    ) -> Self {
        Self {
            name,
            label,
            help,
            kind: Kind::Checkbox,
            value: String::new(),
            checked,
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
    pub fn is_select(&self) -> bool {
        matches!(self.kind, Kind::Select(_))
    }
}

struct StepInfo {
    title: &'static str,
    intro: &'static str,
}

fn step_info(step: u8) -> StepInfo {
    match step {
        1 => StepInfo {
            title: "Admin",
            intro: "Enter the setup code from the container log (Arcane → container → Logs) and choose the passwords.",
        },
        2 => StepInfo {
            title: "Provider / proxy",
            intro: "The playlist URL of your iptv-proxy (or provider). The login is tested before you can continue.",
        },
        3 => StepInfo {
            title: "Folders",
            intro: "Where .strm files are written, as seen inside the container. Both folders must exist and be writable.",
        },
        4 => StepInfo {
            title: "Jellyfin (optional)",
            intro: "Used to refresh the library after new files and to show what is already in Jellyfin.",
        },
        5 => StepInfo {
            title: "TMDB (optional)",
            intro: "Used for posters, cast and actor search. Free key at themoviedb.org → Settings → API.",
        },
        _ => StepInfo {
            title: "Schedules",
            intro: "How often playlists and VOD are refreshed. You can change all of this later in Settings.",
        },
    }
}

fn fields(step: u8, d: &Draft) -> Vec<Field> {
    let set = |b: bool| {
        if b {
            "Already set. Leave empty to keep it."
        } else {
            ""
        }
    };
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
                set(d.admin_hash.is_some()),
            ),
            Field::password("admin_password2", "Admin password (again)", ""),
            Field::password(
                "playlist_password",
                "Playlist password",
                if d.playlist_hash.is_some() {
                    "Already set. Leave empty to keep it."
                } else {
                    "IPTV players use this to download your sorted playlist."
                },
            ),
            Field::password("playlist_password2", "Playlist password (again)", ""),
        ],
        2 => vec![Field::text(
            "provider_url",
            "Playlist URL",
            "e.g. http://iptvproxy.local:8080/get.php?username=…&password=…&type=m3u_plus",
            &d.provider_url,
        )],
        3 => vec![
            Field::text("movies_dir", "Movies folder", "", &d.movies_dir),
            Field::text("series_dir", "Series folder", "", &d.series_dir),
        ],
        4 => vec![
            Field::checkbox("jellyfin_enabled", "Use Jellyfin", "", d.jellyfin_enabled),
            Field::text(
                "jellyfin_url",
                "Jellyfin URL",
                "e.g. http://192.168.1.10:8096",
                &d.jellyfin_url,
            ),
            Field {
                value: d.jellyfin_api_key.clone(),
                ..Field::password(
                    "jellyfin_api_key",
                    "Jellyfin API key",
                    "Jellyfin → Dashboard → API Keys",
                )
            },
        ],
        5 => vec![Field {
            value: d.tmdb_api_key.clone(),
            ..Field::password(
                "tmdb_api_key",
                "TMDB API key",
                "API key (v3) or read access token (v4). Leave empty to skip.",
            )
        }],
        _ => vec![
            Field::number(
                "playlist_refresh_hours",
                "Refresh playlist every (hours)",
                "",
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
                "",
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
            Field {
                value: d.match_type.clone(),
                ..Field {
                    name: "match_type",
                    label: "Matching method",
                    help: "How wanted titles are matched to the provider's titles",
                    kind: Kind::Select(vec![
                        ("string", "Exact (string comparison)"),
                        ("fuzzy", "Fuzzy"),
                    ]),
                    value: String::new(),
                    checked: false,
                }
            },
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
    }
}

// ------------------------------------------------------------------ templates

#[derive(Template)]
#[template(path = "setup_start.html")]
struct StartTpl {
    page: Page,
}

#[derive(Template)]
#[template(path = "setup_step.html")]
struct StepTpl {
    page: Page,
    step: u8,
    last_step: u8,
    title: &'static str,
    intro: &'static str,
    fields: Vec<Field>,
    error: String,
    success: String,
}

#[derive(Template)]
#[template(path = "setup_summary.html")]
struct SummaryTpl {
    page: Page,
    rows: Vec<(String, String)>,
    error: String,
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

fn step_page(step: u8, d: &Draft, error: String, success: String) -> Response {
    let info = step_info(step);
    render(StepTpl {
        page: Page::new(&format!("Setup – {}", info.title)),
        step,
        last_step: LAST_STEP,
        title: info.title,
        intro: info.intro,
        fields: fields(step, d),
        error,
        success,
    })
}

// ------------------------------------------------------------------ handlers

pub async fn start(State(st): State<AppState>) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    render(StartTpl {
        page: Page::new("Setup"),
    })
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
            Some(d) if d.done + 1 >= step => d,
            _ => return Redirect::to("/setup/1").into_response(),
        }
    };
    step_page(step, &d, String::new(), String::new())
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
    let f = |k: &str| {
        form.get(k)
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let on = |k: &str| form.contains_key(k);

    // ---- step 1: setup code + passwords, starts a new draft
    if step == 1 {
        let ip = addr.ip();
        let mut d = draft_for(&st, &headers).unwrap_or_default();
        if let Err(min) = st.limiter.check(ip) {
            return step_page(
                1,
                &d,
                format!("Too many wrong setup codes. Try again in {min} minutes."),
                String::new(),
            );
        }
        if !f("setup_code").eq_ignore_ascii_case(&st.setup_code) {
            st.limiter.fail(ip);
            return step_page(
                1,
                &d,
                "Wrong setup code. It is in the container log.".into(),
                String::new(),
            );
        }
        st.limiter.reset(ip);

        let (a1, a2) = (
            form.get("admin_password").cloned().unwrap_or_default(),
            form.get("admin_password2").cloned().unwrap_or_default(),
        );
        let (p1, p2) = (
            form.get("playlist_password").cloned().unwrap_or_default(),
            form.get("playlist_password2").cloned().unwrap_or_default(),
        );
        if a1 != a2 {
            return step_page(
                1,
                &d,
                "The admin passwords don't match.".into(),
                String::new(),
            );
        }
        if p1 != p2 {
            return step_page(
                1,
                &d,
                "The playlist passwords don't match.".into(),
                String::new(),
            );
        }
        if a1.is_empty() && d.admin_hash.is_none() {
            return step_page(1, &d, "Choose an admin password.".into(), String::new());
        }
        if p1.is_empty() && d.playlist_hash.is_none() {
            return step_page(1, &d, "Choose a playlist password.".into(), String::new());
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
        return resp;
    }

    // ---- steps 2..6 need a draft
    let Some(mut d) = draft_for(&st, &headers) else {
        return Redirect::to("/setup/1").into_response();
    };
    if d.done + 1 < step {
        return Redirect::to("/setup/1").into_response();
    }

    let result: Result<(), String> = match step {
        2 => {
            d.provider_url = f("provider_url");
            match integrations::test_provider(&st.http, &d.provider_url).await {
                Ok(msg) => {
                    d.provider_result = msg;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        3 => {
            d.movies_dir = f("movies_dir");
            d.series_dir = f("series_dir");
            match (
                integrations::test_folder(&d.movies_dir).await,
                integrations::test_folder(&d.series_dir).await,
            ) {
                (Ok(_), Ok(_)) => Ok(()),
                (Err(e), _) => Err(format!("Movies folder: {e}")),
                (_, Err(e)) => Err(format!("Series folder: {e}")),
            }
        }
        4 => {
            d.jellyfin_enabled = on("jellyfin_enabled");
            d.jellyfin_url = f("jellyfin_url").trim_end_matches('/').to_string();
            d.jellyfin_api_key = f("jellyfin_api_key");
            if !d.jellyfin_enabled {
                d.jellyfin_result = "Not used".into();
                Ok(())
            } else if d.jellyfin_url.is_empty() || d.jellyfin_api_key.is_empty() {
                Err("Fill in the Jellyfin URL and API key, or untick \"Use Jellyfin\".".into())
            } else {
                match integrations::test_jellyfin(&st.http, &d.jellyfin_url, &d.jellyfin_api_key)
                    .await
                {
                    Ok(msg) => {
                        d.jellyfin_result = msg;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
        5 => {
            d.tmdb_api_key = f("tmdb_api_key");
            if d.tmdb_api_key.is_empty() {
                d.tmdb_result = "Not used".into();
                Ok(())
            } else {
                match integrations::test_tmdb(&st.http, &d.tmdb_api_key).await {
                    Ok(msg) => {
                        d.tmdb_result = msg;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
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
            validate_schedules(&d)
        }
        _ => return Redirect::to("/setup").into_response(),
    };

    match result {
        Err(e) => {
            store_draft(&st, &headers, d.clone()); // keep what was typed
            step_page(step, &d, e, String::new())
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
        && d.playlist_output != ".."
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

fn yes(b: bool) -> String {
    if b { "Yes".into() } else { "No".into() }
}

fn summary_rows(d: &Draft) -> Vec<(String, String)> {
    let mut rows = vec![
        (
            "Admin password".into(),
            "Set (stored as Argon2id hash)".into(),
        ),
        (
            "Playlist password".into(),
            "Set (stored as Argon2id hash)".into(),
        ),
        ("Provider URL".into(), ProviderUrl::masked(&d.provider_url)),
        ("Provider test".into(), d.provider_result.clone()),
        ("Movies folder".into(), d.movies_dir.clone()),
        ("Series folder".into(), d.series_dir.clone()),
        (
            "Jellyfin".into(),
            if d.jellyfin_enabled {
                format!("{} — {}", d.jellyfin_url, d.jellyfin_result)
            } else {
                "Not used".into()
            },
        ),
        ("TMDB".into(), d.tmdb_result.clone()),
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
            if d.vod_scheduler_enabled {
                format!("every {} h", d.vod_scan_hours)
            } else {
                "Off".into()
            },
        ),
        (
            "Overwrite movies / episodes".into(),
            format!("{} / {}", yes(d.overwrite_movies), yes(d.overwrite_series)),
        ),
        ("Matching".into(), d.match_type.clone()),
    ];
    rows.push((
        "TMDB enrichment".into(),
        if d.tmdb_enrich_enabled {
            format!("daily at {}", d.tmdb_enrich_time)
        } else {
            "Off".into()
        },
    ));
    rows.push((
        "Cast enrichment".into(),
        if d.cast_enrich_enabled {
            format!("daily at {}", d.cast_enrich_time)
        } else {
            "Off".into()
        },
    ));
    rows
}

pub async fn summary(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if st.is_setup_done() {
        return Redirect::to("/").into_response();
    }
    match draft_for(&st, &headers) {
        Some(d) if d.done >= LAST_STEP => render(SummaryTpl {
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
    let Some(d) = draft_for(&st, &headers).filter(|d| d.done >= LAST_STEP) else {
        return Redirect::to("/setup/1").into_response();
    };
    if let Err(e) = save(&st, &d).await {
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

/// Everything in one transaction: either setup is complete or nothing changed.
async fn save(st: &AppState, d: &Draft) -> anyhow::Result<()> {
    let mut tx = st.db.begin().await?;
    let b = |v: bool| if v { "1" } else { "0" };
    let plain: Vec<(&str, String)> = vec![
        (
            s::ADMIN_PASSWORD_HASH,
            d.admin_hash.clone().unwrap_or_default(),
        ),
        (
            s::PLAYLIST_PASSWORD_HASH,
            d.playlist_hash.clone().unwrap_or_default(),
        ),
        (s::MOVIES_DIR, d.movies_dir.clone()),
        (s::SERIES_DIR, d.series_dir.clone()),
        (s::JELLYFIN_ENABLED, b(d.jellyfin_enabled).into()),
        (s::JELLYFIN_URL, d.jellyfin_url.clone()),
        (s::PLAYLIST_OUTPUT, d.playlist_output.clone()),
        (s::PLAYLIST_REFRESH_HOURS, d.playlist_refresh_hours.clone()),
        (s::CUSTOM_GROUP_TITLE, d.custom_group_title.clone()),
        (s::VOD_SCHEDULER_ENABLED, b(d.vod_scheduler_enabled).into()),
        (s::VOD_SCAN_HOURS, d.vod_scan_hours.clone()),
        (s::OVERWRITE_MOVIES, b(d.overwrite_movies).into()),
        (s::OVERWRITE_SERIES, b(d.overwrite_series).into()),
        (s::MATCH_TYPE, d.match_type.clone()),
        (s::TMDB_ENRICH_ENABLED, b(d.tmdb_enrich_enabled).into()),
        (s::TMDB_ENRICH_TIME, d.tmdb_enrich_time.clone()),
        (s::CAST_ENRICH_ENABLED, b(d.cast_enrich_enabled).into()),
        (s::CAST_ENRICH_TIME, d.cast_enrich_time.clone()),
    ];
    anyhow::ensure!(
        !plain[0].1.is_empty() && !plain[1].1.is_empty(),
        "passwords missing"
    );
    for (name, value) in &plain {
        s::set(&mut *tx, name, value).await?;
    }
    s::set_secret(&mut *tx, &st.key, s::SECRET_PROVIDER_URL, &d.provider_url).await?;
    if d.jellyfin_enabled {
        s::set_secret(
            &mut *tx,
            &st.key,
            s::SECRET_JELLYFIN_API_KEY,
            &d.jellyfin_api_key,
        )
        .await?;
    }
    if !d.tmdb_api_key.is_empty() {
        s::set_secret(&mut *tx, &st.key, s::SECRET_TMDB_API_KEY, &d.tmdb_api_key).await?;
    }
    s::set(&mut *tx, s::SESSION_GENERATION, "1").await?;
    s::set(&mut *tx, s::SETUP_COMPLETED, "1").await?;
    tx.commit().await?;
    Ok(())
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
        d.vod_scan_hours = "0".into();
        assert!(validate_schedules(&d).is_err());
        d = Draft::default();
        d.cast_enrich_time = "24:00".into();
        assert!(validate_schedules(&d).is_err());
    }
}
