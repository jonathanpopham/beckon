//! The launcher engine: the wiring between typing and things happening.
//! Owns the app index, the frecency store, the loaded config (aliases,
//! trigger keywords, max_results, the hotkey chord the shell reads via
//! [`hotkey_chord`]), the script command snapshot, and the two ui.rs
//! callbacks, and maps query text to ranked rows and Return to a launch
//! (or a clipboard copy for calculator results and output-mode scripts).
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
use crate::{
    apps, files, hotkey, menubar, onboarding, panel, paste, pasteboard, plugins, scriptcmd,
    switcher, system, theme, ui, winmgmt,
};
use beckon_core::config::{self, Config};
use beckon_core::frecency::FrecencyStore;
use beckon_core::persist;
use beckon_core::quicklinks::{self, QuicklinkStore};
use beckon_core::router::{self, Item, QueryIntent};
use beckon_core::snippets::{self, ExpandContext, SnippetStore};
use beckon_core::{calc, devutil, emoji};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The frecency store file, under the beckon store root.
const FRECENCY_FILE: &str = "frecency.txt";

/// Snippet and quicklink store files, under the beckon store root.
const SNIPPETS_FILE: &str = "snippets.json";
const QUICKLINKS_FILE: &str = "quicklinks.json";

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
    /// Copy a clipboard history entry back to the pasteboard, then paste
    /// it into the frontmost app (pasteboard::activate bumps its own
    /// recency; no frecency record).
    Clip { id: String },
    /// Focus another app's window via the switcher.
    Window { id: String },
    /// Open a file search hit.
    File { id: String },
    /// Press a menu item of the frontmost app.
    Menu { id: String },
    /// Paste an emoji or symbol glyph.
    Emoji { glyph: String },
    /// Expand a snippet and paste the expansion.
    Snippet { id: u64 },
    /// Open a filled quicklink URL in the default browser.
    Link { id: u64, url: String },
    /// Run a script command via scriptcmd::activate; output-mode stdout
    /// lands on the clipboard.
    Script { id: String },
    /// Ask a plugin what to do; its action maps onto the existing copy,
    /// paste, and open paths.
    Plugin { id: String },
}

struct Engine {
    /// The loaded config file (or the defaults when absent or corrupt).
    config: Config,
    /// App index snapshot; refreshed on every summon.
    apps: Vec<Item>,
    /// Static command pool (system commands plus window actions),
    /// computed once at init; both sources are deterministic.
    commands: Vec<Item>,
    /// Script command snapshot; refreshed on every summon (the scriptcmd
    /// cache is mtime-based, so a refresh is one directory stat).
    scripts: Vec<Item>,
    /// Script `@beckon.keyword` annotations as implicit aliases:
    /// keyword -> `script.<file name>` id. Refreshed with `scripts`.
    script_keywords: BTreeMap<String, String>,
    /// Plugin trigger keywords (keyword -> plugin name), discovered once
    /// at init; consulted after the config trigger table so a user
    /// rename can never be shadowed by a plugin.
    plugin_keywords: BTreeMap<String, String>,
    frecency: FrecencyStore,
    frecency_path: PathBuf,
    snippets: SnippetStore,
    snippets_path: PathBuf,
    quicklinks: QuicklinkStore,
    quicklinks_path: PathBuf,
    /// Activation targets for the rows currently in the table.
    results: Vec<Entry>,
}

static ENGINE: OnceLock<Mutex<Engine>> = OnceLock::new();

/// The activation entry for one ranked-pool item: apps launch, scripts
/// run, everything else dispatches as a command id.
fn entry_for(item: &Item) -> Entry {
    match item.kind {
        router::ItemKind::App => Entry::App {
            id: item.id.clone(),
            path: item.subtitle.clone(),
        },
        router::ItemKind::Script => Entry::Script {
            id: item.id.clone(),
        },
        _ => Entry::Command {
            id: item.id.clone(),
        },
    }
}

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

/// One pass over the script commands: the registry items for the ranked
/// pool, plus the `@beckon.keyword` annotations as an implicit alias map
/// (keyword -> `script.<file name>`). Both calls serve from the same
/// mtime cache, so this is cheap enough to run on every summon.
fn script_snapshot() -> (Vec<Item>, BTreeMap<String, String>) {
    let items = scriptcmd::items();
    let keywords = scriptcmd::scripts()
        .into_iter()
        .filter_map(|s| s.keyword.map(|k| (k, format!("script.{}", s.file_name))))
        .collect();
    (items, keywords)
}

/// The summon chord from the loaded config, as the shell's hotkey
/// registration wants it: the ANSI virtual keycode, the Carbon modifier
/// mask, and a human-readable label for the startup log. Falls back to
/// the built-in Option+Space pieces (hotkey::KEY_CODE, hotkey::MODIFIERS)
/// if a hand-built config ever slips past validation. Call after init().
pub fn hotkey_chord() -> (u32, u32, String) {
    let eng = engine().lock().unwrap();
    let hk = &eng.config.hotkey;
    let key_code = config::keycode_for(&hk.key)
        .map(u32::from)
        .unwrap_or(hotkey::KEY_CODE);
    let names: Vec<&str> = hk.modifiers.iter().map(String::as_str).collect();
    let mut mask = hotkey::carbon_modifiers(&names);
    if mask == 0 {
        // Zero modifiers would swallow a plain keypress system-wide;
        // config::parse refuses that, so this is belt and suspenders.
        mask = hotkey::MODIFIERS;
    }
    (key_code, mask, chord_label(hk))
}

/// Human-readable chord label for the startup log, e.g. "Option+Space".
fn chord_label(hk: &config::HotkeyConfig) -> String {
    let mut parts: Vec<String> = hk
        .modifiers
        .iter()
        .map(|m| {
            match m.as_str() {
                "cmd" => "Cmd",
                "opt" => "Option",
                "ctrl" => "Ctrl",
                "shift" => "Shift",
                other => other,
            }
            .to_string()
        })
        .collect();
    let key = match hk.key.chars().next() {
        Some(c) => c.to_ascii_uppercase().to_string() + &hk.key[c.len_utf8()..],
        None => String::new(),
    };
    parts.push(key);
    parts.join("+")
}

/// Outcome of the alias pass over a query (see [`alias_outcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AliasOutcome {
    /// The alias target names an item id (it contains '.'): surface that
    /// exact item as the top row.
    Item(String),
    /// The alias target is plain text: the full rewritten query.
    Rewrite(String),
    /// No alias matched the first token.
    Miss,
}

/// The alias pass, run before triggers and intent parsing. Semantics:
/// the first whitespace-delimited token of the trimmed query is matched
/// exactly (case-sensitive, as stored) against the config `aliases`
/// first, then against script `@beckon.keyword` annotations. A target
/// containing '.' names an item id: that exact item becomes the top row
/// and the rest of the query is ignored (the alias names one command).
/// Any other target rewrites the first token in place, and the rewritten
/// query flows through the normal pipeline, so an alias may point at a
/// trigger keyword ("v" -> "clip"). One substitution per query: rewrites
/// are never re-aliased, so aliases cannot loop.
fn alias_outcome(
    aliases: &BTreeMap<String, String>,
    script_keywords: &BTreeMap<String, String>,
    trimmed: &str,
) -> AliasOutcome {
    let Some(first) = trimmed.split_whitespace().next() else {
        return AliasOutcome::Miss;
    };
    let Some(target) = aliases.get(first).or_else(|| script_keywords.get(first)) else {
        return AliasOutcome::Miss;
    };
    if target.contains('.') {
        return AliasOutcome::Item(target.clone());
    }
    let rest = trimmed[first.len()..].trim();
    if rest.is_empty() {
        AliasOutcome::Rewrite(target.clone())
    } else {
        AliasOutcome::Rewrite(format!("{target} {rest}"))
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
    // The config file mirrors the other stores: missing means defaults,
    // corrupt or invalid logs and defaults (a bad config must not take
    // the launcher down).
    let config_path = root.join(config::CONFIG_FILE);
    let config = match config::load(&config_path) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => Config::default(),
        Err(e) => {
            eprintln!(
                "beckon: cannot load config {}: {e}; using defaults",
                config_path.display()
            );
            Config::default()
        }
    };
    // Restyle the panel and record the row style for ui.rs. engine::init
    // runs after panel::init (shell::run order), so this lands before the
    // panel's first show; the shell needs no config getter for theming.
    theme::apply(&theme::Theme::from_config(&config));

    // Script commands: bootstrap the directory (README plus hello.sh) on
    // first run, then snapshot the discovered scripts for the ranked pool.
    if let Err(e) = scriptcmd::ensure_dir_with_example() {
        eprintln!("beckon: cannot bootstrap the scripts directory: {e}");
    }
    let (scripts, script_keywords) = script_snapshot();

    // Plugins: discovery only (each plugin process spawns on first use);
    // their trigger keywords sit behind the config table in keyword_rows.
    plugins::start();
    let plugin_keywords: BTreeMap<String, String> = plugins::keywords().into_iter().collect();

    let frecency_path = root.join(FRECENCY_FILE);
    let mut frecency = load_frecency(&frecency_path);
    // Entries decayed to zero are dead weight; drop them at the shell
    // edge so the file stays small. Scores are unchanged (they were 0).
    frecency.prune(now_secs());

    let snippets_path = root.join(SNIPPETS_FILE);
    let snippets = match SnippetStore::load(&snippets_path) {
        Ok(Some(store)) => store,
        Ok(None) => snippets::defaults(),
        Err(e) => {
            eprintln!(
                "beckon: corrupt snippet store {}: {e:?}; using defaults",
                snippets_path.display()
            );
            snippets::defaults()
        }
    };
    let quicklinks_path = root.join(QUICKLINKS_FILE);
    let quicklinks = match QuicklinkStore::load(&quicklinks_path) {
        Ok(Some(store)) => store,
        Ok(None) => quicklinks::defaults(),
        Err(e) => {
            eprintln!(
                "beckon: corrupt quicklink store {}: {e:?}; using defaults",
                quicklinks_path.display()
            );
            quicklinks::defaults()
        }
    };

    let mut commands = system::items();
    commands.extend(winmgmt::items());
    // Present only while Accessibility is not granted; self-retires on
    // the next launch after the grant. The status line makes the current
    // world visible in the startup log either way.
    commands.extend(onboarding::items());
    println!("beckon: {}", onboarding::status_line());

    if ENGINE
        .set(Mutex::new(Engine {
            config,
            apps,
            commands,
            scripts,
            script_keywords,
            plugin_keywords,
            frecency,
            frecency_path,
            snippets,
            snippets_path,
            quicklinks,
            quicklinks_path,
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
    // The file index builds on a background thread; items() serves
    // partial snapshots until the walk lands.
    files::start();
}

/// Hotkey entry: refresh the app index (measured cheap; module docs),
/// clear the query, show the default frecency list, and bring the panel
/// up with the caret ready. Programmatic setStringValue: does not fire
/// the query callback, so the pipeline is run explicitly.
pub fn summon() {
    let fresh = apps::index();
    // Scripts refresh here too: the scriptcmd cache is mtime-based, so
    // the steady-state cost is one directory stat per show.
    let (scripts, script_keywords) = script_snapshot();
    {
        let mut eng = engine().lock().unwrap();
        eng.apps = fresh;
        eng.scripts = scripts;
        eng.script_keywords = script_keywords;
    }
    // Kick a background re-walk so file results track the disk; the call
    // coalesces if a walk is already running.
    files::refresh();
    // The panel is non-activating, so the app the user is in stays
    // frontmost: snapshot its menu bar now, once per show, so the "menu"
    // trigger serves from cache at typing speed.
    menubar::snapshot_frontmost();
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
    /// entry per row. The alias pass ([`alias_outcome`]) runs first and
    /// takes precedence over trigger keywords; the rewritten query (or
    /// the original on a miss) then runs the normal pipeline.
    fn refresh_results(&mut self, raw: &str) -> Vec<ui::RowData> {
        let now = now_secs();
        let trimmed = raw.trim();
        match alias_outcome(&self.config.aliases, &self.script_keywords, trimmed) {
            AliasOutcome::Item(id) => {
                if let Some(rows) = self.alias_item_rows(&id) {
                    return rows;
                }
                // A stale alias target (the command is gone): fall
                // through with the query unchanged rather than showing
                // an empty panel.
            }
            AliasOutcome::Rewrite(q) => return self.pipeline_rows(&q, now),
            AliasOutcome::Miss => {}
        }
        self.pipeline_rows(trimmed, now)
    }

    /// The post-alias pipeline: trigger keywords, developer transforms,
    /// then intent parsing (calculator or ranked search).
    fn pipeline_rows(&mut self, raw: &str, now: u64) -> Vec<ui::RowData> {
        // Keyword-triggered sources ride outside the fuzzy pool: their
        // rows are arbitrary text (clipboard history, window titles,
        // file paths) and would pollute app ranking. First word chooses
        // the source through the config trigger table, the rest of the
        // query searches inside it.
        let trimmed = raw.trim();
        if let Some(first) = trimmed.split_whitespace().next() {
            let rest = trimmed[first.len()..].trim();
            if let Some(rows) = self.keyword_rows(&first.to_lowercase(), rest) {
                return rows;
            }
        }
        // Developer transforms answer launcher phrasings like "uuid",
        // "b64 hello", "sha256 abc"; unknown prefixes fall through to the
        // normal pipeline.
        // A malformed argument ("epoch banana") falls through to app
        // search rather than showing an error row.
        if let Some((util, arg)) = devutil::parse_command(trimmed) {
            if let Ok(output) = devutil::run(util, &arg, now, &urandom16()) {
                let row = ui::RowData {
                    title: output.clone(),
                    subtitle: format!("{trimmed} (press Return to copy)"),
                };
                self.results = vec![Entry::Calc { display: output }];
                return vec![row];
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

    /// Route a trigger keyword through the config trigger table to its
    /// source rows. None means the first word is not a trigger keyword
    /// and the query belongs to the ranked pipeline. `keyword` arrives
    /// lowercased; the table's keywords are stored lowercase, so lookup
    /// stays case-insensitive like the old hardcoded arms.
    fn keyword_rows(&mut self, keyword: &str, rest: &str) -> Option<Vec<ui::RowData>> {
        // Clone the canonical name to end the borrow of self.config
        // before the &mut self source calls below.
        let Some(source) = self.config.triggers.get(keyword).cloned() else {
            // Not a built-in trigger; plugins get the keyword next, so a
            // user rename in config always wins over a plugin.
            if self.plugin_keywords.contains_key(keyword) {
                return Some(self.plugin_rows(keyword, rest));
            }
            return None;
        };
        match source.as_str() {
            "clip" => Some(self.clip_rows(rest)),
            "win" => Some(self.window_rows(rest)),
            "file" => Some(self.file_rows(rest)),
            "menu" => Some(self.menu_rows(rest)),
            "emoji" => Some(self.emoji_rows(rest)),
            "snip" => Some(self.snippet_rows(rest)),
            "go" => Some(self.quicklink_rows(rest)),
            // config::parse validates targets against TRIGGER_SOURCES,
            // so this arm is unreachable for any loaded config; fall
            // through to search rather than panicking on a hand-built one.
            _ => None,
        }
    }

    /// Rows from a plugin trigger: the plugin answers beckon.query over
    /// its stdio pipe; activation asks it for an action.
    fn plugin_rows(&mut self, keyword: &str, rest: &str) -> Vec<ui::RowData> {
        let items = plugins::query(keyword, rest);
        self.trigger_rows(items, |id| Entry::Plugin { id })
    }

    /// The single row for an alias that names an item id: the exact id
    /// is looked up across the command, app, and script pools. None when
    /// the id resolves to nothing (a stale alias).
    fn alias_item_rows(&mut self, id: &str) -> Option<Vec<ui::RowData>> {
        let item = self
            .commands
            .iter()
            .chain(self.apps.iter())
            .chain(self.scripts.iter())
            .find(|i| i.id == id)?
            .clone();
        self.results = vec![entry_for(&item)];
        Some(vec![ui::RowData {
            title: item.title,
            subtitle: item.subtitle,
        }])
    }

    /// Rank apps, commands, and scripts together against `query` and
    /// keep the top rows. The pool is rebuilt per keystroke; at ~130
    /// items the clone cost is noise next to the AppKit reload that
    /// follows.
    fn search_rows(&mut self, query: &str, now: u64) -> Vec<ui::RowData> {
        let mut pool = self.apps.clone();
        pool.extend(self.commands.iter().cloned());
        pool.extend(self.scripts.iter().cloned());
        let ranked = router::rank(query, &pool, &self.frecency, now);
        self.results.clear();
        let mut rows = Vec::new();
        for r in ranked.into_iter().take(self.config.max_results) {
            self.results.push(entry_for(&r.item));
            rows.push(ui::RowData {
                title: r.item.title,
                // App subtitles are absolute bundle paths; command
                // subtitles are one-line descriptions.
                subtitle: r.item.subtitle,
            });
        }
        rows
    }

    /// Rows for the clipboard history trigger: recent entries for an
    /// empty search, ClipStore search otherwise; pasteboard::items owns
    /// shaping and the cap.
    fn clip_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let items = pasteboard::items(query);
        self.trigger_rows(items, |id| Entry::Clip { id })
    }

    /// Rows for the window switcher trigger: every layer-0 window,
    /// frontmost app first; the rest of the query filters.
    fn window_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let items = switcher::items(query);
        self.trigger_rows(items, |id| Entry::Window { id })
    }

    /// Rows for the file search trigger: local index ranked by the core
    /// fuzzy scorer, Spotlight topping up thin results. Empty search
    /// shows nothing by design (a file list without a query is noise).
    fn file_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let items = files::items(query);
        let mut rows = self.trigger_rows(items, |id| Entry::File { id });
        // A miss against a still-building index gets an explanation
        // instead of silence. The hint row has no activation entry;
        // handle_activate treats the missing entry as a no-op.
        if rows.is_empty() && !query.is_empty() && !files::ready() {
            rows.push(ui::RowData {
                title: "Indexing files...".to_string(),
                subtitle: "results appear as the walk finishes".to_string(),
            });
        }
        rows
    }

    /// Rows for the menu trigger: the frontmost app's menu items from the
    /// summon-time snapshot, breadcrumb subtitles, AXPress on Return.
    fn menu_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let items = menubar::items(query);
        self.trigger_rows(items, |id| Entry::Menu { id })
    }

    /// Rows for the emoji trigger: the curated table, activation pastes
    /// the glyph.
    fn emoji_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let hits = emoji::search(query, self.config.max_results);
        self.results.clear();
        let mut rows = Vec::new();
        for e in hits {
            rows.push(ui::RowData {
                title: format!("{} {}", e.glyph, e.name),
                subtitle: e.keywords.to_string(),
            });
            self.results.push(Entry::Emoji {
                glyph: e.glyph.to_string(),
            });
        }
        rows
    }

    /// Rows for the snippet trigger: name and keyword matches; the body
    /// preview rides in the subtitle. Activation expands and pastes.
    fn snippet_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let hits: Vec<(u64, String, String)> = self
            .snippets
            .search(query)
            .into_iter()
            .take(self.config.max_results)
            .map(|s| {
                let head: String = s.body.chars().take(40).collect();
                (s.id, s.name.clone(), format!("{} · {head}", s.keyword))
            })
            .collect();
        self.results.clear();
        let mut rows = Vec::new();
        for (id, name, subtitle) in hits {
            rows.push(ui::RowData {
                title: name,
                subtitle,
            });
            self.results.push(Entry::Snippet { id });
        }
        rows
    }

    /// Rows for the quicklink trigger: "go <name> <query...>", first word
    /// picks the link, the rest fills {query}. Activation opens the URL.
    fn quicklink_rows(&mut self, query: &str) -> Vec<ui::RowData> {
        let (name, fill_query) = match query.split_once(char::is_whitespace) {
            Some((n, rest)) => (n, rest.trim()),
            None => (query, ""),
        };
        let hits: Vec<(u64, String, String)> = self
            .quicklinks
            .search(name)
            .into_iter()
            .take(self.config.max_results)
            .map(|q| {
                (
                    q.id,
                    q.name.clone(),
                    quicklinks::fill(&q.template, fill_query),
                )
            })
            .collect();
        self.results.clear();
        let mut rows = Vec::new();
        for (id, name, url) in hits {
            rows.push(ui::RowData {
                title: name,
                subtitle: url.clone(),
            });
            self.results.push(Entry::Link { id, url });
        }
        rows
    }

    /// Shared shaping for keyword-triggered sources: one row and one
    /// activation entry per item, in the source's own order.
    fn trigger_rows(
        &mut self,
        items: Vec<Item>,
        entry: impl Fn(String) -> Entry,
    ) -> Vec<ui::RowData> {
        self.results.clear();
        let mut rows = Vec::new();
        for item in items {
            rows.push(ui::RowData {
                title: item.title,
                subtitle: item.subtitle,
            });
            self.results.push(entry(item.id));
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
            } else if id.starts_with("onboarding.") {
                onboarding::activate(&id)
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
            Ok(()) => {
                // The entry is on the pasteboard; hide first (the panel
                // is non-activating, so the target app already holds key
                // focus), then synthesize Cmd+V. Without the
                // Accessibility grant the paste refuses cleanly and the
                // text stays on the clipboard for a manual paste.
                dismiss();
                if let Err(e) = paste::paste_to_frontmost() {
                    eprintln!("beckon: paste skipped: {e}");
                }
            }
            Err(e) => eprintln!("beckon: clipboard copy failed: {e}"),
        },
        Entry::Window { id } => match switcher::activate(&id) {
            Ok(()) => dismiss(),
            Err(e) => eprintln!("beckon: window focus failed: {e}"),
        },
        Entry::File { id } => match files::activate(&id) {
            Ok(()) => dismiss(),
            Err(e) => eprintln!("beckon: open failed: {e}"),
        },
        Entry::Menu { id } => match menubar::activate(&id) {
            Ok(()) => dismiss(),
            Err(e) => eprintln!("beckon: menu press failed: {e}"),
        },
        Entry::Emoji { glyph } => copy_then_paste(&glyph),
        Entry::Snippet { id } => {
            let expanded = {
                let mut eng = engine().lock().unwrap();
                let Some(snippet) = eng.snippets.get(id) else {
                    eprintln!("beckon: snippet {id} vanished");
                    return;
                };
                let ctx = ExpandContext {
                    now_secs: now_secs(),
                    clipboard: read_clipboard(),
                };
                let expanded = match snippets::expand(&snippet.body, &ctx) {
                    Ok(ex) => ex,
                    Err(e) => {
                        eprintln!("beckon: snippet expansion failed: {e:?}");
                        return;
                    }
                };
                // Cursor placement needs post-paste key synthesis; until
                // that lands the marker is simply removed. Documented in
                // the snippets module grammar.
                eng.snippets.record_use(id, now_secs());
                let path = eng.snippets_path.clone();
                if let Err(e) = eng.snippets.save(&path) {
                    eprintln!("beckon: snippet store save failed: {e:?}");
                }
                expanded
            };
            copy_then_paste(&expanded.text);
        }
        Entry::Script { id } => match scriptcmd::activate(&id) {
            // Silent mode: Ok carries the empty string, nothing to show.
            Ok(output) if output.is_empty() => {
                record_use_and_save(&id);
                dismiss();
            }
            // Output mode: the script's trimmed stdout lands on the
            // clipboard (the panel hides, so a row cannot show it).
            Ok(output) => {
                if copy_to_clipboard(&output) {
                    pasteboard::note_own_write();
                    record_use_and_save(&id);
                    dismiss();
                } else {
                    eprintln!("beckon: pasteboard write failed");
                }
            }
            // Failure keeps the panel up so the error context survives.
            Err(e) => eprintln!("beckon: script {id} failed: {e}"),
        },
        Entry::Plugin { id } => match plugins::activate(&id) {
            Ok(plugins::PluginAction::None) => dismiss(),
            Ok(plugins::PluginAction::Copy(text)) => {
                if copy_to_clipboard(&text) {
                    pasteboard::note_own_write();
                    dismiss();
                } else {
                    eprintln!("beckon: pasteboard write failed");
                }
            }
            Ok(plugins::PluginAction::Paste(text)) => copy_then_paste(&text),
            Ok(plugins::PluginAction::Open(url)) => {
                if open_url(&url) {
                    dismiss();
                } else {
                    eprintln!("beckon: could not open {url}");
                }
            }
            Err(e) => eprintln!("beckon: plugin activation failed: {e}"),
        },
        Entry::Link { id, url } => {
            if open_url(&url) {
                let mut eng = engine().lock().unwrap();
                eng.quicklinks.record_use(id);
                let path = eng.quicklinks_path.clone();
                if let Err(e) = eng.quicklinks.save(&path) {
                    eprintln!("beckon: quicklink store save failed: {e:?}");
                }
                drop(eng);
                dismiss();
            } else {
                eprintln!("beckon: could not open {url}");
            }
        }
    }
}

/// Copy `text` to the pasteboard and paste it into the frontmost app:
/// the shared activation path for emoji and snippets. The panel hides
/// first (it is non-activating, so the target app already holds key
/// focus); without the Accessibility grant the paste refuses cleanly and
/// the text stays on the clipboard.
fn copy_then_paste(text: &str) {
    if copy_to_clipboard(text) {
        pasteboard::note_own_write();
        dismiss();
        if let Err(e) = paste::paste_to_frontmost() {
            eprintln!("beckon: paste skipped: {e}");
        }
    } else {
        eprintln!("beckon: pasteboard write failed");
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

/// Read the general pasteboard's plain-text contents, for snippet
/// {clipboard} expansion. None when the pasteboard holds no string.
fn read_clipboard() -> Option<String> {
    // Safety: main thread; stringForType: returns an NSString or nil and
    // nsstring_to_string accepts both (nil becomes the empty string).
    unsafe {
        let pb = msg!(Id: ffi::class("NSPasteboard"), ffi::sel("generalPasteboard"));
        let ns = msg!(Id: pb, ffi::sel("stringForType:"),
            Id: ffi::nsstring("public.utf8-plain-text"));
        if ns == ffi::NIL {
            None
        } else {
            Some(ffi::nsstring_to_string(ns))
        }
    }
}

/// Open a URL with the user's default handler via NSWorkspace. This is
/// user-initiated navigation in the browser, not a network call by the
/// beckon binary itself, so the airgap posture holds.
fn open_url(url: &str) -> bool {
    // Safety: main thread; URLWithString: returns an NSURL or nil;
    // openURL: takes an NSURL and returns BOOL.
    unsafe {
        let nsurl = msg!(Id: ffi::class("NSURL"), ffi::sel("URLWithString:"),
            Id: ffi::nsstring(url));
        if nsurl == ffi::NIL {
            return false;
        }
        let ws = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
        msg!(Bool: ws, ffi::sel("openURL:"), Id: nsurl) != 0
    }
}

/// Sixteen bytes of OS entropy for the uuid utility, read from
/// /dev/urandom (plain std::fs, airgap clean). A read failure degrades
/// to a time-and-pid mix rather than an error; the uuid utility is a
/// convenience, not a security boundary, as its docs state.
fn urandom16() -> [u8; 16] {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let mix = nanos ^ ((std::process::id() as u64) << 32) ^ now_secs();
        buf[..8].copy_from_slice(&mix.to_le_bytes());
        buf[8..].copy_from_slice(&mix.rotate_left(29).to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn alias_to_id_surfaces_the_item_and_ignores_the_rest() {
        let aliases = map(&[("lh", "window.left-half")]);
        let none = BTreeMap::new();
        assert_eq!(
            alias_outcome(&aliases, &none, "lh"),
            AliasOutcome::Item("window.left-half".to_string())
        );
        // The rest of the query is ignored: the alias names one command.
        assert_eq!(
            alias_outcome(&aliases, &none, "lh whatever else"),
            AliasOutcome::Item("window.left-half".to_string())
        );
    }

    #[test]
    fn alias_to_text_rewrites_the_first_token() {
        let aliases = map(&[("v", "clip"), ("g", "go google")]);
        let none = BTreeMap::new();
        assert_eq!(
            alias_outcome(&aliases, &none, "v"),
            AliasOutcome::Rewrite("clip".to_string())
        );
        assert_eq!(
            alias_outcome(&aliases, &none, "v search text"),
            AliasOutcome::Rewrite("clip search text".to_string())
        );
        // Multi-word targets keep the rest appended.
        assert_eq!(
            alias_outcome(&aliases, &none, "g rust"),
            AliasOutcome::Rewrite("go google rust".to_string())
        );
    }

    #[test]
    fn alias_misses_are_exact_and_first_token_only() {
        let aliases = map(&[("lh", "window.left-half")]);
        let none = BTreeMap::new();
        // Case-sensitive, whole-token, first-token-only, empty query.
        assert_eq!(alias_outcome(&aliases, &none, "LH"), AliasOutcome::Miss);
        assert_eq!(alias_outcome(&aliases, &none, "lhx"), AliasOutcome::Miss);
        assert_eq!(alias_outcome(&aliases, &none, "x lh"), AliasOutcome::Miss);
        assert_eq!(alias_outcome(&aliases, &none, ""), AliasOutcome::Miss);
        assert_eq!(alias_outcome(&aliases, &none, "   "), AliasOutcome::Miss);
    }

    #[test]
    fn script_keywords_act_as_aliases_and_config_aliases_win() {
        let aliases = map(&[("deploy", "system.lock")]);
        let keywords = map(&[("deploy", "script.deploy.sh"), ("hi", "script.hello.sh")]);
        // The config alias shadows the script keyword.
        assert_eq!(
            alias_outcome(&aliases, &keywords, "deploy"),
            AliasOutcome::Item("system.lock".to_string())
        );
        // An unshadowed keyword resolves to its script id.
        assert_eq!(
            alias_outcome(&aliases, &keywords, "hi"),
            AliasOutcome::Item("script.hello.sh".to_string())
        );
    }

    #[test]
    fn chord_labels_read_like_the_startup_log() {
        let default = config::Config::default();
        assert_eq!(chord_label(&default.hotkey), "Option+Space");
        let custom = config::HotkeyConfig {
            key: "k".to_string(),
            modifiers: vec!["cmd".to_string(), "shift".to_string()],
        };
        assert_eq!(chord_label(&custom), "Cmd+Shift+K");
        let fkey = config::HotkeyConfig {
            key: "f12".to_string(),
            modifiers: vec!["ctrl".to_string()],
        };
        assert_eq!(chord_label(&fkey), "Ctrl+F12");
    }
}
