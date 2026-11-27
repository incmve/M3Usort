//! Log output: every line goes to stdout (docker logs) and, once `enable` is
//! called by `serve`, to a daily file in <data>/logs (kept LOG_DAYS days) that
//! the admin log viewer reads. Credentials in URLs are masked before anything is
//! written: `username=`/`password=` query values and the user/pass segments of
//! Xtream stream paths (/movie/u/p/1.mkv).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

pub const LOG_DAYS: i64 = 14;
const PREFIX: &str = "m3usort-";
const SUFFIX: &str = ".log";

static DIR: OnceLock<PathBuf> = OnceLock::new();
static FILE: Mutex<Option<(String, File)>> = Mutex::new(None);

/// Start writing daily log files to `dir` and remove old ones.
pub fn enable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let _ = DIR.set(dir.to_path_buf());
    cleanup(dir);
    Ok(())
}

pub fn dir() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

pub fn file_for(date: &str) -> Option<PathBuf> {
    let valid = date.len() == 10 && date.chars().all(|c| c.is_ascii_digit() || c == '-');
    if !valid {
        return None;
    }
    Some(dir()?.join(format!("{PREFIX}{date}{SUFFIX}")))
}

/// Dates with a log file, newest first.
pub fn dates() -> Vec<String> {
    let Some(d) = dir() else { return Vec::new() };
    let mut out: Vec<String> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_prefix(PREFIX)?
                .strip_suffix(SUFFIX)
                .map(str::to_string)
        })
        .filter(|d| d.len() == 10)
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

fn cleanup(dir: &Path) {
    let today = crate::localtime::today();
    let Ok(today) = today.parse::<jiff::civil::Date>() else {
        return;
    };
    for d in dates() {
        if let Ok(day) = d.parse::<jiff::civil::Date>()
            && (today - day).get_days() as i64 >= LOG_DAYS
        {
            let _ = std::fs::remove_file(dir.join(format!("{PREFIX}{d}{SUFFIX}")));
        }
    }
}

/// Mask credentials in a log line.
pub fn redact(line: &str) -> String {
    let mut s = line.to_string();
    for key in ["password=", "username="] {
        let mut from = 0;
        while let Some(pos) = s[from..].find(key) {
            let start = from + pos + key.len();
            let end = s[start..]
                .find(|c: char| {
                    c == '&' || c == ' ' || c == ')' || c == '"' || c == '\'' || c == '\n'
                })
                .map_or(s.len(), |e| start + e);
            if end > start {
                s.replace_range(start..end, "***");
            }
            from = start + 3;
        }
    }
    for seg in ["/movie/", "/series/", "/live/", "/timeshift/"] {
        let mut from = 0;
        while let Some(pos) = s[from..].find(seg) {
            let start = from + pos + seg.len();
            // user/pass/ follow: mask the next two path segments when a third follows.
            let rest = &s[start..];
            let parts: Vec<&str> = rest.splitn(3, '/').collect();
            if parts.len() == 3
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && !parts[0].contains(' ')
                && !parts[1].contains(' ')
            {
                let len = parts[0].len() + 1 + parts[1].len();
                s.replace_range(start..start + len, "***/***");
                from = start + 7;
            } else {
                from = start;
            }
        }
    }
    s
}

pub struct Tee;

pub struct TeeWriter {
    buf: Vec<u8>,
}

impl<'a> MakeWriter<'a> for Tee {
    type Writer = TeeWriter;
    fn make_writer(&'a self) -> TeeWriter {
        TeeWriter {
            buf: Vec::with_capacity(256),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TeeWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let line = redact(&String::from_utf8_lossy(&self.buf));
        let _ = std::io::stdout().write_all(line.as_bytes());
        let Some(dir) = dir() else { return };
        let today = crate::localtime::today();
        let mut guard = FILE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.as_ref().is_none_or(|(d, _)| *d != today) {
            let path = dir.join(format!("{PREFIX}{today}{SUFFIX}"));
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(|f| (today.clone(), f));
            cleanup(dir);
        }
        if let Some((_, f)) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// One parsed log line for the viewer.
pub struct Line {
    pub time: String,
    pub level: String,
    pub text: String,
}

fn level_rank(l: &str) -> u8 {
    match l {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    }
}

/// Lines of one day, newest first, filtered by minimum level and search text.
/// Lines without a timestamp (continuations) stay with the line above.
pub fn read(date: &str, min_level: &str, search: &str) -> std::io::Result<Vec<Line>> {
    let Some(path) = file_for(date) else {
        return Ok(Vec::new());
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let min = level_rank(min_level);
    let needle = search.to_lowercase();
    let mut lines: Vec<Line> = Vec::new();
    for l in raw.lines() {
        let starts_with_time = l.len() > 19 && l.as_bytes()[4] == b'-' && l.as_bytes()[13] == b':';
        if !starts_with_time {
            if let Some(last) = lines.last_mut() {
                last.text.push('\n');
                last.text.push_str(l);
            }
            continue;
        }
        let rest = l[19..].trim_start();
        let (level, text) = rest.split_once(' ').unwrap_or((rest, ""));
        lines.push(Line {
            time: l[11..19].to_string(),
            level: level.to_string(),
            text: text.trim_start().to_string(),
        });
    }
    Ok(lines
        .into_iter()
        .rev()
        .filter(|l| level_rank(&l.level) >= min)
        .filter(|l| needle.is_empty() || l.text.to_lowercase().contains(&needle))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials() {
        assert_eq!(
            redact(
                "error sending request for url (http://h:8080/player_api.php?username=me&password=s3cr%26t&action=get_vod_info)"
            ),
            "error sending request for url (http://h:8080/player_api.php?username=***&password=***&action=get_vod_info)"
        );
        assert_eq!(
            redact("GET http://h/movie/me/pw/123.mkv failed"),
            "GET http://h/movie/***/***/123.mkv failed"
        );
        assert_eq!(
            redact("/series/me/pw/9.mkv and /live/a/b/1.ts"),
            "/series/***/***/9.mkv and /live/***/***/1.ts"
        );
        assert_eq!(
            redact("Added 'Movies/series/x' folder"),
            "Added 'Movies/series/x' folder"
        );
        assert_eq!(redact("nothing here"), "nothing here");
    }
}
