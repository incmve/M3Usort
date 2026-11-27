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

/// Parsed provider/proxy playlist URL, e.g.
/// `http://iptvproxy.local:8080/get.php?username=u&password=p&type=m3u_plus`.
pub struct ProviderUrl {
    pub base: String,
    pub username: String,
    pub password: String,
}

impl ProviderUrl {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let url = Url::parse(raw.trim()).map_err(|e| format!("Not a valid URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("URL must start with http:// or https://".into());
        }
        let host = url.host_str().ok_or("URL has no host")?;
        let mut username = None;
        let mut password = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "username" => username = Some(v.into_owned()),
                "password" => password = Some(v.into_owned()),
                _ => {}
            }
        }
        let base = match url.port() {
            Some(p) => format!("{}://{host}:{p}", url.scheme()),
            None => format!("{}://{host}", url.scheme()),
        };
        Ok(Self {
            base,
            username: username.ok_or("URL has no username=… parameter")?,
            password: password.ok_or("URL has no password=… parameter")?,
        })
    }

    /// The URL with the password replaced, for display.
    pub fn masked(raw: &str) -> String {
        match Url::parse(raw.trim()) {
            Ok(mut u) => {
                let pairs: Vec<(String, String)> = u
                    .query_pairs()
                    .map(|(k, v)| {
                        let v = if k == "password" {
                            "*****".to_string()
                        } else {
                            v.into_owned()
                        };
                        (k.into_owned(), v)
                    })
                    .collect();
                u.query_pairs_mut().clear().extend_pairs(pairs);
                u.to_string()
            }
            Err(_) => "(invalid URL)".into(),
        }
    }
}

/// Xtream `player_api.php` → `user_info.auth == 1`.
pub async fn test_provider(client: &Client, raw_url: &str) -> Result<String, String> {
    let p = ProviderUrl::parse(raw_url)?;
    let mut api = Url::parse(&format!("{}/player_api.php", p.base)).map_err(|e| e.to_string())?;
    api.query_pairs_mut()
        .append_pair("username", &p.username)
        .append_pair("password", &p.password)
        .append_pair("action", "get_user_info");
    let resp = client
        .get(api)
        .header(reqwest::header::USER_AGENT, provider_user_agent())
        .send()
        .await
        .map_err(|e| format!("Cannot reach {}: {}", p.base, root_cause(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{} answered HTTP {status}", p.base));
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

/// TMDB `GET /3/configuration`. Accepts a v3 API key or a v4 read access token.
pub async fn test_tmdb(client: &Client, key: &str) -> Result<String, String> {
    let key = key.trim();
    let mut url = Url::parse("https://api.themoviedb.org/3/configuration").expect("static URL");
    let req = if key.starts_with("eyJ") {
        client.get(url).bearer_auth(key) // v4 read access token (a JWT)
    } else {
        url.query_pairs_mut().append_pair("api_key", key);
        client.get(url)
    };
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Cannot reach TMDB: {}", root_cause(&e)))?;
    match resp.status().as_u16() {
        200 => Ok("TMDB key accepted".into()),
        401 => Err("TMDB refused the key".into()),
        s => Err(format!("TMDB answered HTTP {s}")),
    }
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

fn as_text(v: &Value) -> String {
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

fn unix_to_date(ts: i64) -> String {
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
    fn parses_provider_url() {
        let p = ProviderUrl::parse(
            "http://iptvproxy.local:8080/get.php?username=u&password=p%26q&type=m3u_plus",
        )
        .unwrap();
        assert_eq!(p.base, "http://iptvproxy.local:8080");
        assert_eq!(p.username, "u");
        assert_eq!(p.password, "p&q");
        assert!(ProviderUrl::parse("http://x/get.php?username=u").is_err());
        assert!(ProviderUrl::parse("ftp://x/?username=u&password=p").is_err());
    }

    #[test]
    fn masks_password() {
        let m = ProviderUrl::masked("http://h:8080/get.php?username=u&password=secret&type=m3u");
        assert!(!m.contains("secret"));
        assert!(m.contains("username=u"));
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
}
