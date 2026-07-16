//! Path navigation: the launcher's answer to hunting files in Finder.
//! When the query is path-shaped (see beckon_core::pathintent), the
//! panel becomes a keyboard file browser:
//!
//! * An existing path shows the target's action rows: Open (Return's
//!   default), Reveal in Finder, Quick Look, Copy Path, followed by the
//!   directory's children when it is a folder.
//! * A partial path completes against its parent directory; Return on a
//!   folder drills in (the engine rewrites the query), so `~/ge` walks
//!   to `~/geist/` and onward without touching the mouse.
//! * A dead path says so instead of dumping app-search noise.
//!
//! Everything filesystem lives here; classification and formatting are
//! pure functions in beckon_core::pathintent. Quick Look shells to the
//! local /usr/bin/qlmanage binary (airgap clean, no network).

use crate::ffi::{self, msg, Bool, Id};
use beckon_core::pathintent::{self, PathQuery};
use std::fs;
use std::path::Path;

/// Row glyphs: instant scannability in a text-only results table.
const GLYPH_DIR: &str = "\u{1F4C1}"; // folder
const GLYPH_FILE: &str = "\u{1F4C4}"; // page
const GLYPH_APP: &str = "\u{2318}"; // command key, the mac glyph

/// What activating a path row does. The engine maps these onto its
/// existing dismiss/copy/requery machinery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathAction {
    /// Open with the default app (Finder for folders, launch for apps).
    Open(String),
    /// Select the item in a Finder window.
    Reveal(String),
    /// qlmanage -p preview.
    QuickLook(String),
    /// Put the absolute path on the clipboard.
    CopyPath(String),
    /// Rewrite the query to this path and keep browsing.
    Drill(String),
    /// A hint row; Return does nothing.
    None,
}

/// One row of the path view.
#[derive(Clone, Debug)]
pub struct PathRow {
    pub title: String,
    pub subtitle: String,
    pub action: PathAction,
}

/// Build the rows for a path-shaped query, or None when the query is
/// not path-shaped and belongs to the normal pipeline. `limit` caps the
/// row count (action rows first, children fill the rest).
pub fn rows(query: &str, limit: usize) -> Option<Vec<PathRow>> {
    let home = std::env::var("HOME").unwrap_or_default();
    let pq = pathintent::parse(query, &home)?;
    let path = Path::new(&pq.expanded);
    let rows = match fs::metadata(path) {
        Ok(meta) => existing_rows(&pq, meta.is_dir(), meta.len(), &home, limit),
        Err(_) => completion_rows(&pq, &home, limit).unwrap_or_else(|| {
            vec![PathRow {
                title: "No such path".to_string(),
                subtitle: pathintent::abbreviate_home(&pq.expanded, &home),
                action: PathAction::None,
            }]
        }),
    };
    Some(rows)
}

fn name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn glyph_for(name: &str, is_dir: bool) -> &'static str {
    if name.ends_with(".app") {
        GLYPH_APP
    } else if is_dir {
        GLYPH_DIR
    } else {
        GLYPH_FILE
    }
}

/// Action rows for a path that exists, plus a folder's children.
fn existing_rows(pq: &PathQuery, is_dir: bool, len: u64, home: &str, limit: usize) -> Vec<PathRow> {
    let path = &pq.expanded;
    let name = name_of(path);
    let display = pathintent::abbreviate_home(path, home);
    let kind = pathintent::kind_word(&name, is_dir);
    let is_app = name.ends_with(".app");

    let (open_title, detail) = if is_app {
        (format!("{GLYPH_APP} Launch {name}"), String::new())
    } else if is_dir {
        let items = fs::read_dir(path).map(|d| d.count()).unwrap_or(0);
        let noun = if items == 1 { "item" } else { "items" };
        (
            format!("{GLYPH_DIR} Open {name} in Finder"),
            format!("{items} {noun} \u{00B7} "),
        )
    } else {
        (
            format!("{GLYPH_FILE} Open {name}"),
            format!("{} \u{00B7} ", pathintent::format_size(len)),
        )
    };

    let mut rows = vec![PathRow {
        title: open_title,
        subtitle: format!("{kind} \u{00B7} {detail}{display}"),
        action: PathAction::Open(path.clone()),
    }];
    rows.push(PathRow {
        title: "Reveal in Finder".to_string(),
        subtitle: display.clone(),
        action: PathAction::Reveal(path.clone()),
    });
    if !is_dir || is_app {
        rows.push(PathRow {
            title: "Quick Look".to_string(),
            subtitle: "preview without opening".to_string(),
            action: PathAction::QuickLook(path.clone()),
        });
    }
    rows.push(PathRow {
        title: "Copy Path".to_string(),
        subtitle: path.clone(),
        action: PathAction::CopyPath(path.clone()),
    });

    // A folder browses: its children fill the remaining rows, folders
    // first. Return on any child drills the query into it.
    if is_dir && !is_app {
        let mut children = list_dir(path, "", false);
        children.truncate(limit.saturating_sub(rows.len()));
        rows.extend(children);
    }
    rows.truncate(limit);
    rows
}

/// Completion rows: the parent's entries whose names match the typed
/// leaf, prefix matches ranked above substring matches, folders first
/// inside each band. Hidden entries appear only when the leaf itself
/// starts with a dot.
fn completion_rows(pq: &PathQuery, home: &str, limit: usize) -> Option<Vec<PathRow>> {
    let path = Path::new(&pq.expanded);
    let parent = path.parent()?;
    let leaf = path.file_name()?.to_string_lossy().into_owned();
    if !parent.is_dir() {
        return None;
    }
    let _ = home;
    let mut rows = list_dir(&parent.to_string_lossy(), &leaf, leaf.starts_with('.'));
    if rows.is_empty() {
        return None;
    }
    rows.truncate(limit);
    Some(rows)
}

/// List a directory as drill rows, filtered by `leaf` (empty keeps
/// everything). Sort: match band (prefix, then substring), folders
/// before files, then name. Deterministic.
fn list_dir(dir: &str, leaf: &str, show_hidden: bool) -> Vec<PathRow> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let needle = leaf.to_lowercase();
    let mut scored: Vec<(u8, bool, String, String, bool, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !show_hidden {
            continue;
        }
        let lower = name.to_lowercase();
        let band = if needle.is_empty() || lower.starts_with(&needle) {
            0u8
        } else if lower.contains(&needle) {
            1
        } else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        scored.push((
            band,
            !meta.is_dir(),
            name,
            entry.path().to_string_lossy().into_owned(),
            meta.is_dir(),
            meta.len(),
        ));
    }
    scored.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    scored
        .into_iter()
        .map(|(_, _, name, full, is_dir, len)| {
            let kind = pathintent::kind_word(&name, is_dir);
            let subtitle = if is_dir {
                kind.to_string()
            } else {
                format!("{kind} \u{00B7} {}", pathintent::format_size(len))
            };
            PathRow {
                title: format!("{} {name}", glyph_for(&name, is_dir)),
                subtitle,
                action: PathAction::Drill(full),
            }
        })
        .collect()
}

/// The query text a drill rewrites to: home-abbreviated, and folders get
/// the trailing slash so their contents list immediately.
pub fn drill_query(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = pathintent::abbreviate_home(path, &home);
    if Path::new(path).is_dir() && !path.ends_with(".app") {
        format!("{display}/")
    } else {
        display
    }
}

/// Open a path with its default handler via NSWorkspace (Finder for
/// folders, launch for .app bundles, default app for documents).
pub fn open(path: &str) -> bool {
    // Safety: main thread; fileURLWithPath: returns an NSURL or nil and
    // openURL: takes an NSURL returning BOOL.
    unsafe {
        let url = msg!(Id: ffi::class("NSURL"), ffi::sel("fileURLWithPath:"),
            Id: ffi::nsstring(path));
        if url == ffi::NIL {
            return false;
        }
        let ws = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
        msg!(Bool: ws, ffi::sel("openURL:"), Id: url) != 0
    }
}

/// Select the item in a Finder window (the "where is it" answer).
pub fn reveal(path: &str) -> bool {
    // Safety: main thread; selectFile:inFileViewerRootedAtPath: takes
    // two NSStrings (the second may be empty) and returns BOOL.
    unsafe {
        let ws = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
        msg!(Bool: ws, ffi::sel("selectFile:inFileViewerRootedAtPath:"),
            Id: ffi::nsstring(path), Id: ffi::nsstring(""))
            != 0
    }
}

/// Quick Look preview via the local qlmanage binary, detached so the
/// launcher never waits on the preview window.
pub fn quick_look(path: &str) {
    use std::process::{Command, Stdio};
    let result = Command::new("/usr/bin/qlmanage")
        .arg("-p")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = result {
        eprintln!("beckon: quick look failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("beckon-pathnav-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Alpha")).unwrap();
        fs::create_dir_all(dir.join("beta")).unwrap();
        fs::write(dir.join("apple.txt"), b"1234").unwrap();
        fs::write(dir.join("notes.md"), b"x").unwrap();
        fs::write(dir.join(".hidden"), b"x").unwrap();
        dir
    }

    #[test]
    fn non_paths_are_not_claimed() {
        assert!(rows("safari", 9).is_none());
        assert!(rows("clip foo", 9).is_none());
    }

    #[test]
    fn existing_file_gets_action_rows() {
        let dir = fixture();
        let q = dir.join("apple.txt").to_string_lossy().into_owned();
        let rows = rows(&q, 9).unwrap();
        assert!(rows[0].title.contains("Open apple.txt"));
        assert!(rows[0].subtitle.contains("4 B"));
        assert_eq!(rows[1].title, "Reveal in Finder");
        assert_eq!(rows[2].title, "Quick Look");
        assert_eq!(rows[3].title, "Copy Path");
        assert_eq!(rows[3].action, PathAction::CopyPath(q));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn folder_lists_children_folders_first() {
        let dir = fixture();
        let q = dir.to_string_lossy().into_owned();
        let all = rows(&q, 20).unwrap();
        assert!(all[0].title.contains("in Finder"));
        assert!(all[0].subtitle.contains("items"));
        let titles: Vec<&str> = all.iter().map(|r| r.title.as_str()).collect();
        let alpha = titles.iter().position(|t| t.contains("Alpha")).unwrap();
        let beta = titles.iter().position(|t| t.contains("beta")).unwrap();
        let apple = titles.iter().position(|t| t.contains("apple.txt")).unwrap();
        assert!(alpha < beta && beta < apple, "folders first, by name");
        assert!(!titles.iter().any(|t| t.contains(".hidden")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_leaf_completes_prefix_before_substring() {
        let dir = fixture();
        let q = dir.join("a").to_string_lossy().into_owned();
        let comp = rows(&q, 9).unwrap();
        // Prefix band: Alpha (dir) then apple.txt; substring band: beta.
        assert!(comp[0].title.contains("Alpha"));
        assert!(comp[1].title.contains("apple.txt"));
        assert!(matches!(comp[0].action, PathAction::Drill(_)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dead_path_says_so() {
        let dir = fixture();
        let q = dir.join("zzz-nope/deeper").to_string_lossy().into_owned();
        let hint = rows(&q, 9).unwrap();
        assert_eq!(hint[0].title, "No such path");
        assert_eq!(hint[0].action, PathAction::None);
        let _ = fs::remove_dir_all(dir);
    }
}
