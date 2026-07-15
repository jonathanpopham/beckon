//! The launcher engine: the wiring between typing and things happening.
//! Owns the app index, the frecency store, and the two ui.rs callbacks,
//! and maps query text to ranked rows and Return to a launch (or a
//! clipboard copy for calculator results).
//!
//! This module is the shell edge of the deterministic core: every clock
//! read (SystemTime -> unix seconds) and every file touch happens here,
//! so beckon-core keeps its injected-inputs purity and the golden tests
//! keep their teeth.
//!
//! Threading invariants: everything here is main-thread only, like the
//! rest of the shell (ffi module invariant 2). init() runs before the run
//! loop starts; handle_query and handle_activate are invoked by the ui.rs
//! field-delegate hooks on the main run loop; summon() is called by the
//! Carbon hotkey callback, which also runs on the main thread. The ENGINE
//! Mutex exists to satisfy Rust's static-safety rules, not to enable
//! cross-thread use, and it is never held across a call into ui.rs or
//! panel.rs.
//!
//! Race-freedom of activation: `Engine::results` is updated in the same
//! main-thread call that hands the matching rows to ui::set_items, and
//! activation reads it on the same thread, so the entry a Return keypress
//! resolves to is always the entry the table was showing. Callback lock
//! rules from ui.rs are respected: the callbacks registered here never
//! call their own set_on_* functions; set_items and panel calls are safe.
//!
//! Index freshness: summon() re-runs apps::index() on every panel show so
//! a freshly installed app is launchable without restarting beckon.
//! Measured on this machine (108 apps, debug build): 35 ms cold, 6 to
//! 7 ms warm, well under the 50 ms budget for a show.

use crate::ffi::{self, msg, Bool, Id};
use crate::{apps, panel, pasteboard, system, ui, winmgmt};
use beckon_core::calc;
use beckon_core::frecency::FrecencyStore;
use beckon_core::persist;
use beckon_core::router::{self, Item, QueryIntent};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Rows shown per query. Nine fits the panel without scrolling and maps
/// to the single keystroke depth a launcher lives at.
const MAX_RESULTS: usize = 9;

/// The frecency store file, under the beckon store root.
const FRECENCY_FILE: &str = "frecency.txt";

/// What activating a result row does. One entry per visible row, in row
/// order; see the module docs for why this mirrors the table race free.
#[derive(Clone, Debug)]
enum Entry {
    /// Launch the app bundle at `path`, then record a use of `id`.
    App { id: String, path: String },
    /// Copy the calculator result to the clipboard.
    Calc { display: String },
    /// Run a command source action ("system.*" via system::activate,
    /// "window.*" via winmgmt::activate), then record a use of `id`.
    Command { id: String },
    /// Copy a clipboard history entry back to the pasteboard
    /// (pasteboard::activate bumps its own recency; no frecency record).
    Clip { id: String },
}

struct Engine {
    /// App index snapshot; refreshed on every summon.
    apps: Vec<Item>,
    /// Static command pool (system commands plus window actions),
    /// computed once at init; both sources are deterministic.
    commands: Vec<Item>,
    frecency: FrecencyStore,
    frecency_path: PathBuf,
    /// Activation targets for the rows currently in the table.
    results: Vec<Entry>,
}

static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

fn engine() -> &'static Mutex<Engine> {
    ENGINE.get().expect("engine::init runs before any callback")
}

/// Current wall clock as unix seconds. The one clock read in the process;
/// a clock before the epoch degrades to 0 rather than panicking.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the frecency store from `path`. A missing file is a fresh
/// install; a corrupt or unreadable file must not take the launcher down,
/// so it logs to stderr and starts fresh (the next save overwrites it).
fn load_frecency(path: &Path) -> FrecencyStore {
    let bytes = match persist::read_optional(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return FrecencyStore::new(),
        Err(e) => {
            eprintln!(
                "beckon: cannot read frecency store {}: {e}; starting fresh",
                path.display()
            );
            return FrecencyStore::new();
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "beckon: frecency store {} is not UTF-8 ({e}); starting fresh",
                path.display()
            );
            return FrecencyStore::new();
        }
    };
    match text.parse() {
        Ok(store) => store,
        Err(e) => {
            eprintln!(
                "beckon: corrupt frecency store {}: {e}; starting fresh",
                path.display()
            );
            FrecencyStore::new()
        }
    }
}

/// Index apps, load frecency, and register the two ui callbacks. Called
/// once by the shell at startup, on the main thread, after panel::init.
pub fn init() {
    let start = Instant::now();
    let apps = apps::index();
    println!(
        "beckon: indexed {} apps in {} ms",
        apps.len(),
        start.elapsed().as_millis()
    );

    let root = match persist::ensure_store_root() {
        Ok(root) => root,
        Err(e) => {
            let fallback = persist::store_root();
            eprintln!(
                "beckon: cannot create store root {}: {e}; frecency will not persist",
                fallback.display()
            );
            fallback
        }
    };
    let frecency_path = root.join(FRECENCY_FILE);
    let mut frecency = load_frecency(&frecency_path);
    // Entries decayed to zero are dead weight; drop them at the shell
    // edge so the file stays small. Scores are unchanged (they were 0).
    frecency.prune(now_secs());

    let mut commands = system::items();
    commands.extend(winmgmt::items());

    if ENGINE
        .set(Mutex::new(Engine {
            apps,
            commands,
            frecency,
            frecency_path,
            results: Vec::new(),
        }))
        .is_err()
    {
        panic!("engine::init called twice");
    }

    ui::set_on_query_changed(|q| handle_query(&q));
    ui::set_on_activate(handle_activate);
    // Main thread, before the run loop starts: the watcher's poll timer
    // lands on the main run loop, and its store under the same root.
    pasteboard::start();
}

/// Hotkey entry: refresh the app index (measured cheap; module docs),
/// clear the query, show the default frecency list, and bring the panel
/// up with the caret ready. Programmatic setStringValue: does not fire
/// the query callback, so the pipeline is run explicitly.
pub fn summon() {
    let fresh = apps::index();
    engine().lock().unwrap().apps = fresh;
    panel::set_query("");
    handle_query("");
    panel::show();
}

/// The query pipeline: parse intent, compute rows, remember the matching
/// activation entries, and hand the rows to the table.
fn handle_query(raw: &str) {
    let rows = {
        let mut eng = engine().lock().unwrap();
        eng.refresh_results(raw)
    };
    ui::set_items(&rows);
}

impl Engine {
    /// Compute the rows for `raw` and set `self.results` to match, one
    /// entry per row.
    fn refresh_results(&mut self, raw: &str) -> Vec<ui::RowData> {
        let now = now_secs();
        // Clipboard history rides a keyword trigger, not the fuzzy pool:
        // history entries are arbitrary text and would pollute app
        // ranking. "clip" or "clipboard" as the first word, with the rest
        // of the query searching the history.
        let trimmed = raw.trim();
        if let Some(first) = trimmed.split_whitespace().next() {
            let lower = first.to_lowercase();
            if lower == "clip" || lower == "clipboard" {
                let rest = trimmed[first.len()..].trim();
                return self.clip_rows(rest);
            }
        }
        match QueryIntent::parse(raw) {
            // Empty query: a pure frecency list (recent habits first,
            // then alphabetical), so a fresh install shows the
            // alphabetical head of the app index.
            QueryIntent::Empty => self.search_rows("", now),
            QueryIntent::Search(q) => self.search_rows(&q, now),
            QueryIntent::Calc(expr) => match calc::eval(&expr) {
                Ok(result) => {
                    let row = ui::RowData {
                        title: result.display.clone(),
                        subtitle: format!("{expr} (press Return to copy)"),
                    };
                    self.results = vec![Entry::Calc {
                        display: result.display,
                    }];
                    vec![row]
                }
                // The intent heuristic leaned Calc but the expression
                // does not evaluate ("1password"); fall through to app
                // search. The calculator is the final judge.
                Err(_) => self.search_rows(&expr, now),
            },
        }
    }

    /// Rank apps and commands together against `query` and keep the top
    /// rows. The pool is rebuilt per keystroke; at ~130 items the clone
    /// cost is noise next to the AppKit reload that follows.
    fn search_rows(&mut self, query: &str, now: u64) -> Vec<ui::RowData> {
        let mut pool = self.apps.clone();
        pool.extend(self.commands.iter().cloned());
        let ranked = router::rank(query, &pool, &self.frecency, now);
        self.results.clear();
        let mut rows = Vec::new();
        for r in ranked.into_iter().take(MAX_RESULTS) {
            rows.push(ui::RowData {
                title: r.item.title,
                // App subtitles are absolute bundle paths; command
                // subtitles are one-line descriptions.
                subtitle: r.item.subtitle.clone(),
            });
            self.results.push(match r.item.kind {
                router::ItemKind::App => Entry::App {
                    id: r.item.id,
                    path: r.item.subtitle,
                },
                _ => Entry::Command { id: r.item.id },
            });
        }
        rows
    }

    /// Rows for the clipboard history trigger: recent entries for an
    /// empty search, ClipStore search otherwise; pasteboard::items owns
    /// shaping and the cap.
    fn clip_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let items = pasteboard::items(query);
        self.results.clear();
        let mut rows = Vec::new();
        for item in items {
            rows.push(ui::RowData {
                title: item.title,
                subtitle: item.subtitle,
            });
            self.results.push(Entry::Clip { id: item.id });
        }
        rows
    }
}

/// The activation pipeline: map the row index to its entry and act. A
/// failed launch leaves the panel up so the error context is not thrown
/// away; success hides the panel and clears the query for the next
/// summon.
fn handle_activate(index: usize) {
    let entry = engine().lock().unwrap().results.get(index).cloned();
    let Some(entry) = entry else {
        return;
    };
    match entry {
        Entry::App { id, path } => match apps::launch(&path) {
            Ok(()) => {
                record_use_and_save(&id);
                dismiss();
            }
            Err(e) => eprintln!("beckon: launch failed: {e}"),
        },
        Entry::Calc { display } => {
            if copy_to_clipboard(&display) {
                // Our own write; the history watcher must not capture it.
                pasteboard::note_own_write();
                dismiss();
            } else {
                eprintln!("beckon: pasteboard write failed");
            }
        }
        Entry::Command { id } => {
            let result = if id.starts_with("window.") {
                winmgmt::activate(&id)
            } else {
                system::activate(&id)
            };
            match result {
                Ok(()) => {
                    record_use_and_save(&id);
                    dismiss();
                }
                Err(e) => {
                    eprintln!("beckon: command {id} failed: {e}");
                    // Window actions need the Accessibility grant; offer
                    // the system prompt once the need is proven.
                    if id.starts_with("window.") && !winmgmt::is_trusted() {
                        winmgmt::prompt_for_trust();
                    }
                }
            }
        }
        Entry::Clip { id } => match pasteboard::activate(&id) {
            Ok(()) => dismiss(),
            Err(e) => eprintln!("beckon: clipboard copy failed: {e}"),
        },
    }
}

/// Hide the panel and clear the query so the next summon starts fresh.
fn dismiss() {
    panel::hide();
    panel::set_query("");
}

/// Record one use of `id` at the current wall clock and persist the
/// store atomically (canonical Display text via write_atomic). The
/// launch path calls this through record_use_and_save; it is public so
/// the --smoke run can prove the persistence path without launching an
/// app.
pub fn record_use_now(id: &str) -> Result<(), String> {
    let mut eng = engine().lock().unwrap();
    eng.frecency.record_use(id, now_secs());
    let text = eng.frecency.to_string();
    persist::write_atomic(&eng.frecency_path, text.as_bytes()).map_err(|e| {
        format!(
            "cannot save frecency to {}: {e}",
            eng.frecency_path.display()
        )
    })
}

fn record_use_and_save(id: &str) {
    if let Err(e) = record_use_now(id) {
        eprintln!("beckon: {e}");
    }
}

/// Copy `text` to the general pasteboard. The type identifier is the
/// documented literal value of NSPasteboardTypeString, spelled out
/// because no headers are linked to import the constant from.
fn copy_to_clipboard(text: &str) -> bool {
    // Safety: main thread; clearContents returns NSInteger (the new
    // change count) and setString:forType: takes two NSStrings and
    // returns BOOL.
    unsafe {
        let pb = msg!(Id: ffi::class("NSPasteboard"), ffi::sel("generalPasteboard"));
        let _ = msg!(isize: pb, ffi::sel("clearContents"));
        msg!(Bool: pb, ffi::sel("setString:forType:"),
            Id: ffi::nsstring(text),
            Id: ffi::nsstring("public.utf8-plain-text"))
            != 0
    }
}
