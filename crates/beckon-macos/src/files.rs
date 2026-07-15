//! File search: a fast local index with a Spotlight fallback.
//!
//! Two tiers:
//!
//! 1. **Local index.** A pure std::fs walk over the directories a launcher
//!    query actually means: ~/Desktop, ~/Documents, ~/Downloads (each to
//!    [`DEPTH_CAP`] levels deep), plus the visible top-level entries of ~
//!    itself (one level only; their contents are covered by the dedicated
//!    roots or are not worth indexing). Hidden entries and the junk names
//!    in [`SKIP_DIRS`] are skipped everywhere, symlinks are never followed
//!    (cycle safety; a symlink is indexed as a plain entry), and the whole
//!    index stops cleanly at [`MAX_ENTRIES`]. The walk runs on a background
//!    std::thread spawned by [`start`] (idempotent) or [`refresh`]
//!    (coalescing); partial results are published per root into a Mutex
//!    snapshot, so [`items`] never blocks and simply sees what exists so
//!    far. Per-directory entries are sorted by name, making the finished
//!    index a pure function of the filesystem.
//!
//! 2. **Spotlight fallback.** When the local index yields fewer than
//!    [`LOCAL_MIN_ROWS`] rows for a query, [`spotlight`] shells out to the
//!    local system binary /usr/bin/mdfind (airgap clean, no network):
//!
//!    ```text
//!    /usr/bin/mdfind -onlyin "$HOME" "kMDItemFSName == '*<query>*'c"
//!    ```
//!
//!    a case-insensitive filename-contains query. The characters ' " \ *
//!    are stripped from the query first so it cannot escape the metadata
//!    query literal. mdfind has no timeout or count flag, so the cap is
//!    enforced by reading at most [`SPOTLIGHT_LINE_CAP`] lines from its
//!    stdout and then dropping the child: kill plus wait, so no zombie is
//!    left behind and a runaway query cannot hold the pipe open.
//!
//! The registry contract: [`items`] returns [`ItemKind::File`] rows with
//! id `file.<absolute path>`, title = file name, subtitle = the path with
//! $HOME abbreviated to `~`. The empty query returns nothing (a file list
//! without a query is noise). Non-empty queries rank the local index with
//! [`fuzzy::score`] on the file name, score descending with a
//! deterministic tie-break by path, capped at [`RESULT_CAP`]; Spotlight
//! hits are deduped by path and appended only when the local tier runs
//! thin. [`activate`] opens the file or folder in its default app via
//! NSWorkspace openURL: (the supported, synchronous-BOOL API, so failure
//! is observable), falling back to /usr/bin/open if AppKit declines.
//!
//! This module is not yet wired into the engine; the integrator removes
//! the file-wide dead_code allow below when hooking it up.
#![allow(dead_code)]

use crate::ffi::{self, msg, Bool, Id};
use beckon_core::fuzzy;
use beckon_core::router::{Item, ItemKind};
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Directory names never indexed and never descended into, at any depth.
/// Hidden entries (leading dot) are skipped by the same check that
/// consults this list, which is why .git and .venv are technically
/// redundant here; they stay listed so the policy reads complete.
const SKIP_DIRS: &[&str] = &[
    "Library",
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    "venv",
    ".venv",
];

/// How deep the walk descends below each dedicated root. The root's
/// immediate children are depth 1, so a value of 4 indexes four levels.
const DEPTH_CAP: usize = 4;

/// Hard cap on total indexed entries across all roots. The walk stops
/// cleanly the moment the accumulator reaches this size.
const MAX_ENTRIES: usize = 20_000;

/// Maximum rows [`items`] returns for one query.
const RESULT_CAP: usize = 9;

/// When the local tier yields fewer rows than this, Spotlight tops up.
const LOCAL_MIN_ROWS: usize = 3;

/// Maximum lines read from mdfind before the child is killed.
const SPOTLIGHT_LINE_CAP: usize = 32;

/// One indexed filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// The file or directory name, what fuzzy matching runs against.
    name: String,
    /// Absolute path.
    path: String,
    /// Whether the entry is a directory (symlinks count as plain files;
    /// the walk never follows them).
    is_dir: bool,
}

/// The shared snapshot the background walk publishes into.
struct IndexState {
    entries: Vec<Entry>,
    /// A walk is currently running; refresh requests coalesce on this.
    building: bool,
    /// At least one full walk has completed.
    built_once: bool,
}

fn state() -> &'static Mutex<IndexState> {
    static STATE: OnceLock<Mutex<IndexState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(IndexState {
            entries: Vec::new(),
            building: false,
            built_once: false,
        })
    })
}

static STARTED: AtomicBool = AtomicBool::new(false);

/// Kick off the first background index build. Idempotent: only the first
/// call spawns; later calls are free no-ops. Never blocks.
pub fn start() {
    if !STARTED.swap(true, Ordering::SeqCst) {
        spawn_walk();
    }
}

/// Re-walk the roots on a fresh background thread. Coalesces: if a walk
/// is already running, this returns without spawning another.
pub fn refresh() {
    spawn_walk();
}

/// Whether at least one full walk has completed. [`items`] works either
/// way; before readiness it ranks whatever partial snapshot exists.
pub fn ready() -> bool {
    let s = state().lock().unwrap();
    s.built_once
}

/// Spawn the walk thread unless one is already running (coalescing).
fn spawn_walk() {
    {
        let mut s = state().lock().unwrap();
        if s.building {
            return;
        }
        s.building = true;
    }
    std::thread::spawn(|| {
        let mut acc: Vec<Entry> = Vec::new();
        for (root, max_depth) in index_roots() {
            walk_dir(&root, 1, max_depth, &mut acc, MAX_ENTRIES);
            // Publish per root so a query mid-build sees partial results.
            let mut s = state().lock().unwrap();
            s.entries = acc.clone();
            if acc.len() >= MAX_ENTRIES {
                break;
            }
        }
        let mut s = state().lock().unwrap();
        s.entries = acc;
        s.building = false;
        s.built_once = true;
    });
}

/// The walk roots, each paired with its depth cap: the home directory
/// itself at depth 1 (visible top-level entries only), then the three
/// user content directories at [`DEPTH_CAP`]. Children of the dedicated
/// roots never duplicate the home pass because the home pass stops at
/// depth 1 and a walk emits children, not the root itself.
fn index_roots() -> Vec<(PathBuf, usize)> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    vec![
        (home.clone(), 1),
        (home.join("Desktop"), DEPTH_CAP),
        (home.join("Documents"), DEPTH_CAP),
        (home.join("Downloads"), DEPTH_CAP),
    ]
}

/// Whether a directory entry is skipped entirely: hidden (leading dot)
/// or named in [`SKIP_DIRS`]. Applies uniformly at every depth.
fn skip_name(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// Recursively index `dir`, whose children sit at `depth`. Entries are
/// visited in name order (read_dir order is arbitrary; sorting makes the
/// index deterministic). Stops descending past `max_depth` and stops
/// entirely once `out` holds `cap` entries. Symlinks are recorded via
/// their own file type (never followed), so cycles are impossible.
fn walk_dir(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<Entry>, cap: usize) {
    if depth > max_depth || out.len() >= cap {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if skip_name(name) {
            continue;
        }
        // file_type does not follow symlinks: a symlink to a directory is
        // indexed as a non-directory and never descended into.
        let Ok(ftype) = entry.file_type() else {
            continue;
        };
        children.push((name.to_string(), entry.path(), ftype.is_dir()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path, is_dir) in children {
        if out.len() >= cap {
            return;
        }
        let Some(path_str) = path.to_str() else {
            continue;
        };
        out.push(Entry {
            name,
            path: path_str.to_string(),
            is_dir,
        });
        if is_dir {
            walk_dir(&path, depth + 1, max_depth, out, cap);
        }
    }
}

/// Rank `entries` against `query` by fuzzy score on the name, descending,
/// with a deterministic tie-break by path ascending, capped at `cap`.
/// Non-matching entries are dropped.
fn rank_local(query: &str, entries: &[Entry], cap: usize) -> Vec<Entry> {
    let mut scored: Vec<(i64, &Entry)> = entries
        .iter()
        .filter_map(|e| fuzzy::score(query, &e.name).map(|m| (m.score, e)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
    scored.truncate(cap);
    scored.into_iter().map(|(_, e)| e.clone()).collect()
}

/// The registry id for a path: `file.<absolute path>`.
fn id_for_path(path: &str) -> String {
    format!("file.{path}")
}

/// The path back out of a registry id, or None when the id is not ours.
fn path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix("file.")
}

/// Abbreviate `home` (and nothing else) to `~` at the front of `path`.
fn abbreviate_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn home_string() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn item_for(name: &str, path: &str, home: &str) -> Item {
    Item::new(
        &id_for_path(path),
        name,
        &abbreviate_home(path, home),
        ItemKind::File,
    )
}

/// File rows for `query`, ready for the registry. Empty (or all
/// whitespace) queries return nothing. Local index rows come first,
/// ranked by [`rank_local`]; when they number fewer than
/// [`LOCAL_MIN_ROWS`], Spotlight hits are deduped by path and appended
/// up to [`RESULT_CAP`]. Never blocks on the index build: a build in
/// flight simply means a partial (or empty) local tier.
pub fn items(query: &str) -> Vec<Item> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    // Defensive: the integrator calls start() at init, but a stray early
    // query should kick the index off rather than stay empty forever.
    start();
    let home = home_string();
    let local = {
        let s = state().lock().unwrap();
        rank_local(q, &s.entries, RESULT_CAP)
    };
    let mut seen: HashSet<String> = local.iter().map(|e| e.path.clone()).collect();
    let mut out: Vec<Item> = local
        .iter()
        .map(|e| item_for(&e.name, &e.path, &home))
        .collect();
    if out.len() < LOCAL_MIN_ROWS {
        for (name, path) in spotlight(q) {
            if out.len() >= RESULT_CAP {
                break;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            out.push(item_for(&name, &path, &home));
        }
    }
    out
}

/// Tier 2: ask Spotlight for filename matches under $HOME. Returns
/// (file name, absolute path) pairs, at most [`SPOTLIGHT_LINE_CAP`].
///
/// Invocation, exactly:
///
/// ```text
/// /usr/bin/mdfind -onlyin "$HOME" "kMDItemFSName == '*<query>*'c"
/// ```
///
/// The `c` modifier makes the comparison case-insensitive and the `*`
/// wildcards make it a contains match. The characters ' " \ * are
/// stripped from the query before it is spliced into the literal, so the
/// query cannot terminate the string or inject its own wildcards (the
/// arguments go straight to exec, so there is no shell to inject into).
/// mdfind has no timeout or count flag: the cap is enforced by reading at
/// most the cap in lines and then killing and waiting on the child, which
/// both bounds the work and reaps the process.
pub fn spotlight(query: &str) -> Vec<(String, String)> {
    let home = home_string();
    if home.is_empty() {
        return Vec::new();
    }
    let sanitized: String = query
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\' | '*'))
        .collect();
    if sanitized.trim().is_empty() {
        return Vec::new();
    }
    let md_query = format!("kMDItemFSName == '*{sanitized}*'c");
    let Ok(mut child) = Command::new("/usr/bin/mdfind")
        .arg("-onlyin")
        .arg(&home)
        .arg(&md_query)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if out.len() >= SPOTLIGHT_LINE_CAP {
                break;
            }
            let Some(name) = Path::new(&line).file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            out.push((name.to_string(), line.clone()));
        }
    }
    // Cap reached (or EOF): drop the child either way. kill on an
    // already-exited process is a harmless error; wait reaps it.
    let _ = child.kill();
    let _ = child.wait();
    out
}

/// Open the file or directory behind a `file.<path>` id in its default
/// application. Primary path is NSWorkspace openURL: with a file URL: it
/// is the supported AppKit API and returns a BOOL synchronously, so a
/// refusal is observable (unlike the fire-and-forget completion-handler
/// variants). If AppKit returns NO or NSURL rejects the path, fall back
/// to /usr/bin/open, which handles some edge cases (for example files
/// with no claimed type) via LaunchServices on its own.
pub fn activate(id: &str) -> Result<(), String> {
    let Some(path) = path_from_id(id) else {
        return Err(format!("not a file id: {id}"));
    };
    if !Path::new(path).exists() {
        return Err(format!("no file at {path}"));
    }
    let _pool = ffi::AutoreleasePool::new();
    // Safety: fileURLWithPath: takes an NSString and returns an NSURL or
    // nil (handled); sharedWorkspace returns the singleton; openURL:
    // takes an NSURL and returns a BOOL.
    let opened = unsafe {
        let url = msg!(Id: ffi::class("NSURL"), ffi::sel("fileURLWithPath:"),
            Id: ffi::nsstring(path));
        if url.is_null() {
            ffi::NO
        } else {
            let workspace = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
            msg!(Bool: workspace, ffi::sel("openURL:"), Id: url)
        }
    };
    if opened != ffi::NO {
        return Ok(());
    }
    let status = Command::new("/usr/bin/open")
        .arg(path)
        .status()
        .map_err(|e| format!("open fallback failed to spawn: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "could not open {path} (NSWorkspace and open both refused)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree under the system temp dir, removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> TempTree {
            let dir = std::env::temp_dir().join(format!(
                "beckon-files-test-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempTree(dir)
        }

        fn file(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }

        fn dir(&self, rel: &str) {
            std::fs::create_dir_all(self.0.join(rel)).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn walk_all(root: &Path, max_depth: usize, cap: usize) -> Vec<Entry> {
        let mut out = Vec::new();
        walk_dir(root, 1, max_depth, &mut out, cap);
        out
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn walk_skips_hidden_and_junk_dirs() {
        let t = TempTree::new("skip");
        t.file("keep.txt");
        t.file(".hidden.txt");
        t.file("node_modules/lib.js");
        t.file("target/debug/bin");
        t.file("__pycache__/mod.pyc");
        t.file("venv/pyvenv.cfg");
        t.file(".venv/pyvenv.cfg");
        t.file("Library/Caches/blob");
        t.dir(".git");
        let entries = walk_all(&t.0, DEPTH_CAP, MAX_ENTRIES);
        assert_eq!(names(&entries), vec!["keep.txt"]);
    }

    #[test]
    fn walk_respects_the_depth_cap() {
        let t = TempTree::new("depth");
        t.file("d1.txt");
        t.file("a/d2.txt");
        t.file("a/b/d3.txt");
        t.file("a/b/c/d4.txt");
        t.file("a/b/c/d/d5.txt");
        let entries = walk_all(&t.0, 4, MAX_ENTRIES);
        let got = names(&entries);
        // Depth 4 includes the dir "d" itself and the file d4.txt; the
        // depth-5 file d5.txt is out.
        assert!(got.contains(&"d4.txt"), "{got:?}");
        assert!(got.contains(&"d"), "{got:?}");
        assert!(!got.contains(&"d5.txt"), "{got:?}");
        // A depth-1 walk sees only the top level.
        let shallow = walk_all(&t.0, 1, MAX_ENTRIES);
        assert_eq!(names(&shallow), vec!["a", "d1.txt"]);
    }

    #[test]
    fn walk_stops_cleanly_at_the_entry_cap() {
        let t = TempTree::new("cap");
        for i in 0..10 {
            t.file(&format!("f{i}.txt"));
        }
        let entries = walk_all(&t.0, DEPTH_CAP, 3);
        assert_eq!(entries.len(), 3);
        // Name order makes the truncation deterministic.
        assert_eq!(names(&entries), vec!["f0.txt", "f1.txt", "f2.txt"]);
    }

    #[test]
    fn walk_records_dirs_and_files_with_absolute_paths() {
        let t = TempTree::new("kinds");
        t.file("doc.txt");
        t.dir("folder");
        let entries = walk_all(&t.0, DEPTH_CAP, MAX_ENTRIES);
        assert_eq!(entries.len(), 2);
        let doc = entries.iter().find(|e| e.name == "doc.txt").unwrap();
        let folder = entries.iter().find(|e| e.name == "folder").unwrap();
        assert!(!doc.is_dir);
        assert!(folder.is_dir);
        for e in &entries {
            assert!(e.path.starts_with('/'), "not absolute: {}", e.path);
        }
    }

    #[test]
    fn walk_is_deterministic() {
        let t = TempTree::new("det");
        t.file("b.txt");
        t.file("a.txt");
        t.file("sub/c.txt");
        let a = walk_all(&t.0, DEPTH_CAP, MAX_ENTRIES);
        let b = walk_all(&t.0, DEPTH_CAP, MAX_ENTRIES);
        assert_eq!(a, b);
        assert_eq!(names(&a), vec!["a.txt", "b.txt", "sub", "c.txt"]);
    }

    fn entry(name: &str, path: &str) -> Entry {
        Entry {
            name: name.to_string(),
            path: path.to_string(),
            is_dir: false,
        }
    }

    #[test]
    fn rank_orders_by_score_then_path_and_caps() {
        let entries = vec![
            entry("notes-archive.txt", "/x/notes-archive.txt"),
            entry("Notes.txt", "/b/Notes.txt"),
            entry("Notes.txt", "/a/Notes.txt"),
            entry("unrelated.png", "/x/unrelated.png"),
        ];
        let ranked = rank_local("notes", &entries, 9);
        // The exact-stem matches win; the tie between identical names
        // breaks by path ascending; the non-match is dropped.
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].path, "/a/Notes.txt");
        assert_eq!(ranked[1].path, "/b/Notes.txt");
        assert_eq!(ranked[2].path, "/x/notes-archive.txt");
        let capped = rank_local("notes", &entries, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].path, "/a/Notes.txt");
    }

    #[test]
    fn rank_drops_non_matches_entirely() {
        let entries = vec![entry("alpha.txt", "/a"), entry("beta.txt", "/b")];
        assert!(rank_local("zzz", &entries, 9).is_empty());
    }

    #[test]
    fn id_round_trips_through_the_registry_form() {
        let path = "/Users/someone/Documents/report.pdf";
        let id = id_for_path(path);
        assert_eq!(id, "file./Users/someone/Documents/report.pdf");
        assert_eq!(path_from_id(&id), Some(path));
        assert_eq!(path_from_id("app.com.apple.Safari"), None);
    }

    #[test]
    fn home_abbreviation() {
        let home = "/Users/someone";
        assert_eq!(
            abbreviate_home("/Users/someone/Documents/a.txt", home),
            "~/Documents/a.txt"
        );
        assert_eq!(abbreviate_home("/Users/someone", home), "~");
        // A sibling that merely shares the prefix is not abbreviated.
        assert_eq!(
            abbreviate_home("/Users/someone-else/b.txt", home),
            "/Users/someone-else/b.txt"
        );
        assert_eq!(abbreviate_home("/etc/hosts", home), "/etc/hosts");
        assert_eq!(abbreviate_home("/etc/hosts", ""), "/etc/hosts");
    }

    #[test]
    fn empty_query_returns_nothing() {
        assert!(items("").is_empty());
        assert!(items("   ").is_empty());
    }

    #[test]
    fn activate_rejects_foreign_ids_and_missing_paths() {
        assert!(activate("app.com.apple.Safari").is_err());
        assert!(activate("file./definitely/not/here.txt").is_err());
    }

    // Hardware checks, excluded from the gate: they walk the real home
    // directory, talk to the live Spotlight store, and open the default
    // text editor. Run manually:
    //     cargo test -p beckon-macos hardware_ -- --ignored --nocapture

    #[test]
    #[ignore = "walks the real home directory and queries Spotlight; run manually"]
    fn hardware_real_index_and_query() {
        assert!(items("").is_empty());
        let t0 = std::time::Instant::now();
        start();
        while !ready() {
            assert!(
                t0.elapsed().as_secs() < 60,
                "index build did not finish within 60s"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let elapsed_ms = t0.elapsed().as_millis();
        let count = state().lock().unwrap().entries.len();
        println!("hardware: index entries={count} build_ms={elapsed_ms}");
        assert!(count > 0, "the real index came back empty");
        assert!(count <= MAX_ENTRIES);
        let hits = items("beckon");
        println!("hardware: query \"beckon\" rows={}", hits.len());
        for item in &hits {
            println!(
                "hardware:   {} | {} | {}",
                item.title, item.subtitle, item.id
            );
            assert_eq!(item.kind, ItemKind::File);
            assert!(item.id.starts_with("file./"));
        }
    }

    #[test]
    #[ignore = "queries the live Spotlight store; run manually"]
    fn hardware_spotlight_fallback() {
        let hits = spotlight("beckon");
        println!("hardware: spotlight rows={}", hits.len());
        for (name, path) in hits.iter().take(5) {
            println!("hardware:   {name} | {path}");
            assert!(path.starts_with('/'));
        }
        assert!(hits.len() <= SPOTLIGHT_LINE_CAP);
    }

    #[test]
    #[ignore = "opens the default text editor; run manually"]
    fn hardware_activate_temp_file() {
        let path = std::env::temp_dir().join("beckon-files-activate-check.txt");
        std::fs::write(&path, b"beckon file-search activation check\n").unwrap();
        let id = id_for_path(path.to_str().unwrap());
        activate(&id).expect("activation failed");
        println!("hardware: activated {id} (a text editor should have opened)");
        // Give LaunchServices a moment before the process exits.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
