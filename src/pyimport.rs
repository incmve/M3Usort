//! One-time import from the Python M3Usort's config.py: playlist groups
//! (`desired_group_titles`), custom channels (`target_channel_names`) and the
//! wanted lists (`wanted_movies`, `wanted_series`). Nothing else.
//!
//! The file is read as text, never executed: only `name = [ "…", '…', ]` list
//! literals of strings are understood (comments, both quote styles and the
//! usual escapes, as Python writes and people edit them).

use std::collections::HashSet;

use serde::Serialize;
use sqlx::SqlitePool;

pub const GROUPS: &str = "desired_group_titles";
pub const CUSTOM: &str = "target_channel_names";
pub const WANTED_MOVIES: &str = "wanted_movies";
pub const WANTED_SERIES: &str = "wanted_series";

/// The string items of `name = [...]` at the start of a line, or None when the
/// list isn't in the file. Non-string items are skipped (counted in `skipped`).
pub fn list(src: &str, name: &str) -> Option<(Vec<String>, usize)> {
    let mut at = None;
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        // `name = [` at the start of a line; the last one wins, as in Python.
        let value = line
            .trim_start()
            .strip_prefix(name)
            .and_then(|r| r.trim_start().strip_prefix('='))
            .map(str::trim_start)
            .filter(|r| r.starts_with('['));
        if let Some(v) = value {
            at = Some(offset + line.len() - v.len());
        }
        offset += line.len();
    }
    let start = at?;
    let b = src.as_bytes();
    debug_assert_eq!(b[start], b'[');
    let mut i = start + 1;
    let mut items = Vec::new();
    let mut skipped = 0;
    let mut other = false; // saw a non-string token since the last comma
    while i < b.len() {
        match b[i] {
            b']' => {
                if other {
                    skipped += 1;
                }
                return Some((items, skipped));
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b',' => {
                if other {
                    skipped += 1;
                    other = false;
                }
                i += 1;
            }
            b'"' | b'\'' => {
                let (s, next) = string(src, i)?;
                items.push(s);
                i = next;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                other = true;
                i += 1;
            }
        }
    }
    None // no closing bracket
}

/// Parse a Python string literal starting at `i` (a quote). Returns the value
/// and the index after the closing quote.
fn string(src: &str, i: usize) -> Option<(String, usize)> {
    let b = src.as_bytes();
    let q = b[i];
    let triple = b.len() >= i + 3 && b[i + 1] == q && b[i + 2] == q;
    let j = if triple { i + 3 } else { i + 1 };
    let mut out = String::new();
    let chars: Vec<(usize, char)> = src[j..].char_indices().map(|(k, c)| (k + j, c)).collect();
    let mut k = 0;
    while k < chars.len() {
        let (pos, c) = chars[k];
        if c == q as char {
            if !triple {
                return Some((out, pos + 1));
            }
            if b.len() >= pos + 3 && b[pos + 1] == q && b[pos + 2] == q {
                return Some((out, pos + 3));
            }
            out.push(c);
            k += 1;
            continue;
        }
        if c == '\n' && !triple {
            return None;
        }
        if c == '\\' && k + 1 < chars.len() {
            let n = chars[k + 1].1;
            k += 2;
            match n {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                '\n' => {}
                'x' | 'u' => {
                    let len = if n == 'x' { 2 } else { 4 };
                    let hex: String = chars[k..].iter().take(len).map(|(_, c)| *c).collect();
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) if hex.len() == len => {
                            out.push(ch);
                            k += len;
                        }
                        _ => {
                            out.push('\\');
                            out.push(n);
                        }
                    }
                }
                other => {
                    // Python keeps unknown escapes as written.
                    out.push('\\');
                    out.push(other);
                }
            }
            continue;
        }
        out.push(c);
        k += 1;
    }
    None
}

fn clean(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

#[derive(Default, Serialize)]
pub struct Parsed {
    pub groups: Vec<String>,
    pub custom: Vec<String>,
    pub wanted_movies: Vec<String>,
    pub wanted_series: Vec<String>,
    /// Lists that weren't in the file at all.
    pub missing_lists: Vec<&'static str>,
    pub skipped_items: usize,
}

pub fn parse(src: &str) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let mut found = 0;
    for (name, slot) in [
        (GROUPS, &mut p.groups),
        (CUSTOM, &mut p.custom),
        (WANTED_MOVIES, &mut p.wanted_movies),
        (WANTED_SERIES, &mut p.wanted_series),
    ] {
        match list(src, name) {
            Some((items, skipped)) => {
                found += 1;
                *slot = clean(items);
                p.skipped_items += skipped;
            }
            None => p.missing_lists.push(name),
        }
    }
    if found == 0 {
        return Err(
            "This doesn't look like the Python M3Usort's config.py: none of desired_group_titles, \
                    target_channel_names, wanted_movies or wanted_series was found."
                .into(),
        );
    }
    Ok(p)
}

/// What the import would do, shown before it is applied.
#[derive(Serialize)]
pub struct Preview {
    pub parsed: Parsed,
    /// Groups / channels not in the provider's current playlist (kept anyway).
    pub groups_not_found: Vec<String>,
    pub custom_not_found: Vec<String>,
    pub wanted_new: usize,
    pub wanted_existing: usize,
    pub current_groups: i64,
    pub current_custom: i64,
    pub playlist_loaded: bool,
}

pub async fn preview(db: &SqlitePool, src: &str) -> Result<Preview, String> {
    let parsed = parse(src)?;
    let e = |e: sqlx::Error| e.to_string();
    let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM m3u_entries")
        .fetch_one(db)
        .await
        .map_err(e)?;
    let groups: HashSet<String> =
        sqlx::query_scalar("SELECT DISTINCT group_title FROM m3u_entries")
            .fetch_all(db)
            .await
            .map_err(e)?
            .into_iter()
            .collect();
    let names: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT name FROM m3u_entries")
        .fetch_all(db)
        .await
        .map_err(e)?
        .into_iter()
        .collect();
    let mut wanted_existing = 0;
    for (kind, terms) in [
        ("movie", &parsed.wanted_movies),
        ("series", &parsed.wanted_series),
    ] {
        for t in terms {
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM wanted WHERE kind = ? AND term = ?")
                    .bind(kind)
                    .bind(t)
                    .fetch_one(db)
                    .await
                    .map_err(e)?;
            wanted_existing += n as usize;
        }
    }
    let loaded = entries > 0;
    Ok(Preview {
        groups_not_found: if loaded {
            parsed
                .groups
                .iter()
                .filter(|g| !groups.contains(*g))
                .cloned()
                .collect()
        } else {
            Vec::new()
        },
        custom_not_found: if loaded {
            parsed
                .custom
                .iter()
                .filter(|n| !names.contains(*n))
                .cloned()
                .collect()
        } else {
            Vec::new()
        },
        wanted_new: parsed.wanted_movies.len() + parsed.wanted_series.len() - wanted_existing,
        wanted_existing,
        current_groups: sqlx::query_scalar("SELECT COUNT(*) FROM playlist_groups")
            .fetch_one(db)
            .await
            .map_err(e)?,
        current_custom: sqlx::query_scalar("SELECT COUNT(*) FROM playlist_custom")
            .fetch_one(db)
            .await
            .map_err(e)?,
        playlist_loaded: loaded,
        parsed,
    })
}

/// Apply: groups and custom channels replace the current ones (only when the
/// file has at least one; an empty list never wipes what you have); wanted
/// terms are added, existing ones stay. Returns a summary line.
pub async fn apply(db: &SqlitePool, src: &str) -> Result<String, String> {
    let p = parse(src)?;
    let e = |e: anyhow::Error| e.to_string();
    let mut parts = Vec::new();
    if !p.groups.is_empty() {
        crate::playlist::save_groups(db, &p.groups)
            .await
            .map_err(e)?;
        parts.push(format!("{} group(s)", p.groups.len()));
    }
    if !p.custom.is_empty() {
        crate::playlist::save_custom(db, &p.custom)
            .await
            .map_err(e)?;
        parts.push(format!("{} custom channel(s)", p.custom.len()));
    }
    let now = crate::catalog::now_secs();
    let mut added = 0;
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    for (kind, terms) in [("movie", &p.wanted_movies), ("series", &p.wanted_series)] {
        for t in terms {
            let term: String = t.chars().take(200).collect();
            added += sqlx::query("INSERT INTO wanted (kind, term, created_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(kind)
                .bind(&term)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?
                .rows_affected();
        }
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    if p.wanted_movies.len() + p.wanted_series.len() > 0 {
        parts.push(format!("{added} new wanted term(s)"));
    }
    if parts.is_empty() {
        return Ok("Nothing to import: the lists in this file are empty.".into());
    }
    Ok(format!("Imported {}.", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
debug = "no"
new_group_title = "NL Custom"

# List of channel groups to whitelist
desired_group_titles = [
    "NL | Algemeen",
    "NL | Sport",
    'UK | Entertainment',   # hand-edited, single quotes
    "NL | Sport",
]

# List of specific target channel names, in the desired order
target_channel_names = [
    "NPO 1 HD",
    "RTL 4 HD",
    "Ziggo Sport \"Select\"",
    "Caf\xe9 TV",
]

wanted_series = [
    "Bitterzoet",
    "The Office",
]
wanted_movies = []
some_other = ["x"]
"#;

    #[test]
    fn parses_python_lists() {
        let p = parse(SAMPLE).unwrap();
        assert_eq!(
            p.groups,
            vec!["NL | Algemeen", "NL | Sport", "UK | Entertainment"]
        );
        assert_eq!(
            p.custom,
            vec!["NPO 1 HD", "RTL 4 HD", "Ziggo Sport \"Select\"", "Café TV"]
        );
        assert_eq!(p.wanted_series, vec!["Bitterzoet", "The Office"]);
        assert!(p.wanted_movies.is_empty());
        assert!(p.missing_lists.is_empty());
        assert_eq!(p.skipped_items, 0);
    }

    #[test]
    fn odd_input() {
        assert!(parse("print('hello')").is_err());
        // Missing lists are reported, not invented.
        let p = parse("wanted_movies = [\n  \"Dune\",\n]\n").unwrap();
        assert_eq!(p.wanted_movies, vec!["Dune"]);
        assert_eq!(p.missing_lists, vec![GROUPS, CUSTOM, WANTED_SERIES]);
        // Non-strings are skipped and counted; a comment with a bracket doesn't end the list.
        let (items, skipped) = list(
            "desired_group_titles = [ 'a', 42, # ] not the end\n 'b' ]",
            GROUPS,
        )
        .unwrap();
        assert_eq!(
            (items, skipped),
            (vec!["a".to_string(), "b".to_string()], 1)
        );
        // Unclosed list: nothing.
        assert!(list("desired_group_titles = [ 'a',", GROUPS).is_none());
        // A variable whose name only starts the same doesn't count.
        assert!(list("desired_group_titles_old = ['x']", GROUPS).is_none());
    }

    #[tokio::test]
    async fn applies_without_wiping() {
        let dir = std::env::temp_dir().join(format!("m3usort-pyi-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.db"), true).await.unwrap();
        crate::playlist::save_custom(&db, &["Keep me".to_string()])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO wanted (kind, term, created_at) VALUES ('series', 'Bitterzoet', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        let src = "desired_group_titles = ['G1', 'G2']\ntarget_channel_names = []\nwanted_series = ['Bitterzoet', 'Baantjer']\n";
        let pv = preview(&db, src).await.unwrap();
        assert_eq!((pv.wanted_new, pv.wanted_existing), (1, 1));
        let msg = apply(&db, src).await.unwrap();
        assert_eq!(msg, "Imported 2 group(s), 1 new wanted term(s).");
        let custom: Vec<String> = sqlx::query_scalar("SELECT name FROM playlist_custom")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(
            custom,
            vec!["Keep me"],
            "empty list in the file doesn't wipe"
        );
        let groups: Vec<String> =
            sqlx::query_scalar("SELECT title FROM playlist_groups ORDER BY position")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(groups, vec!["G1", "G2"]);
        std::fs::remove_dir_all(dir).ok();
    }
}
