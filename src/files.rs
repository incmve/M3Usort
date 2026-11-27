//! Admin file browser for the movie and series folders: list, look inside,
//! and delete (one or many). Safety rules:
//! - only inside the configured movie/series folders (no "..", symlinks that
//!   lead outside are refused), never the folder itself;
//! - real video files are never deleted from here, also not inside a folder
//!   (that is Jellyfin's or the NAS's job; Python allowed it);
//! - deleting a title folder also takes it off M3Usort's library list, so the
//!   next VOD run doesn't write it again; Jellyfin is asked to rescan.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::library::is_video_name;
use crate::settings as s;
use crate::web::AppState;

/// Deeper than any movie/series layout; protects against odd trees.
const MAX_DEPTH: usize = 6;

#[derive(Default, Clone, Copy, Serialize)]
pub struct Scan {
    pub files: usize,
    pub dirs: usize,
    pub strm: usize,
    pub video: usize,
    pub bytes: u64,
}

/// Count what a folder holds, recursively (symlinks are counted, not followed).
pub fn scan(dir: &Path) -> std::io::Result<Scan> {
    let mut out = Scan::default();
    walk(dir, 0, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, depth: usize, out: &mut Scan) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        let name = e.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            out.dirs += 1;
            if depth < MAX_DEPTH {
                walk(&e.path(), depth + 1, out)?;
            } else {
                // Too deep to check: treat as unsafe to delete.
                out.video += 1;
            }
        } else {
            out.files += 1;
            out.bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            if name.to_lowercase().ends_with(".strm") {
                out.strm += 1;
            } else if is_video_name(&name) {
                out.video += 1;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Root {
    Movies,
    Series,
}

impl Root {
    pub fn parse(s: &str) -> Option<Root> {
        match s {
            "movies" => Some(Root::Movies),
            "series" => Some(Root::Series),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Root::Movies => "movies",
            Root::Series => "series",
        }
    }
}

pub async fn root_dir(st: &AppState, root: Root) -> Option<PathBuf> {
    let key = if root == Root::Movies {
        s::MOVIES_DIR
    } else {
        s::SERIES_DIR
    };
    let dir = s::get(&st.db, key).await.ok().flatten()?;
    let dir = dir.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// A relative path from the browser: plain names only (no "..", no root).
pub fn clean_rel(rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel.trim_matches('/'));
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Resolve `rel` under `base` and make sure it really is inside (symlinks).
fn inside(base: &Path, rel: &Path) -> Result<PathBuf, String> {
    let base = base
        .canonicalize()
        .map_err(|e| format!("folder {} not available: {e}", base.display()))?;
    let full = base.join(rel);
    // Check the parent with symlinks resolved, then the entry itself without
    // following it (a symlink entry is deleted as a link, never followed).
    let parent = full.parent().unwrap_or(&base);
    let parent = parent.canonicalize().map_err(|_| "not found".to_string())?;
    if !parent.starts_with(&base) {
        return Err("outside the media folder".into());
    }
    let name = full.file_name().ok_or("no name")?;
    Ok(parent.join(name))
}

#[derive(Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: String,
    pub modified: String,
    /// For folders: what's inside. For files: "strm", "video" or "other".
    pub kind: &'static str,
    pub detail: String,
    pub in_library: bool,
    pub deletable: bool,
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn mtime(meta: &std::fs::Metadata) -> String {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| {
            let ts = d.as_secs() as i64;
            format!(
                "{} {}",
                crate::localtime::date(ts),
                crate::localtime::time_of(ts)
            )
        })
        .unwrap_or_default()
}

/// List a folder. `library` holds the folder names of M3Usort's library (top level only).
pub fn list(
    base: &Path,
    rel: &Path,
    library: &std::collections::HashSet<String>,
) -> Result<Vec<Entry>, String> {
    let dir = if rel.as_os_str().is_empty() {
        base.canonicalize()
            .map_err(|e| format!("folder {} not available: {e}", base.display()))?
    } else {
        inside(base, rel)?
    };
    let meta = std::fs::symlink_metadata(&dir).map_err(|_| "not found".to_string())?;
    if !meta.is_dir() {
        return Err("not a folder".into());
    }
    let top = rel.as_os_str().is_empty();
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        let Ok(meta) = e.metadata() else { continue };
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            let sc = scan(&e.path()).unwrap_or(Scan {
                video: 1,
                ..Default::default()
            });
            let kind = match (sc.files + sc.dirs, sc.strm, sc.video) {
                (0, _, _) => "empty",
                (_, s, v) if s > 0 && v > 0 => "mixed",
                (_, _, v) if v > 0 => "video",
                (_, s, _) if s > 0 => "strm",
                _ => "other",
            };
            let mut detail = format!("{} file(s)", sc.files);
            if sc.strm > 0 {
                detail.push_str(&format!(", {} .strm", sc.strm));
            }
            if sc.video > 0 {
                detail.push_str(&format!(", {} video", sc.video));
            }
            out.push(Entry {
                in_library: top && library.contains(&name),
                deletable: sc.video == 0,
                size: human(sc.bytes),
                modified: mtime(&meta),
                kind,
                detail,
                name,
                is_dir: true,
            });
        } else {
            let video = is_video_name(&name);
            out.push(Entry {
                kind: if name.to_lowercase().ends_with(".strm") {
                    "strm"
                } else if video {
                    "video"
                } else {
                    "other"
                },
                deletable: !video,
                size: human(meta.len()),
                modified: mtime(&meta),
                detail: String::new(),
                in_library: false,
                name,
                is_dir: false,
            });
        }
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Delete entries (names inside `rel`). Returns (deleted, messages for refused ones,
/// top-level folder names that were deleted).
pub fn delete(base: &Path, rel: &Path, names: &[String]) -> (usize, Vec<String>, Vec<String>) {
    let mut done = 0;
    let mut refused = Vec::new();
    let mut top_folders = Vec::new();
    for name in names {
        let Some(n) = clean_rel(name).filter(|n| n.components().count() == 1) else {
            refused.push(format!("{name}: invalid name"));
            continue;
        };
        let path = match inside(base, &rel.join(&n)) {
            Ok(p) => p,
            Err(e) => {
                refused.push(format!("{name}: {e}"));
                continue;
            }
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            refused.push(format!("{name}: not found"));
            continue;
        };
        let result = if meta.is_dir() {
            match scan(&path) {
                Ok(sc) if sc.video > 0 => {
                    refused.push(format!(
                        "{name}: holds {} real video file(s); not deleted",
                        sc.video
                    ));
                    continue;
                }
                Ok(_) => std::fs::remove_dir_all(&path),
                Err(e) => Err(e),
            }
        } else if is_video_name(name) {
            refused.push(format!("{name}: real video file; not deleted"));
            continue;
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                done += 1;
                if meta.is_dir() && rel.as_os_str().is_empty() {
                    top_folders.push(name.clone());
                }
            }
            Err(e) => refused.push(format!("{name}: {e}")),
        }
    }
    (done, refused, top_folders)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("m3usort-files-{}", crate::auth::random_code(8)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rel_paths_are_plain() {
        assert_eq!(clean_rel("a/b"), Some(PathBuf::from("a/b")));
        assert_eq!(clean_rel("/a/"), Some(PathBuf::from("a")));
        assert_eq!(clean_rel(""), Some(PathBuf::new()));
        assert_eq!(clean_rel("../etc"), None);
        assert_eq!(clean_rel("a/../../x"), None);
    }

    #[test]
    fn delete_rules() {
        let root = tmp();
        let outside = tmp();
        std::fs::create_dir_all(root.join("Strm Movie")).unwrap();
        std::fs::write(root.join("Strm Movie/Strm Movie.strm"), "u").unwrap();
        std::fs::write(root.join("Strm Movie/poster.jpg"), "x").unwrap();
        std::fs::create_dir_all(root.join("Real Show/Season 1")).unwrap();
        std::fs::write(root.join("Real Show/Season 1/e1.MKV"), "v").unwrap();
        std::fs::write(root.join("Real Show/show.strm"), "u").unwrap();
        std::fs::write(outside.join("keep.txt"), "k").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        // Season folder video is seen (the old check missed it).
        assert_eq!(scan(&root.join("Real Show")).unwrap().video, 1);

        let names: Vec<String> = ["Strm Movie", "Real Show", "../x", "missing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (done, refused, top) = delete(&root, Path::new(""), &names);
        assert_eq!(done, 1);
        assert_eq!(top, vec!["Strm Movie".to_string()]);
        assert_eq!(refused.len(), 3, "{refused:?}");
        assert!(!root.join("Strm Movie").exists());
        assert!(root.join("Real Show/Season 1/e1.MKV").exists());

        // A video file itself is refused; a .strm next to it may go.
        let (done, refused, _) = delete(&root, Path::new("Real Show/Season 1"), &["e1.MKV".into()]);
        assert_eq!((done, refused.len()), (0, 1));
        let (done, _, top) = delete(&root, Path::new("Real Show"), &["show.strm".into()]);
        assert_eq!(done, 1);
        assert!(top.is_empty());

        // Listing through a symlink that leads outside is refused; deleting the
        // link removes the link only.
        #[cfg(unix)]
        {
            let lib = std::collections::HashSet::new();
            assert!(list(&root, Path::new("link"), &lib).is_err());
            let (done, _, _) = delete(&root, Path::new(""), &["link".into()]);
            assert_eq!(done, 1);
            assert!(outside.join("keep.txt").exists());
        }
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(outside).ok();
    }
}
