//! Script commands: drop an annotated executable into
//! `~/.beckon/scripts/` and it becomes a launcher command, the way
//! Raycast script commands work.
//!
//! # Discovery
//!
//! [`items`] scans `<store_root>/scripts` (so `BECKON_HOME` relocates it,
//! see `beckon_core::persist::store_root`) for regular files with any
//! executable bit set. Hidden files (leading dot) are skipped silently.
//! A plain file without an exec bit is skipped with a one-time stderr
//! hint naming the file and the fix (`chmod +x`), except documentation
//! files (`.md`, `.txt`), which are expected to live here (the bootstrap
//! writes a README) and are skipped without noise. Symlinks are
//! followed: a symlink to an executable regular file counts.
//!
//! The scan is cached per process and invalidated by the directory's
//! mtime: adding, removing, or renaming a script re-scans on the next
//! call. Editing a script's annotation header in place does NOT change
//! the directory mtime, so the cached title and mode stay stale until
//! the directory itself changes or the process restarts; that is the
//! documented cost of the cheap invalidation check (one stat per query).
//!
//! # Annotation grammar
//!
//! The first 30 lines (within the first 8 KiB, decoded lossily as UTF-8)
//! of each script are scanned for annotation lines:
//!
//! ```text
//! line   = ws* marker ws* "@beckon." key ws value
//! marker = "#"+ | "//" "/"* | "--" "-"*
//! key    = "title" | "subtitle" | "mode" | "keyword"
//! value  = rest of the line, trimmed; an empty value means absent
//! ```
//!
//! This is the Raycast convention adapted: `#`, `//`, and `--` cover
//! shell, Python, Ruby, JavaScript, Swift, Lua, SQL, and Haskell. The
//! first valid occurrence of each key wins; later duplicates, unknown
//! keys, and unknown `mode` values are ignored. `mode` is `silent`
//! (default: run quietly, Ok carries the empty string) or `output`
//! (Ok carries trimmed stdout for the integrator to show as a row or
//! copy). `title` defaults to the file name minus its final extension.
//! `subtitle` and `keyword` are parsed and carried on [`Script`];
//! `keyword` acts as an implicit alias in the engine's alias pass
//! (typing the keyword surfaces the script as the top row, with config
//! aliases taking precedence). The [`Item`]
//! subtitle itself is always the ~-abbreviated path plus the mode, so
//! every row says what file runs and how.
//!
//! # Execution
//!
//! [`activate`] runs the script directly with `std::process::Command`:
//! no shell wraps it (the kernel honors the shebang), so there is no
//! interpolation surface. The working directory is the scripts dir,
//! stdin is null (a script that reads stdin sees EOF instead of
//! inheriting beckon's), and stdout and stderr are captured on drain
//! threads. The child gets `DEFAULT_TIMEOUT_MS` (10 seconds) to exit;
//! past that it is killed, reaped, and reported as an Err. The same
//! deadline bounds output collection, so a background grandchild
//! holding the pipes open cannot wedge the launcher either. Tests pass
//! a short timeout through `activate_in`/`run_script` so the sleeping
//! fixture resolves in milliseconds, not seconds.
//!
//! A nonzero exit becomes Err carrying the script's stderr (or the exit
//! status when stderr is empty). Unknown ids are Err, never a panic.
//!
//! # Bootstrap
//!
//! [`ensure_dir_with_example`] makes the feature discoverable: if the
//! scripts directory does not exist yet, it is created with a README
//! documenting this grammar plus one annotated executable example
//! (`hello.sh`). An existing directory is never touched.

use beckon_core::persist;
use beckon_core::router::{Item, ItemKind};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How many leading lines of a script are searched for annotations.
const ANNOTATION_LINES: usize = 30;

/// How many leading bytes of a script are read for the annotation scan.
/// Bounds the read for large binaries; 30 annotation lines fit easily.
const HEADER_BYTES: u64 = 8 * 1024;

/// How long a script may run before it is killed, in milliseconds.
/// Tests use `run_script`/`activate_in` with a short value instead of
/// waiting this out.
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Poll interval for the wait-with-timeout loop, in milliseconds.
const POLL_MS: u64 = 20;

/// What activate does with the script's stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Run quietly; success returns the empty string.
    Silent,
    /// Success returns trimmed stdout for the launcher to show or copy.
    Output,
}

impl Mode {
    /// The annotation spelling, used in subtitles.
    fn label(self) -> &'static str {
        match self {
            Mode::Silent => "silent",
            Mode::Output => "output",
        }
    }
}

/// One discovered script with its parsed annotations. `subtitle` and
/// `keyword` ride here for the integrator (the alias pass); the registry
/// [`Item`] carries the path-plus-mode subtitle instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// File name inside the scripts dir; the id is `script.<file_name>`.
    pub file_name: String,
    /// Absolute path to the executable.
    pub path: PathBuf,
    /// Annotation title, or the file name stem when unannotated.
    pub title: String,
    /// `@beckon.subtitle`, if present.
    // Parsed and carried for a future richer row rendering; only tests
    // read it today (the Item subtitle is the path plus the mode).
    #[allow(dead_code)]
    pub subtitle: Option<String>,
    /// `@beckon.mode`, defaulting to silent.
    pub mode: Mode,
    /// `@beckon.keyword`, if present; the engine treats it as an
    /// implicit alias to this script's id.
    pub keyword: Option<String>,
}

/// Raw parse result of an annotation header; all fields optional.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Ann {
    title: Option<String>,
    subtitle: Option<String>,
    mode: Option<Mode>,
    keyword: Option<String>,
}

/// The scripts directory under the store root (`BECKON_HOME` aware).
fn scripts_dir() -> PathBuf {
    persist::store_root().join("scripts")
}

/// Strip one comment marker (`#`, `//`, or `--`, with repeats of the
/// marker character tolerated, so `##` and `---` work) from the start of
/// a line. None means the line is not a comment in any known style.
fn strip_comment_marker(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("//") {
        return Some(rest.trim_start_matches('/'));
    }
    if let Some(rest) = t.strip_prefix("--") {
        return Some(rest.trim_start_matches('-'));
    }
    t.strip_prefix('#').map(|rest| rest.trim_start_matches('#'))
}

/// Parse the annotation grammar out of a script header. First valid
/// occurrence of each key wins; unknown keys, unknown mode values, and
/// empty values are ignored.
fn parse_annotations(text: &str) -> Ann {
    let mut ann = Ann::default();
    for line in text.lines().take(ANNOTATION_LINES) {
        let Some(rest) = strip_comment_marker(line) else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix("@beckon.") else {
            continue;
        };
        let (key, value) = match rest.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (rest, ""),
        };
        if value.is_empty() {
            continue;
        }
        match key {
            "title" if ann.title.is_none() => ann.title = Some(value.to_string()),
            "subtitle" if ann.subtitle.is_none() => ann.subtitle = Some(value.to_string()),
            "keyword" if ann.keyword.is_none() => ann.keyword = Some(value.to_string()),
            "mode" if ann.mode.is_none() => {
                ann.mode = match value {
                    "silent" => Some(Mode::Silent),
                    "output" => Some(Mode::Output),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    ann
}

/// Read and parse the annotation header of one file. Reads at most
/// [`HEADER_BYTES`]; an unreadable file parses as unannotated.
fn read_annotations(path: &Path) -> Ann {
    let mut head = Vec::new();
    if let Ok(file) = fs::File::open(path) {
        let _ = file.take(HEADER_BYTES).read_to_end(&mut head);
    }
    parse_annotations(&String::from_utf8_lossy(&head))
}

/// Scan `dir` for script commands. Missing dir means no scripts. The
/// `warned` set makes the not-executable hint fire once per path per
/// process. Output is sorted by title then file name, which matches the
/// Item ordering contract (title then id) because the id is a pure
/// function of the file name.
fn scan_dir(dir: &Path, warned: &mut BTreeSet<PathBuf>) -> Vec<Script> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // fs::metadata follows symlinks, so a symlink to an executable
        // regular file participates like the file itself.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.permissions().mode() & 0o111 == 0 {
            let lower = name.to_ascii_lowercase();
            let is_doc = lower.ends_with(".md") || lower.ends_with(".txt");
            if !is_doc && warned.insert(path.clone()) {
                eprintln!(
                    "beckon: scripts: {} is not executable; run chmod +x to enable it",
                    path.display()
                );
            }
            continue;
        }
        let ann = read_annotations(&path);
        let title = ann.title.unwrap_or_else(|| {
            Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(name)
                .to_string()
        });
        out.push(Script {
            file_name: name.to_string(),
            path,
            title,
            subtitle: ann.subtitle,
            mode: ann.mode.unwrap_or(Mode::Silent),
            keyword: ann.keyword,
        });
    }
    out.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    out
}

/// Per-process scan cache. `refresh` re-scans only when the scripts
/// directory itself (path or mtime) changed since the cached scan.
struct ScanCache {
    dir: PathBuf,
    dir_mtime: Option<SystemTime>,
    scripts: Vec<Script>,
    primed: bool,
    warned: BTreeSet<PathBuf>,
}

impl ScanCache {
    fn new() -> ScanCache {
        ScanCache {
            dir: PathBuf::new(),
            dir_mtime: None,
            scripts: Vec::new(),
            primed: false,
            warned: BTreeSet::new(),
        }
    }

    /// Return the scripts in `dir`, re-scanning only when the directory
    /// mtime (or the directory path itself) changed. A missing directory
    /// caches as empty with no mtime.
    fn refresh(&mut self, dir: &Path) -> &[Script] {
        let mtime = fs::metadata(dir).and_then(|m| m.modified()).ok();
        if !self.primed || self.dir != dir || self.dir_mtime != mtime {
            self.scripts = scan_dir(dir, &mut self.warned);
            self.dir = dir.to_path_buf();
            self.dir_mtime = mtime;
            self.primed = true;
        }
        &self.scripts
    }
}

/// The process-wide cache behind [`items`], [`scripts`], and
/// [`activate`]. Poisoning is shrugged off: the cache holds no
/// invariants a panic could break mid-update that a re-scan cannot fix.
fn cache() -> MutexGuard<'static, ScanCache> {
    static CACHE: OnceLock<Mutex<ScanCache>> = OnceLock::new();
    let mutex = CACHE.get_or_init(|| Mutex::new(ScanCache::new()));
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Replace a leading `$HOME` with `~` for display.
fn abbreviate(path: &Path) -> String {
    abbreviate_from(path, std::env::var_os("HOME"))
}

fn abbreviate_from(path: &Path, home: Option<OsString>) -> String {
    if let Some(home) = home {
        if !home.is_empty() {
            if let Ok(rest) = path.strip_prefix(Path::new(&home)) {
                if rest.as_os_str().is_empty() {
                    return "~".to_string();
                }
                return format!("~/{}", rest.display());
            }
        }
    }
    path.display().to_string()
}

/// One script as a registry [`Item`]: id `script.<file name>`, the
/// annotation (or fallback) title, and a subtitle of the ~-abbreviated
/// path plus the mode.
fn to_item(script: &Script) -> Item {
    Item::new(
        &format!("script.{}", script.file_name),
        &script.title,
        &format!("{} ({})", abbreviate(&script.path), script.mode.label()),
        ItemKind::Script,
    )
}

/// Every script command as a registry [`Item`], deterministically
/// ordered by title then id. Uses the mtime cache, so steady-state cost
/// is one directory stat.
pub fn items() -> Vec<Item> {
    let dir = scripts_dir();
    cache().refresh(&dir).iter().map(to_item).collect()
}

/// The discovered scripts with their full annotation metadata (subtitle
/// and keyword included), same order and cache as [`items`]. This is
/// the surface the integrator's alias pass reads keywords from.
pub fn scripts() -> Vec<Script> {
    let dir = scripts_dir();
    cache().refresh(&dir).to_vec()
}

/// Uncached scan-and-map, used by tests against fixture directories.
#[cfg(test)]
fn items_in(dir: &Path) -> Vec<Item> {
    let mut warned = BTreeSet::new();
    scan_dir(dir, &mut warned).iter().map(to_item).collect()
}

/// Resolve an item id back to its script. The scan is the real guard
/// (only names that were discovered resolve); the separator check just
/// rejects obviously hostile ids early.
fn lookup(scripts: &[Script], id: &str) -> Result<Script, String> {
    let Some(name) = id.strip_prefix("script.") else {
        return Err(format!("not a script id: {id}"));
    };
    if name.contains('/') {
        return Err(format!("invalid script id: {id}"));
    }
    scripts
        .iter()
        .find(|s| s.file_name == name)
        .cloned()
        .ok_or_else(|| format!("unknown script: {name}"))
}

/// Drain one child pipe to a lossy string on its own thread, so the
/// wait loop never deadlocks against a full pipe buffer.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Execute one script: direct exec (no shell), cwd = the scripts dir,
/// null stdin, piped stdout and stderr. The child must exit within
/// `timeout_ms` or it is killed, reaped, and reported as Err; the same
/// deadline bounds output collection so a lingering background
/// grandchild holding the pipes cannot block past it (its drain threads
/// are abandoned to finish on their own when the pipe closes).
fn run_script(script: &Script, dir: &Path, timeout_ms: u64) -> Result<String, String> {
    let mut child = Command::new(&script.path)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", script.path.display()))?;
    let out_thread = drain(child.stdout.take());
    let err_thread = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait failed for {}: {e}", script.file_name));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} timed out after {timeout_ms} ms and was killed",
                script.file_name
            ));
        }
        thread::sleep(Duration::from_millis(POLL_MS));
    };
    while !(out_thread.is_finished() && err_thread.is_finished()) {
        if Instant::now() >= deadline {
            return Err(format!(
                "{} exited but held its output pipes open past the timeout \
                 (a background child inherited them?)",
                script.file_name
            ));
        }
        thread::sleep(Duration::from_millis(POLL_MS));
    }
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    if !status.success() {
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("{} failed with {status}", script.file_name)
        } else {
            format!("{} failed with {status}: {stderr}", script.file_name)
        });
    }
    Ok(match script.mode {
        Mode::Output => stdout.trim().to_string(),
        Mode::Silent => String::new(),
    })
}

/// Run the script command `id` (`script.<file name>`) and report what
/// happened: Ok is trimmed stdout in `output` mode and the empty string
/// in `silent` mode; a nonzero exit, a timeout kill, or an unknown id
/// is an Err worth showing. See [`run_script`] for the execution rules.
pub fn activate(id: &str) -> Result<String, String> {
    let dir = scripts_dir();
    let script = lookup(cache().refresh(&dir), id)?;
    run_script(&script, &dir, DEFAULT_TIMEOUT_MS)
}

/// Test seam for [`activate`]: same lookup and execution, but against
/// an explicit directory, without the process-wide cache, and with a
/// caller-chosen timeout so the timeout test finishes in milliseconds.
#[cfg(test)]
fn activate_in(dir: &Path, id: &str, timeout_ms: u64) -> Result<String, String> {
    let mut warned = BTreeSet::new();
    let scripts = scan_dir(dir, &mut warned);
    let script = lookup(&scripts, id)?;
    run_script(&script, dir, timeout_ms)
}

/// The README the bootstrap drops next to the example script.
const README_MD: &str = r#"# beckon script commands

Every executable file in this directory becomes a beckon command.
Drop a script here, `chmod +x` it, and it shows up in the launcher.

## Annotations

beckon reads the first 30 lines (up to 8 KiB) of each script for
comment lines of the form:

    # @beckon.title    Deploy Blog
    # @beckon.subtitle Push the static site
    # @beckon.mode     output
    # @beckon.keyword  deploy

`#`, `//`, and `--` comment markers all work, so shell, Python, Ruby,
JavaScript, Lua, SQL, and Haskell scripts can all carry annotations.
The first valid occurrence of each key wins.

- `title`: the name shown in the launcher. Defaults to the file name
  without its extension.
- `subtitle`: an optional short description.
- `mode`: `silent` (the default) runs the script quietly; `output`
  shows the script's trimmed stdout in the launcher after it runs.
- `keyword`: an optional alias word for quick invocation.

## Rules

- Hidden files and non-executable files are ignored.
- Scripts run with this directory as the working directory, with no
  stdin, and are killed after 10 seconds.
- A nonzero exit shows the script's stderr as the error.
"#;

/// The annotated example script the bootstrap installs.
const HELLO_SH: &str = r#"#!/bin/sh
# @beckon.title Hello from beckon
# @beckon.subtitle Your first script command
# @beckon.mode output
echo "Hello, $(whoami)! Drop more scripts in this directory to add commands."
"#;

/// Create the scripts directory with a README and one annotated
/// example (`hello.sh`, mode 0755) so the feature is discoverable, but
/// ONLY if the directory does not exist yet: an existing directory is
/// never touched, so user edits and deletions of the examples stick.
/// Returns the directory path.
pub fn ensure_dir_with_example() -> io::Result<PathBuf> {
    ensure_dir_with_example_in(&scripts_dir())
}

fn ensure_dir_with_example_in(dir: &Path) -> io::Result<PathBuf> {
    // symlink_metadata: any existing entry (dir, file, even a dangling
    // symlink) means "exists, hands off".
    if dir.symlink_metadata().is_ok() {
        return Ok(dir.to_path_buf());
    }
    fs::create_dir_all(dir)?;
    fs::write(dir.join("README.md"), README_MD)?;
    let hello = dir.join("hello.sh");
    fs::write(&hello, HELLO_SH)?;
    fs::set_permissions(&hello, fs::Permissions::from_mode(0o755))?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Isolation rule for this module's tests: everything runs against
    // per-test temp directories through the *_in seams; no test reads
    // BECKON_HOME or HOME through the public entry points, so nothing
    // here can ever touch the real ~/.beckon.

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "beckon-scriptcmd-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn write_script(dir: &Path, name: &str, body: &str, exec: bool) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write script");
        let mode = if exec { 0o755 } else { 0o644 };
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    #[test]
    fn golden_hash_annotations() {
        let ann = parse_annotations(
            "#!/bin/sh\n\
             # @beckon.title Deploy Blog\n\
             ##  @beckon.subtitle Push the static site\n\
             # @beckon.mode output\n\
             # @beckon.keyword deploy\n\
             echo hi\n",
        );
        assert_eq!(ann.title.as_deref(), Some("Deploy Blog"));
        assert_eq!(ann.subtitle.as_deref(), Some("Push the static site"));
        assert_eq!(ann.mode, Some(Mode::Output));
        assert_eq!(ann.keyword.as_deref(), Some("deploy"));
    }

    #[test]
    fn golden_slash_annotations() {
        let ann = parse_annotations(
            "#!/usr/bin/env node\n\
             // @beckon.title Node Task\n\
             /// @beckon.subtitle Triple slash tolerated\n\
             //@beckon.mode output\n\
             // @beckon.keyword node\n",
        );
        assert_eq!(ann.title.as_deref(), Some("Node Task"));
        assert_eq!(ann.subtitle.as_deref(), Some("Triple slash tolerated"));
        assert_eq!(ann.mode, Some(Mode::Output));
        assert_eq!(ann.keyword.as_deref(), Some("node"));
    }

    #[test]
    fn golden_dash_annotations() {
        let ann = parse_annotations(
            "-- @beckon.title Lua Task\n\
             --- @beckon.subtitle SQL and Haskell style\n\
             -- @beckon.mode silent\n\
             --   @beckon.keyword lua\n",
        );
        assert_eq!(ann.title.as_deref(), Some("Lua Task"));
        assert_eq!(ann.subtitle.as_deref(), Some("SQL and Haskell style"));
        assert_eq!(ann.mode, Some(Mode::Silent));
        assert_eq!(ann.keyword.as_deref(), Some("lua"));
    }

    #[test]
    fn annotations_default_when_missing() {
        let ann = parse_annotations("#!/bin/sh\n# @beckon.title Only Title\necho x\n");
        assert_eq!(ann.title.as_deref(), Some("Only Title"));
        assert_eq!(ann.subtitle, None);
        assert_eq!(ann.mode, None);
        assert_eq!(ann.keyword, None);
        // A completely unannotated header parses to all-absent.
        assert_eq!(parse_annotations("#!/bin/sh\necho x\n"), Ann::default());
    }

    #[test]
    fn annotations_stop_after_line_30() {
        // Title on line 30 is inside the window; keyword on 31 is not.
        let mut text = String::new();
        for _ in 0..29 {
            text.push_str("# filler\n");
        }
        text.push_str("# @beckon.title Edge Of Window\n");
        text.push_str("# @beckon.keyword too-late\n");
        let ann = parse_annotations(&text);
        assert_eq!(ann.title.as_deref(), Some("Edge Of Window"));
        assert_eq!(ann.keyword, None);
    }

    #[test]
    fn first_valid_occurrence_wins_and_unknown_mode_is_ignored() {
        let ann = parse_annotations(
            "# @beckon.title First\n\
             # @beckon.title Second\n\
             # @beckon.mode loud\n\
             # @beckon.mode output\n\
             # @beckon.volume 11\n",
        );
        assert_eq!(ann.title.as_deref(), Some("First"));
        // "loud" is not a mode, so the later valid value lands.
        assert_eq!(ann.mode, Some(Mode::Output));
    }

    #[test]
    fn annotation_without_value_is_ignored() {
        let ann = parse_annotations("# @beckon.title\n# @beckon.subtitle   \n");
        assert_eq!(ann, Ann::default());
    }

    #[test]
    fn filename_stem_titles_unannotated_scripts() {
        let dir = temp_dir();
        write_script(&dir, "backup.sh", "#!/bin/sh\necho x\n", true);
        write_script(&dir, "noext", "#!/bin/sh\necho x\n", true);
        let mut warned = BTreeSet::new();
        let scripts = scan_dir(&dir, &mut warned);
        let titles: Vec<&str> = scripts.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["backup", "noext"]);
        // Unannotated scripts default to silent mode.
        assert!(scripts.iter().all(|s| s.mode == Mode::Silent));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_skips_hidden_and_warns_non_executable_once() {
        let dir = temp_dir();
        write_script(&dir, "run.sh", "#!/bin/sh\necho x\n", true);
        write_script(&dir, ".hidden.sh", "#!/bin/sh\necho x\n", true);
        let plain = write_script(&dir, "forgot-chmod.sh", "#!/bin/sh\necho x\n", false);
        fs::create_dir(dir.join("subdir")).expect("mkdir");
        let mut warned = BTreeSet::new();
        let scripts = scan_dir(&dir, &mut warned);
        let names: Vec<&str> = scripts.iter().map(|s| s.file_name.as_str()).collect();
        assert_eq!(names, vec!["run.sh"]);
        assert!(warned.contains(&plain));
        // Scanning again does not grow the warned set: hint fires once.
        let _ = scan_dir(&dir, &mut warned);
        assert_eq!(warned.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_skips_docs_without_warning() {
        let dir = temp_dir();
        write_script(&dir, "README.md", "# docs\n", false);
        write_script(&dir, "notes.txt", "notes\n", false);
        let mut warned = BTreeSet::new();
        let scripts = scan_dir(&dir, &mut warned);
        assert!(scripts.is_empty());
        assert!(warned.is_empty(), "docs must not trigger the chmod hint");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn items_are_sorted_and_shaped() {
        let dir = temp_dir();
        write_script(
            &dir,
            "zeta.sh",
            "#!/bin/sh\n# @beckon.title Alpha Task\n# @beckon.mode output\necho x\n",
            true,
        );
        write_script(&dir, "beta.sh", "#!/bin/sh\necho x\n", true);
        let items = items_in(&dir);
        // Ordered by title, not file name: "Alpha Task" before "beta".
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["script.zeta.sh", "script.beta.sh"]);
        for item in &items {
            assert_eq!(item.kind, ItemKind::Script);
            assert!(item.id.starts_with("script."), "bad id: {}", item.id);
            assert!(!item.title.is_empty());
            assert!(!item.subtitle.contains('\n'));
        }
        // Subtitle is the path plus the mode.
        assert!(items[0].subtitle.contains("zeta.sh"));
        assert!(items[0].subtitle.ends_with("(output)"));
        assert!(items[1].subtitle.ends_with("(silent)"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn items_are_deterministic() {
        let dir = temp_dir();
        write_script(&dir, "a.sh", "#!/bin/sh\necho x\n", true);
        write_script(
            &dir,
            "b.sh",
            "#!/bin/sh\n# @beckon.title Bee\necho x\n",
            true,
        );
        assert_eq!(items_in(&dir), items_in(&dir));
        // A missing directory is simply empty, not an error.
        assert!(items_in(&dir.join("nope")).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn id_round_trips_from_items_to_activate() {
        let dir = temp_dir();
        write_script(
            &dir,
            "greet.sh",
            "#!/bin/sh\n# @beckon.title Greet\n# @beckon.mode output\necho hi there\n",
            true,
        );
        let items = items_in(&dir);
        assert_eq!(items.len(), 1);
        let result = activate_in(&dir, &items[0].id, 5_000);
        assert_eq!(result, Ok("hi there".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_output_mode_captures_trimmed_stdout() {
        let dir = temp_dir();
        write_script(
            &dir,
            "out.sh",
            "#!/bin/sh\n# @beckon.mode output\necho '  padded  '\necho\n",
            true,
        );
        let result = activate_in(&dir, "script.out.sh", 5_000);
        assert_eq!(result, Ok("padded".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_silent_mode_returns_empty_string() {
        let dir = temp_dir();
        write_script(&dir, "quiet.sh", "#!/bin/sh\necho ignored stdout\n", true);
        let result = activate_in(&dir, "script.quiet.sh", 5_000);
        assert_eq!(result, Ok(String::new()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_nonzero_exit_surfaces_stderr() {
        let dir = temp_dir();
        write_script(
            &dir,
            "boom.sh",
            "#!/bin/sh\necho 'boom happened' >&2\nexit 3\n",
            true,
        );
        let err = activate_in(&dir, "script.boom.sh", 5_000).unwrap_err();
        assert!(err.contains("boom happened"), "{err}");
        assert!(err.contains("boom.sh"), "{err}");
        // Nonzero exit with silent stderr still names the status.
        write_script(&dir, "mute-fail.sh", "#!/bin/sh\nexit 7\n", true);
        let err = activate_in(&dir, "script.mute-fail.sh", 5_000).unwrap_err();
        assert!(err.contains("mute-fail.sh"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_unknown_and_malformed_ids_err() {
        let dir = temp_dir();
        write_script(&dir, "real.sh", "#!/bin/sh\ntrue\n", true);
        assert!(activate_in(&dir, "script.fake.sh", 5_000).is_err());
        assert!(activate_in(&dir, "system.sleep", 5_000).is_err());
        assert!(activate_in(&dir, "", 5_000).is_err());
        assert!(activate_in(&dir, "script.../real.sh", 5_000).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    // The timeout test uses the test-only timeout knob (250 ms instead
    // of the shipped 10 s) so it proves the kill without slowing the
    // suite; `exec` makes the sleeper the direct child, so the kill
    // lands on the process actually sleeping.
    #[test]
    fn activate_timeout_kills_sleeper_fast() {
        let dir = temp_dir();
        write_script(&dir, "sleeper.sh", "#!/bin/sh\nexec sleep 5\n", true);
        let started = Instant::now();
        let err = activate_in(&dir, "script.sleeper.sh", 250).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("sleeper.sh"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout took {:?}",
            started.elapsed()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_does_not_inherit_stdin() {
        let dir = temp_dir();
        // cat drains stdin: with a null stdin it sees EOF immediately;
        // an inherited stdin could block until the timeout.
        write_script(
            &dir,
            "stdin.sh",
            "#!/bin/sh\n# @beckon.mode output\ncat\necho reached-the-end\n",
            true,
        );
        let result = activate_in(&dir, "script.stdin.sh", 5_000);
        assert_eq!(result, Ok("reached-the-end".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activate_runs_in_the_scripts_dir() {
        let dir = temp_dir();
        write_script(
            &dir,
            "where.sh",
            "#!/bin/sh\n# @beckon.mode output\npwd -P\n",
            true,
        );
        let result = activate_in(&dir, "script.where.sh", 5_000).expect("runs");
        let reported = fs::canonicalize(&result).expect("canonical reported");
        let expected = fs::canonicalize(&dir).expect("canonical dir");
        assert_eq!(reported, expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_rescans_only_when_dir_mtime_changes() {
        let dir = temp_dir();
        write_script(
            &dir,
            "a.sh",
            "#!/bin/sh\n# @beckon.title Version One\ntrue\n",
            true,
        );
        let mut cache = ScanCache::new();
        assert_eq!(cache.refresh(&dir)[0].title, "Version One");
        // Rewriting the file changes the file mtime but not the
        // directory mtime, so the cache (by design) stays stale.
        fs::write(
            dir.join("a.sh"),
            "#!/bin/sh\n# @beckon.title Version Two\ntrue\n",
        )
        .expect("rewrite");
        assert_eq!(cache.refresh(&dir)[0].title, "Version One");
        // Adding a file touches the directory mtime: full re-scan, and
        // the rewritten annotation is picked up too.
        thread::sleep(Duration::from_millis(30));
        write_script(&dir, "b.sh", "#!/bin/sh\ntrue\n", true);
        let scripts: Vec<String> = cache
            .refresh(&dir)
            .iter()
            .map(|s| s.title.clone())
            .collect();
        assert_eq!(scripts, vec!["Version Two".to_string(), "b".to_string()]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_dir_with_example_bootstraps_once() {
        let root = temp_dir();
        let dir = root.join("scripts");
        let created = ensure_dir_with_example_in(&dir).expect("bootstrap");
        assert_eq!(created, dir);
        assert!(dir.join("README.md").is_file());
        let hello = dir.join("hello.sh");
        let mode = fs::metadata(&hello)
            .expect("hello meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        // The example is discovered with its annotations.
        let mut warned = BTreeSet::new();
        let scripts = scan_dir(&dir, &mut warned);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].title, "Hello from beckon");
        assert_eq!(scripts[0].mode, Mode::Output);
        assert!(warned.is_empty(), "the README must not trigger the hint");
        // And it actually runs, greeting on stdout.
        let out = activate_in(&dir, "script.hello.sh", 5_000).expect("hello runs");
        assert!(out.starts_with("Hello, "), "{out}");
        // A second call never overwrites: user edits stick.
        fs::write(dir.join("README.md"), "user edited\n").expect("edit");
        fs::remove_file(&hello).expect("delete example");
        let again = ensure_dir_with_example_in(&dir).expect("second call");
        assert_eq!(again, dir);
        let readme = fs::read_to_string(dir.join("README.md")).expect("read");
        assert_eq!(readme, "user edited\n");
        assert!(!hello.exists(), "deleted example must not come back");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn abbreviate_replaces_home_prefix() {
        let home = Some(OsString::from("/home/u"));
        assert_eq!(
            abbreviate_from(Path::new("/home/u/.beckon/scripts/x.sh"), home.clone()),
            "~/.beckon/scripts/x.sh"
        );
        assert_eq!(abbreviate_from(Path::new("/home/u"), home.clone()), "~");
        assert_eq!(abbreviate_from(Path::new("/etc/x.sh"), home), "/etc/x.sh");
        assert_eq!(abbreviate_from(Path::new("/etc/x.sh"), None), "/etc/x.sh");
        assert_eq!(
            abbreviate_from(Path::new("/etc/x.sh"), Some(OsString::new())),
            "/etc/x.sh"
        );
    }
}
