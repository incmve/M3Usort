//! Connection tests used by the setup wizard (and later by Settings).
//! Each returns a short human-readable description on success.

use std::path::Path;
use std::time::Duration;

use reqwest::{Client, Url};
use serde_json::Value;

pub const DEFAULT_PROVIDER_UA: &str = "VLC/3.0.20 LibVLC/3.0.20";

pub fn http_client() -> anyhow::Result<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("m3usort/{}", crate::version::VERSION))
        .build()?)
}

pub fn provider_user_agent() -> String {
    std::env::var("PROVIDER_USER_AGENT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER_UA.into())
}

/// Xtream Codes API login: server + username + password.
#[derive(Clone, Debug, PartialEq)]
pub struct Xtream {
    /// `http(s)://host[:port][/prefix]`, no trailing slash.
    pub server: String,
    pub username: String,
    pub password: String,
}

impl Xtream {
    /// Clean up a server address: scheme + host + optional port/prefix, with any
    /// `/get.php` or `/player_api.php` and query string removed.
    pub fn normalize_server(raw: &str) -> Result<String, String> {
        let raw = raw.trim();
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        let mut url =
            Url::parse(&with_scheme).map_err(|e| format!("Not a valid server address: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Server must start with http:// or https://".into());
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err("Server address has no host".into());
        }
        url.set_query(None);
        url.set_fragment(None);
        let path = url.path().trim_end_matches('/').to_string();
        let path = path
            .strip_suffix("/get.php")
            .or_else(|| path.strip_suffix("/player_api.php"))
            .unwrap_or(&path)
            .to_string();
        url.set_path(&path);
        Ok(url.as_str().trim_end_matches('/').to_string())
    }

    /// Split a pasted playlist URL (`…/get.php?username=u&password=p&…`) into its parts.
    /// `None` if it doesn't carry both username and password.
    pub fn from_playlist_url(raw: &str) -> Option<Self> {
        let url = Url::parse(raw.trim()).ok()?;
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        Some(Self {
            server: Self::normalize_server(raw).ok()?,
            username: q.get("username")?.clone(),
            password: q.get("password")?.clone(),
        })
    }

    fn with_login(&self, path: &str) -> Result<Url, String> {
        let mut u = Url::parse(&format!("{}/{path}", self.server)).map_err(|e| e.to_string())?;
        u.query_pairs_mut()
            .append_pair("username", &self.username)
            .append_pair("password", &self.password);
        Ok(u)
    }

    /// `player_api.php?username=…&password=…&action=…`
    pub fn api_url(&self, action: &str) -> Result<Url, String> {
        let mut u = self.with_login("player_api.php")?;
        u.query_pairs_mut().append_pair("action", action);
        Ok(u)
    }

    /// Live TV playlist: `get.php?username=…&password=…&type=m3u_plus&output=ts`.
    #[allow(dead_code)] // used by the playlist milestone
    pub fn m3u_url(&self) -> Result<Url, String> {
        let mut u = self.with_login("get.php")?;
        u.query_pairs_mut()
            .append_pair("type", "m3u_plus")
            .append_pair("output", "ts");
        Ok(u)
    }
}

/// Xtream `player_api.php` → `user_info.auth == 1`.
pub async fn test_provider(client: &Client, x: &Xtream) -> Result<String, String> {
    let api = x.api_url("get_user_info")?;
    let resp = client
        .get(api)
        .header(reqwest::header::USER_AGENT, provider_user_agent())
        .send()
        .await
        .map_err(|e| format!("Cannot reach {}: {}", x.server, root_cause(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{} answered HTTP {status}", x.server));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| "Answer is not Xtream JSON. Is this the right URL?".to_string())?;
    let info = body
        .get("user_info")
        .ok_or("Answer has no user_info. Is this an Xtream API?")?;
    let auth = info.get("auth").map(as_text).unwrap_or_default();
    if auth != "1" {
        return Err("Login refused: wrong username or password".into());
    }
    let status = info
        .get("status")
        .map(as_text)
        .unwrap_or_else(|| "?".into());
    let max = info
        .get("max_connections")
        .map(as_text)
        .unwrap_or_else(|| "?".into());
    let exp = info
        .get("exp_date")
        .map(as_text)
        .and_then(|s| s.parse::<i64>().ok())
        .map(unix_to_date)
        .unwrap_or_else(|| "unknown".into());
    Ok(format!(
        "Logged in. Status: {status}, expires: {exp}, max connections: {max}"
    ))
}

/// Jellyfin `GET /System/Info` with the API key.
pub async fn test_jellyfin(client: &Client, base: &str, api_key: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    let url =
        Url::parse(&format!("{base}/System/Info")).map_err(|e| format!("Not a valid URL: {e}"))?;
    let resp = client
        .get(url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("MediaBrowser Token=\"{}\"", api_key.trim()),
        )
        .send()
        .await
        .map_err(|e| format!("Cannot reach {base}: {}", root_cause(&e)))?;
    match resp.status().as_u16() {
        200 => {}
        401 | 403 => return Err("Jellyfin refused the API key".into()),
        s => return Err(format!("Jellyfin answered HTTP {s}")),
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| "Answer is not Jellyfin JSON".to_string())?;
    let name = body
        .get("ServerName")
        .map(as_text)
        .unwrap_or_else(|| "?".into());
    let version = body
        .get("Version")
        .map(as_text)
        .unwrap_or_else(|| "?".into());
    Ok(format!("Connected to \"{name}\", Jellyfin {version}"))
}

pub const TMDB_BASE: &str = "https://api.themoviedb.org";

/// Ask Jellyfin to rescan its libraries (as the Python version does after new files).
pub async fn jellyfin_refresh(client: &Client, base: &str, api_key: &str) -> Result<(), String> {
    let url = format!("{}/Library/Refresh", base.trim().trim_end_matches('/'));
    let resp = client
        .post(url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("MediaBrowser Token=\"{}\"", api_key.trim()),
        )
        .send()
        .await
        .map_err(|e| format!("cannot reach Jellyfin: {}", root_cause(&e)))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("Jellyfin answered HTTP {}", resp.status()))
    }
}

/// TMDB key test, using the same call the Python version uses in production:
/// `GET /3/search/movie?api_key=…&query=…&language=en-US` (v3 API key as query
/// parameter). A v4 read access token (a JWT, starts with `eyJ`) is sent as a
/// Bearer header instead, which TMDB also accepts on v3 endpoints.
pub async fn test_tmdb(client: &Client, key: &str) -> Result<String, String> {
    test_tmdb_at(client, TMDB_BASE, key).await
}

pub async fn test_tmdb_at(client: &Client, base: &str, key: &str) -> Result<String, String> {
    let key = key.trim();
    let mut url = Url::parse(&format!("{base}/3/search/movie")).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("query", "Inception")
        .append_pair("language", "en-US");
    let req = if key.starts_with("eyJ") {
        client.get(url).bearer_auth(key)
    } else {
        url.query_pairs_mut().append_pair("api_key", key);
        client.get(url)
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Cannot reach TMDB: {}", root_cause(&e)))?;
    match resp.status().as_u16() {
        200 => {}
        401 => return Err("TMDB refused the key (HTTP 401)".into()),
        s => return Err(format!("TMDB answered HTTP {s}")),
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| "Answer is not TMDB JSON".to_string())?;
    let n = body
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    Ok(format!(
        "TMDB key accepted (test search returned {n} result(s))"
    ))
}

/// Folder must exist inside the container and be writable by the app's user.
pub async fn test_folder(path: &str) -> Result<String, String> {
    let path = path.trim();
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("Use an absolute path inside the container, e.g. /data/media/movies".into());
    }
    if !p.is_dir() {
        return Err(format!(
            "{path} does not exist inside the container. Check the volume mount in docker-compose.yml"
        ));
    }
    let probe = p.join(".m3usort-write-test");
    tokio::fs::write(&probe, b"ok").await.map_err(|e| {
        format!("Cannot write in {path}: {e}. Check PUID/PGID and folder permissions")
    })?;
    let _ = tokio::fs::remove_file(&probe).await;
    Ok("Exists and is writable".into())
}

pub fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn root_cause(e: &reqwest::Error) -> String {
    let mut src: &dyn std::error::Error = e;
    while let Some(next) = src.source() {
        src = next;
    }
    if e.is_timeout() {
        "timed out".into()
    } else {
        src.to_string()
    }
}

pub fn unix_to_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_normalization() {
        let n = |s: &str| Xtream::normalize_server(s).unwrap();
        assert_eq!(
            n("http://iptvproxy.local:8080"),
            "http://iptvproxy.local:8080"
        );
        assert_eq!(
            n("http://iptvproxy.local:8080/"),
            "http://iptvproxy.local:8080"
        );
        assert_eq!(n("iptvproxy.local:8080"), "http://iptvproxy.local:8080");
        assert_eq!(
            n("http://h:8080/get.php?username=u&password=p"),
            "http://h:8080"
        );
        assert_eq!(n("https://h/xc/player_api.php"), "https://h/xc");
        assert!(Xtream::normalize_server("ftp://h").is_err());
    }

    #[test]
    fn splits_pasted_playlist_url() {
        let x = Xtream::from_playlist_url(
            "http://iptvproxy.local:8080/get.php?username=u&password=p%26q&type=m3u_plus",
        )
        .unwrap();
        assert_eq!(x.server, "http://iptvproxy.local:8080");
        assert_eq!((x.username.as_str(), x.password.as_str()), ("u", "p&q"));
        assert!(Xtream::from_playlist_url("http://x/get.php?username=u").is_none());
    }

    #[test]
    fn builds_urls_with_encoding() {
        let x = Xtream {
            server: "http://h:8080".into(),
            username: "u".into(),
            password: "a&b#c".into(),
        };
        let api = x.api_url("get_user_info").unwrap();
        assert_eq!(
            api.as_str(),
            "http://h:8080/player_api.php?username=u&password=a%26b%23c&action=get_user_info"
        );
        assert!(
            x.m3u_url()
                .unwrap()
                .as_str()
                .ends_with("/get.php?username=u&password=a%26b%23c&type=m3u_plus&output=ts")
        );
    }

    #[test]
    fn dates() {
        assert_eq!(unix_to_date(0), "1970-01-01");
        assert_eq!(unix_to_date(2_524_876_541), "2050-01-04");
    }

    #[tokio::test]
    async fn folder_checks() {
        assert!(test_folder("relative/path").await.is_err());
        assert!(test_folder("/definitely/not/here").await.is_err());
        let d = std::env::temp_dir();
        assert!(test_folder(d.to_str().unwrap()).await.is_ok());
    }

    /// Tiny in-process stand-in for TMDB and an Xtream proxy.
    async fn mock_server() -> String {
        use axum::extract::Query;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;
        use std::collections::HashMap;
        let app = axum::Router::new()
            .route(
                "/3/search/movie",
                get(|Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
                    let bearer = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer eyJgood");
                    let ok = q.get("api_key").map(String::as_str) == Some("goodkey") || bearer;
                    if ok && q.contains_key("query") && q.get("language").map(String::as_str) == Some("en-US") {
                        (StatusCode::OK, r#"{"results":[{"id":27205}]}"#)
                    } else {
                        (StatusCode::UNAUTHORIZED, r#"{"status_code":7}"#)
                    }
                }),
            )
            .route(
                "/player_api.php",
                get(|Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
                    let ua_ok = h.get("user-agent").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("VLC"));
                    let ok = ua_ok && q.get("username").map(String::as_str) == Some("u") && q.get("password").map(String::as_str) == Some("p");
                    format!(r#"{{"user_info":{{"auth":{},"status":"Active","exp_date":"2524876541","max_connections":"2"}}}}"#, u8::from(ok))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn local_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn tmdb_like_python_version() {
        let base = mock_server().await;
        let c = local_client();
        assert!(
            test_tmdb_at(&c, &base, "goodkey")
                .await
                .unwrap()
                .contains("1 result")
        );
        assert!(
            test_tmdb_at(&c, &base, "eyJgood").await.is_ok(),
            "v4 token as Bearer"
        );
        assert!(
            test_tmdb_at(&c, &base, "badkey")
                .await
                .unwrap_err()
                .contains("401")
        );
    }

    #[tokio::test]
    async fn provider_login() {
        let base = mock_server().await;
        let c = local_client();
        let x = |pw: &str| Xtream {
            server: base.clone(),
            username: "u".into(),
            password: pw.into(),
        };
        let ok = test_provider(&c, &x("p")).await.unwrap();
        assert!(ok.contains("2050-01-04") && ok.contains("max connections: 2"));
        assert!(
            test_provider(&c, &x("wrong"))
                .await
                .unwrap_err()
                .contains("refused")
        );
    }
}
