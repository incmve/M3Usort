//! TMDB client. Same API key handling as the setup test (v3 key as `api_key`, v4
//! token as Bearer) and the same language as the Python version (en-US).
//! One details call with `append_to_response=credits,external_ids` replaces the
//! Python version's separate credits / poster / tv-detail calls.

use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use serde_json::Value;

use crate::integrations::as_text;

pub const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/w500";

#[derive(Debug, Default)]
pub struct Details {
    pub poster: Option<String>,
    pub plot: Option<String>,
    pub rating: Option<f64>,
    pub imdb_id: Option<String>,
    pub seasons: Option<i64>,
    pub status: Option<String>,
    pub cast: Vec<Person>,
}

#[derive(Debug, Clone)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub character: Option<String>,
}

/// `movie` or `tv` path segment.
pub fn media(is_movie: bool) -> &'static str {
    if is_movie { "movie" } else { "tv" }
}

async fn get(
    client: &Client,
    base: &str,
    key: &str,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Option<Value>, String> {
    let mut url = Url::parse(&format!("{base}/3/{path}")).map_err(|e| e.to_string())?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
        if !key.starts_with("eyJ") {
            q.append_pair("api_key", key);
        }
    }
    // One retry after a 429, honouring Retry-After (as the Python cast job did).
    for attempt in 0..2 {
        let mut req = client.get(url.clone()).timeout(Duration::from_secs(10));
        if key.starts_with("eyJ") {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("cannot reach TMDB: {e}"))?;
        match resp.status() {
            StatusCode::OK => {
                return resp
                    .json()
                    .await
                    .map(Some)
                    .map_err(|e| format!("TMDB answer: {e}"));
            }
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::TOO_MANY_REQUESTS if attempt == 0 => {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5)
                    .min(60);
                tracing::warn!("TMDB rate limit; waiting {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            StatusCode::UNAUTHORIZED => return Err("TMDB refused the API key".into()),
            s => return Err(format!("TMDB answered HTTP {s}")),
        }
    }
    Err("TMDB rate limit".into())
}

/// Search by title (+ year when known). Returns the first hit's id and poster.
pub async fn search(
    client: &Client,
    base: &str,
    key: &str,
    is_movie: bool,
    title: &str,
    year: Option<i32>,
) -> Result<Option<(i64, Option<String>)>, String> {
    let year_s = year.map(|y| y.to_string()).unwrap_or_default();
    let year_param = if is_movie {
        "year"
    } else {
        "first_air_date_year"
    };
    let mut params = vec![("query", title), ("language", "en-US")];
    if !year_s.is_empty() {
        params.push((year_param, year_s.as_str()));
    }
    let path = format!("search/{}", media(is_movie));
    let mut body = get(client, base, key, &path, &params).await?;
    // Retry without the year: providers' years are sometimes off by one.
    if year.is_some() && first_result(&body).is_none() {
        body = get(client, base, key, &path, &params[..2]).await?;
    }
    Ok(first_result(&body).map(|r| {
        let id = r.get("id").and_then(Value::as_i64).unwrap_or(0);
        (id, poster_url(r))
    }))
    .map(|o| o.filter(|(id, _)| *id > 0))
}

fn first_result(body: &Option<Value>) -> Option<&Value> {
    body.as_ref()?.get("results")?.as_array()?.first()
}

fn poster_url(v: &Value) -> Option<String> {
    v.get("poster_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(|p| format!("{IMAGE_BASE}{p}"))
}

/// Poster, plot, rating, IMDb id, seasons/status (tv) and top-10 cast in one call.
/// Ok(None) when TMDB doesn't know the id.
pub async fn details(
    client: &Client,
    base: &str,
    key: &str,
    is_movie: bool,
    tmdb_id: &str,
) -> Result<Option<Details>, String> {
    let id: i64 = tmdb_id
        .trim()
        .parse()
        .map_err(|_| format!("'{tmdb_id}' is not a TMDB id"))?;
    let path = format!("{}/{id}", media(is_movie));
    let Some(v) = get(
        client,
        base,
        key,
        &path,
        &[
            ("language", "en-US"),
            ("append_to_response", "credits,external_ids"),
        ],
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(parse_details(&v)))
}

pub fn parse_details(v: &Value) -> Details {
    let text = |k: &str| {
        v.get(k)
            .map(as_text)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let cast = v
        .pointer("/credits/cast")
        .and_then(Value::as_array)
        .map(|a| {
            let mut people: Vec<&Value> = a.iter().collect();
            people.sort_by_key(|p| p.get("order").and_then(Value::as_i64).unwrap_or(i64::MAX));
            people
                .into_iter()
                .filter_map(|p| {
                    Some(Person {
                        id: p.get("id")?.as_i64()?,
                        name: p.get("name")?.as_str()?.to_string(),
                        character: p
                            .get("character")
                            .and_then(Value::as_str)
                            .filter(|c| !c.is_empty())
                            .map(String::from),
                    })
                })
                .take(10)
                .collect()
        })
        .unwrap_or_default();
    Details {
        poster: poster_url(v),
        plot: text("overview"),
        rating: v
            .get("vote_average")
            .and_then(Value::as_f64)
            .filter(|r| *r > 0.0),
        imdb_id: text("imdb_id").or_else(|| {
            v.pointer("/external_ids/imdb_id")
                .map(as_text)
                .filter(|s| !s.is_empty())
        }),
        seasons: v.get("number_of_seasons").and_then(Value::as_i64),
        status: text("status"),
        cast,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tv_details() {
        let v = serde_json::json!({
            "poster_path": "/abc.jpg", "overview": "A show.", "vote_average": 8.4, "number_of_seasons": 3,
            "status": "Returning Series", "external_ids": {"imdb_id": "tt123"},
            "credits": {"cast": [
                {"id": 2, "name": "Second", "order": 1, "character": "B"},
                {"id": 1, "name": "First", "order": 0, "character": ""}
            ]}
        });
        let d = parse_details(&v);
        assert_eq!(
            d.poster.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/abc.jpg")
        );
        assert_eq!(
            (d.seasons, d.status.as_deref(), d.imdb_id.as_deref()),
            (Some(3), Some("Returning Series"), Some("tt123"))
        );
        assert_eq!(
            d.cast.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["First", "Second"]
        );
        assert_eq!(d.cast[0].character, None);
    }
}
