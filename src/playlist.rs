//! Live TV playlist: download the provider's M3U, let the user pick and order
//! groups and a custom group, and serve the sorted result to IPTV players.
//!
//! Same output as the Python version's rebuild(): `#EXTM3U`, then the custom group
//! (its channels get group-title="<custom title>"), then the chosen groups in order.
//! A channel name appears once. One fix on purpose: the custom group follows the
//! order you set (Python saved the order but wrote the provider's order).

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::catalog::{self, now_secs};
use crate::integrations;
use crate::settings as s;
use crate::web::AppState;

/// Don't replace the stored M3U if the new download has less than this share of
/// the entries we had: a half-broken download must not empty the playlist.
const MIN_SHARE_TO_REPLACE: f64 = 0.5;
const BATCH: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub group: String,
    pub extinf: String,
    pub url: String,
    pub kind: &'static str,
}

/// Value of an attribute in an #EXTINF line, e.g. attr(line, "group-title").
pub fn attr<'a>(extinf: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=\"");
    let start = extinf.find(&pat)? + pat.len();
    let end = extinf[start..].find('"')? + start;
    Some(&extinf[start..end])
}

/// Replace (or add) group-title="…" in an #EXTINF line.
pub fn set_group(extinf: &str, group: &str) -> String {
    let group = group.replace('"', "'");
    match extinf.find("group-title=\"") {
        Some(i) => {
            let start = i + "group-title=\"".len();
            match extinf[start..].find('"') {
                Some(len) => format!("{}{}{}", &extinf[..start], group, &extinf[start + len..]),
                None => extinf.to_string(),
            }
        }
        // No group-title yet: add it before the comma that starts the name.
        None => match extinf.rfind(',') {
            Some(c) => format!("{} group-title=\"{group}\"{}", &extinf[..c], &extinf[c..]),
            None => extinf.to_string(),
        },
    }
}

/// Parse an M3U line by line (like Python's rebuild): #EXTINF followed by a URL.
/// Returns the header line (#EXTM3U with any attributes such as url-tvg) and the entries.
pub fn parse(text: &str) -> (String, Vec<Entry>) {
    let mut header = "#EXTM3U".to_string();
    let mut out = Vec::new();
    let mut pending: Option<&str> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if i == 0 && line.starts_with("#EXTM3U") {
            header = line.trim().to_string();
            continue;
        }
        if line.starts_with("#EXTINF") {
            pending = Some(line);
        } else if line.starts_with('#') || line.trim().is_empty() {
            // other directives (#EXTGRP, #EXTVLCOPT) are skipped, as in Python
        } else if let Some(extinf) = pending.take() {
            let url = line.trim().to_string();
            let name = extinf
                .rsplit_once(',')
                .map(|(_, n)| n.trim().to_string())
                .unwrap_or_default();
            let kind = if url.contains("/movie/") {
                "movie"
            } else if url.contains("/series/") {
                "series"
            } else {
                "live"
            };
            out.push(Entry {
                name,
                group: attr(extinf, "group-title").unwrap_or("").to_string(),
                extinf: extinf.to_string(),
                url,
                kind,
            });
        }
    }
    (header, out)
}

/// Download get.php and replace the stored entries (in one transaction).
pub async fn refresh(st: &AppState) -> anyhow::Result<String> {
    if st.playlist_running.swap(true, Ordering::SeqCst) {
        anyhow::bail!("a playlist download is already running");
    }
    let started = now_secs();
    let run_id: i64 = sqlx::query_scalar("INSERT INTO m3u_runs (started) VALUES (?) RETURNING id")
        .bind(started)
        .fetch_one(&st.db)
        .await?;
    let result = refresh_inner(st).await;
    st.playlist_running.store(false, Ordering::SeqCst);
    let (ok, entries, msg) = match &result {
        Ok((n, m)) => (true, Some(*n as i64), m.clone()),
        Err(e) => (false, None, format!("{e:#}")),
    };
    sqlx::query("UPDATE m3u_runs SET finished = ?, ok = ?, entries = ?, message = ? WHERE id = ?")
        .bind(now_secs())
        .bind(i64::from(ok))
        .bind(entries)
        .bind(&msg)
        .bind(run_id)
        .execute(&st.db)
        .await?;
    sqlx::query("DELETE FROM m3u_runs WHERE id <= (SELECT MAX(id) - 100 FROM m3u_runs)")
        .execute(&st.db)
        .await?;
    if ok {
        tracing::info!("playlist downloaded in {}s: {msg}", now_secs() - started);
        Ok(msg)
    } else {
        tracing::error!("playlist download failed: {msg}");
        anyhow::bail!(msg)
    }
}

async fn refresh_inner(st: &AppState) -> anyhow::Result<(usize, String)> {
    let x = catalog::load_xtream(st)
        .await?
        .ok_or_else(|| anyhow::anyhow!("provider is not configured"))?;
    let url = x.m3u_url().map_err(anyhow::Error::msg)?;
    let resp = st
        .http
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            integrations::provider_user_agent(),
        )
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach provider: {e}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "get.php answered HTTP {}",
        resp.status()
    );
    let text = resp.text().await?;
    anyhow::ensure!(
        text.trim_start().starts_with("#EXTM3U"),
        "answer is not an M3U playlist"
    );
    let (header, entries) = parse(&text);
    anyhow::ensure!(
        !entries.is_empty(),
        "the M3U has no channels; playlist left unchanged"
    );

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM m3u_entries")
        .fetch_one(&st.db)
        .await?;
    if before > 0 && (entries.len() as f64) < before as f64 * MIN_SHARE_TO_REPLACE {
        anyhow::bail!(
            "the provider's M3U has only {} entries (had {before}); kept the previous one in case the download is incomplete",
            entries.len()
        );
    }

    let mut tx = st.db.begin().await?;
    sqlx::query("DELETE FROM m3u_entries")
        .execute(&mut *tx)
        .await?;
    for (chunk_no, chunk) in entries.chunks(BATCH).enumerate() {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "INSERT INTO m3u_entries (pos, name, group_title, extinf, url, kind) ",
        );
        qb.push_values(chunk.iter().enumerate(), |mut b, (i, e)| {
            b.push_bind((chunk_no * BATCH + i) as i64)
                .push_bind(e.name.clone())
                .push_bind(e.group.clone())
                .push_bind(e.extinf.clone())
                .push_bind(e.url.clone())
                .push_bind(e.kind);
        });
        qb.build().execute(&mut *tx).await?;
    }
    s::set(&mut *tx, s::M3U_HEADER, &header).await?;
    tx.commit().await?;

    let live = entries.iter().filter(|e| e.kind == "live").count();
    let groups = entries
        .iter()
        .map(|e| e.group.as_str())
        .collect::<HashSet<_>>()
        .len();
    Ok((
        entries.len(),
        format!("{} entries ({live} live) in {groups} groups", entries.len()),
    ))
}

/// Download when the last success is older than the refresh interval (15 min back-off on failure).
pub async fn maybe_refresh(st: &AppState) -> anyhow::Result<()> {
    if !st.is_setup_done() || st.playlist_running.load(Ordering::SeqCst) {
        return Ok(());
    }
    let hours: i64 = s::get(&st.db, s::PLAYLIST_REFRESH_HOURS)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let last_ok: Option<i64> = sqlx::query_scalar("SELECT MAX(started) FROM m3u_runs WHERE ok = 1")
        .fetch_one(&st.db)
        .await?;
    let last_any: Option<i64> = sqlx::query_scalar("SELECT MAX(started) FROM m3u_runs")
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

// ------------------------------------------------------------------ building the output

/// The sorted playlist, built from the stored entries and the user's choices.
pub async fn build(db: &SqlitePool) -> anyhow::Result<(String, usize)> {
    let header = s::get(db, s::M3U_HEADER)
        .await?
        .unwrap_or_else(|| "#EXTM3U".into());
    let custom_title = s::get(db, s::CUSTOM_GROUP_TITLE)
        .await?
        .unwrap_or_else(|| "Custom".into());
    let mut out = String::with_capacity(1 << 20);
    out.push_str(&header);
    out.push('\n');
    let mut seen: HashSet<String> = HashSet::new();
    let mut count = 0;

    // 1. Custom group, in the order the user set. First occurrence of each name.
    let custom: Vec<(String, String)> = sqlx::query_as(
        "SELECT e.extinf, e.url FROM playlist_custom c
         JOIN m3u_entries e ON e.pos = (SELECT MIN(pos) FROM m3u_entries WHERE name = c.name)
         ORDER BY c.position",
    )
    .fetch_all(db)
    .await?;
    for (extinf, url) in custom {
        let name = extinf
            .rsplit_once(',')
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        if seen.insert(name) {
            out.push_str(&set_group(&extinf, &custom_title));
            out.push('\n');
            out.push_str(&url);
            out.push('\n');
            count += 1;
        }
    }

    // 2. Chosen groups in the user's order; channels in the provider's order.
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT e.name, e.extinf, e.url FROM playlist_groups g
         JOIN m3u_entries e ON e.group_title = g.title
         ORDER BY g.position, e.pos",
    )
    .fetch_all(db)
    .await?;
    for (name, extinf, url) in rows {
        if seen.insert(name) {
            out.push_str(&extinf);
            out.push('\n');
            out.push_str(&url);
            out.push('\n');
            count += 1;
        }
    }
    Ok((out, count))
}

// ------------------------------------------------------------------ queries for the pages

pub struct Group {
    pub title: String,
    pub count: i64,
    pub kind: String,
    pub position: Option<i64>,
}

/// All groups with channel count and main kind; chosen ones carry their position.
pub async fn groups(db: &SqlitePool) -> anyhow::Result<Vec<Group>> {
    let rows: Vec<(String, i64, String, Option<i64>)> = sqlx::query_as(
        "SELECT e.group_title, COUNT(*),
                CASE WHEN SUM(e.kind = 'live') * 2 >= COUNT(*) THEN 'live'
                     WHEN SUM(e.kind = 'movie') >= SUM(e.kind = 'series') THEN 'movies' ELSE 'series' END,
                g.position
         FROM m3u_entries e LEFT JOIN playlist_groups g ON g.title = e.group_title
         GROUP BY e.group_title
         ORDER BY e.group_title COLLATE NOCASE",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(title, count, kind, position)| Group {
            title,
            count,
            kind,
            position,
        })
        .collect())
}

/// Chosen groups in order, also those the provider no longer has (count 0).
pub async fn chosen_groups(db: &SqlitePool) -> anyhow::Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT g.title, (SELECT COUNT(*) FROM m3u_entries e WHERE e.group_title = g.title)
         FROM playlist_groups g ORDER BY g.position",
    )
    .fetch_all(db)
    .await?)
}

/// Custom group channels in order, with the group they come from ('' = gone at provider).
pub async fn custom_channels(db: &SqlitePool) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT c.name, COALESCE((SELECT group_title FROM m3u_entries e WHERE e.name = c.name ORDER BY pos LIMIT 1), '')
         FROM playlist_custom c ORDER BY c.position",
    )
    .fetch_all(db)
    .await?)
}

/// Live channel names matching a search (for adding single channels to the custom group).
pub async fn search_channels(db: &SqlitePool, q: &str) -> anyhow::Result<Vec<(String, String)>> {
    let like = format!(
        "%{}%",
        q.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    Ok(sqlx::query_as(
        "SELECT name, MIN(group_title) FROM m3u_entries
         WHERE kind = 'live' AND name LIKE ? ESCAPE '\\' COLLATE NOCASE
         GROUP BY name ORDER BY MIN(pos) LIMIT 100",
    )
    .bind(like)
    .fetch_all(db)
    .await?)
}

/// Replace the chosen groups with this ordered list.
pub async fn save_groups(db: &SqlitePool, titles: &[String]) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM playlist_groups")
        .execute(&mut *tx)
        .await?;
    let mut seen = HashSet::new();
    for (i, t) in titles
        .iter()
        .filter(|t| seen.insert(t.as_str()))
        .enumerate()
    {
        sqlx::query("INSERT INTO playlist_groups (title, position) VALUES (?, ?)")
            .bind(t)
            .bind(i as i64)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Replace the custom group with this ordered list of channel names.
pub async fn save_custom(db: &SqlitePool, names: &[String]) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM playlist_custom")
        .execute(&mut *tx)
        .await?;
    let mut seen = HashSet::new();
    for (i, n) in names
        .iter()
        .filter(|n| !n.trim().is_empty() && seen.insert(n.as_str()))
        .enumerate()
    {
        sqlx::query("INSERT INTO playlist_custom (name, position) VALUES (?, ?)")
            .bind(n)
            .bind(i as i64)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Append all live channels of a group to the custom group (Python's "Add Channel Groups").
pub async fn add_group_to_custom(db: &SqlitePool, group: &str) -> anyhow::Result<usize> {
    let mut names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM playlist_custom ORDER BY position")
            .fetch_all(db)
            .await?;
    let new: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM m3u_entries WHERE group_title = ? GROUP BY name ORDER BY MIN(pos)",
    )
    .bind(group)
    .fetch_all(db)
    .await?;
    let have: HashSet<String> = names.iter().cloned().collect();
    let added: Vec<String> = new.into_iter().filter(|n| !have.contains(n)).collect();
    let n = added.len();
    names.extend(added);
    save_custom(db, &names).await?;
    Ok(n)
}

pub struct Status {
    pub live: i64,
    pub chosen: i64,
    pub custom: i64,
    pub last_ok: String,
    pub last_message: String,
    pub last_failed: bool,
}

pub async fn status(db: &SqlitePool) -> anyhow::Result<Status> {
    let one =
        |sql: &'static str| async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(db).await };
    let last: Option<(i64, Option<String>)> = sqlx::query_as(
        "SELECT ok, message FROM m3u_runs WHERE finished IS NOT NULL ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(db)
    .await?;
    let last_ok: Option<i64> =
        sqlx::query_scalar("SELECT MAX(finished) FROM m3u_runs WHERE ok = 1")
            .fetch_one(db)
            .await?;
    Ok(Status {
        live: one("SELECT COUNT(*) FROM m3u_entries WHERE kind = 'live'").await?,
        chosen: one("SELECT COUNT(*) FROM playlist_groups").await?,
        custom: one("SELECT COUNT(*) FROM playlist_custom").await?,
        last_ok: last_ok.map(catalog::ago).unwrap_or_else(|| "never".into()),
        last_message: last.as_ref().and_then(|l| l.1.clone()).unwrap_or_default(),
        last_failed: last.is_some_and(|l| l.0 == 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#EXTM3U url-tvg=\"http://epg/x.xml\"\r\n\
#EXTINF:-1 tvg-id=\"NPO1.nl\" tvg-name=\"NL: NPO 1\" tvg-logo=\"\" group-title=\"NL KANALEN\",NL: NPO 1\r\n\
http://p/live/u/p/1.ts\r\n\
#EXTINF:-1 tvg-name=\"NL: RTL 4\" group-title=\"NL KANALEN\",NL: RTL 4\n\
#EXTVLCOPT:http-user-agent=x\n\
http://p/live/u/p/2.ts\n\
#EXTINF:-1 tvg-name=\"Sport 1\" group-title=\"SPORT\",Sport 1\n\
http://p/live/u/p/3.ts\n\
#EXTINF:-1 group-title=\"SPORT\",NL: NPO 1\n\
http://p/live/u/p/9.ts\n\
#EXTINF:-1 group-title=\"VOD NL\",Some Movie (2020)\n\
http://p/movie/u/p/77.mkv\n\
#EXTINF:-1,No Group\n\
http://p/live/u/p/5.ts\n";

    #[test]
    fn parses_like_python() {
        let (header, e) = parse(SAMPLE);
        assert_eq!(header, "#EXTM3U url-tvg=\"http://epg/x.xml\"");
        assert_eq!(e.len(), 6);
        assert_eq!(
            (e[0].name.as_str(), e[0].group.as_str()),
            ("NL: NPO 1", "NL KANALEN")
        );
        assert_eq!(
            e[1].url, "http://p/live/u/p/2.ts",
            "#EXTVLCOPT between EXTINF and URL is skipped"
        );
        assert_eq!(e[4].kind, "movie");
        assert_eq!(e[5].group, "");
    }

    #[test]
    fn group_rewrite() {
        assert_eq!(
            set_group("#EXTINF:-1 group-title=\"A\",X", "Mine"),
            "#EXTINF:-1 group-title=\"Mine\",X"
        );
        assert_eq!(
            set_group("#EXTINF:-1 tvg-id=\"1\",X", "Mine"),
            "#EXTINF:-1 tvg-id=\"1\" group-title=\"Mine\",X"
        );
        assert_eq!(
            attr(
                "#EXTINF:-1 tvg-name=\"a,b\" group-title=\"G\",a,b",
                "group-title"
            ),
            Some("G")
        );
    }

    #[tokio::test]
    async fn build_order_and_dedup() {
        let dir = std::env::temp_dir().join(format!("m3usort-pl-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        let (header, entries) = parse(SAMPLE);
        for (i, e) in entries.iter().enumerate() {
            sqlx::query("INSERT INTO m3u_entries VALUES (?, ?, ?, ?, ?, ?)")
                .bind(i as i64)
                .bind(&e.name)
                .bind(&e.group)
                .bind(&e.extinf)
                .bind(&e.url)
                .bind(e.kind)
                .execute(&db)
                .await
                .unwrap();
        }
        s::set(&db, s::M3U_HEADER, &header).await.unwrap();
        s::set(&db, s::CUSTOM_GROUP_TITLE, "NL Custom")
            .await
            .unwrap();
        // Custom order deliberately differs from the provider's order.
        save_custom(
            &db,
            &[
                "NL: RTL 4".into(),
                "NL: NPO 1".into(),
                "Gone Channel".into(),
            ],
        )
        .await
        .unwrap();
        save_groups(&db, &["SPORT".into(), "NL KANALEN".into()])
            .await
            .unwrap();
        let (m3u, n) = build(&db).await.unwrap();
        let names: Vec<&str> = m3u
            .lines()
            .filter(|l| l.starts_with("#EXTINF"))
            .map(|l| l.rsplit_once(',').unwrap().1)
            .collect();
        // RTL 4 and NPO 1 first (custom order), then SPORT (Sport 1; its NPO 1 is a dup), NL KANALEN adds nothing new.
        assert_eq!(names, vec!["NL: RTL 4", "NL: NPO 1", "Sport 1"]);
        assert_eq!(n, 3);
        assert!(m3u.starts_with("#EXTM3U url-tvg="));
        assert!(m3u.contains("group-title=\"NL Custom\",NL: RTL 4\nhttp://p/live/u/p/2.ts"));
        assert!(m3u.contains("group-title=\"NL Custom\",NL: NPO 1\nhttp://p/live/u/p/1.ts"));

        assert_eq!(
            add_group_to_custom(&db, "SPORT").await.unwrap(),
            1,
            "only Sport 1 is new"
        );
        assert_eq!(
            custom_channels(&db).await.unwrap().last().unwrap().0,
            "Sport 1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
