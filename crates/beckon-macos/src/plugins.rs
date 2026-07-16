//! Plugin host: discovers executables in `<store_root>/plugins/` and
//! speaks the beckon plugin protocol (JSON-RPC 2.0 over stdio, specified
//! in `beckon_core::rpc`) to each one.
//!
//! Lifecycle. [`start`] scans the plugins directory and records what is
//! there; it spawns nothing. A plugin process is spawned on demand, the
//! first time something needs its manifest (the trigger table, a query,
//! an activation), and is then kept alive for the rest of the session
//! with its stdin and stdout as the protocol pipes. On beckon exit the
//! pipes close and a well-behaved plugin exits on EOF.
//!
//! Handshake. Immediately after spawn the host sends `beckon.manifest`
//! and waits up to two seconds for the reply. The wait is implemented
//! with a dedicated reader thread per plugin that pushes complete stdout
//! lines into an mpsc channel; the host side blocks on
//! `Receiver::recv_timeout`, which is the entire timeout mechanism (no
//! polling, no signals). A plugin that misses the deadline, replies with
//! garbage, or declares an unsupported protocol version is killed and
//! blacklisted for the session.
//!
//! Robustness rules, in one place:
//! - Response lines are capped at 1 MiB; an oversized line is fatal.
//! - Every call has the same two second timeout as the handshake.
//! - A plugin whose process has died (EOF, write failure, timeout) is
//!   respawned once per query attempt; if the respawn also fails the
//!   plugin is blacklisted for the session.
//! - A JSON-RPC error response is a per-request failure, not a death:
//!   the plugin stays alive and nothing is respawned.
//! - Ids, titles, and subtitles from plugins have control characters
//!   stripped before they reach the display or the registry.
//!
//! stderr. Each plugin's stderr is piped into a cheap forwarder thread
//! that prefixes every line with the plugin's executable name and writes
//! it to beckon's stderr, so plugin diagnostics stay visible and
//! attributable. (The alternative of inheriting the raw fd would
//! interleave unattributed lines; /dev/null would hide crashes. The one
//! thread per plugin is the cheap middle.)
//!
//! Item ids. A plugin item surfaces in the registry as
//! `plugin.<name>.<plugin item id>` where `<name>` is the manifest name
//! with dots and whitespace rewritten to `-` (so the id remains
//! splittable) and `<plugin item id>` is the plugin's own id, echoed
//! back verbatim on activation.
//!
//! The integrator wires [`keywords`], [`query`], and [`activate`] into
//! the engine after merge; until then this module is unreferenced.

use beckon_core::router::{Item, ItemKind};
use beckon_core::rpc;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// How long the handshake and every later call may take before the
/// plugin is considered dead.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap on one response line. A plugin that emits more per line than this
/// is broken or hostile; either way the read stops there.
const MAX_LINE_BYTES: usize = 1 << 20;

/// What beckon should do after a plugin item activates. The integrator
/// maps these onto the engine's existing paths (copy_to_clipboard,
/// copy_then_paste, open_url).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAction {
    /// The plugin handled everything itself.
    None,
    /// Copy the payload to the clipboard.
    Copy(String),
    /// Copy the payload and paste it into the frontmost app.
    Paste(String),
    /// Open the payload as a URL or path.
    Open(String),
}

/// One discovered plugin executable, not necessarily running.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginDef {
    /// The executable's file name, used for stderr prefixes and as the
    /// fallback identity before the manifest arrives.
    file_name: String,
    /// Absolute path to the executable.
    path: PathBuf,
}

/// A running plugin: the child process, its protocol pipes, and the
/// manifest it answered the handshake with.
struct LivePlugin {
    child: Child,
    stdin: ChildStdin,
    /// Lines from the reader thread. `Err` is terminal: EOF, an
    /// oversized line, or a broken read; the thread exits after sending.
    rx: Receiver<Result<String, String>>,
    manifest: rpc::Manifest,
    /// The next JSON-RPC request id. The handshake used 1.
    next_id: i128,
}

impl Drop for LivePlugin {
    fn drop(&mut self) {
        // Removal from the live table is how plugins are put down; the
        // reader and stderr threads exit on the EOF this causes.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One call's failure. `fatal` means the process is unusable (dead,
/// silent, or speaking garbage) and should be killed; a non-fatal
/// failure is a well-formed JSON-RPC error response from a live plugin.
struct CallFail {
    fatal: bool,
    msg: String,
}

impl CallFail {
    fn fatal(msg: String) -> CallFail {
        CallFail { fatal: true, msg }
    }

    fn soft(msg: String) -> CallFail {
        CallFail { fatal: false, msg }
    }
}

impl LivePlugin {
    /// Send one request line and wait for the matching response. A
    /// response whose id does not match is a protocol violation (calls
    /// are strictly sequential, so there is nothing else it could
    /// belong to) and is fatal.
    fn call(
        &mut self,
        line: &str,
        expect_id: i128,
    ) -> Result<beckon_core::persist::Value, CallFail> {
        if let Err(e) = self
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
        {
            return Err(CallFail::fatal(format!("stdin write failed: {e}")));
        }
        let text = recv_line(&self.rx, REPLY_TIMEOUT).map_err(CallFail::fatal)?;
        match rpc::parse_response(&text) {
            Ok(resp) if resp.id == expect_id => Ok(resp.result),
            Ok(resp) => Err(CallFail::fatal(format!(
                "response id {} does not match request id {expect_id}",
                resp.id
            ))),
            Err(rpc::RpcError::Remote { code, message, .. }) => {
                Err(CallFail::soft(format!("plugin error {code}: {message}")))
            }
            Err(e) => Err(CallFail::fatal(format!("protocol violation: {e}"))),
        }
    }
}

/// Wait for the next non-blank line from the reader thread, up to
/// `timeout`. Blank lines are tolerated and skipped (some runtimes pad
/// output); the deadline covers the whole wait, not each attempt.
fn recv_line(rx: &Receiver<Result<String, String>>, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("no response within {}s", timeout.as_secs()));
        }
        match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => {
                if !line.trim().is_empty() {
                    return Ok(line);
                }
            }
            Ok(Err(reason)) => return Err(format!("plugin stdout ended: {reason}")),
            Err(_) => return Err(format!("no response within {}s", timeout.as_secs())),
        }
    }
}

/// Reader thread: turns the plugin's stdout into a stream of complete
/// lines on a channel, enforcing [`MAX_LINE_BYTES`]. Sends one final
/// `Err` and exits on EOF, an oversized line, non-UTF-8 bytes, or a read
/// failure. `recv_timeout` on the receiving side is what turns "plugin
/// never answered" into a clean timeout instead of a blocked host.
fn spawn_reader(stdout: ChildStdout) -> Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut buf: Vec<u8> = Vec::new();
            let n = {
                let mut limited = (&mut reader).take(MAX_LINE_BYTES as u64 + 1);
                match limited.read_until(b'\n', &mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = tx.send(Err(format!("read failed: {e}")));
                        return;
                    }
                }
            };
            if n == 0 {
                let _ = tx.send(Err("end of stream".to_string()));
                return;
            }
            let content_len = if buf.last() == Some(&b'\n') {
                buf.len() - 1
            } else {
                buf.len()
            };
            if content_len > MAX_LINE_BYTES {
                let _ = tx.send(Err("response line exceeds 1 MiB".to_string()));
                return;
            }
            let Ok(line) = String::from_utf8(buf) else {
                let _ = tx.send(Err("response is not UTF-8".to_string()));
                return;
            };
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if tx.send(Ok(line)).is_err() {
                return;
            }
        }
    });
    rx
}

/// Forward a plugin's stderr to beckon's stderr, one line at a time,
/// prefixed with the plugin's executable name so diagnostics stay
/// attributable. Exits when the plugin's stderr closes.
fn spawn_stderr_forwarder(stderr: ChildStderr, name: String) {
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            eprintln!("beckon plugin[{name}]: {line}");
        }
    });
}

/// Strip control characters (including newlines and tabs) from
/// plugin-supplied text before it reaches the display or the registry.
fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Sanitize a manifest name into an id-safe segment: control characters
/// stripped, dots and whitespace rewritten to `-` (dots would make
/// `plugin.<name>.<item>` ambiguous). Falls back to the executable file
/// name, then to a literal `plugin`, so the result is never empty.
fn sanitize_name(raw: &str, fallback: &str) -> String {
    fn clean(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_control())
            .map(|c| {
                if c == '.' || c.is_whitespace() {
                    '-'
                } else {
                    c
                }
            })
            .collect()
    }
    let cleaned = clean(raw);
    if !cleaned.is_empty() {
        return cleaned;
    }
    let fallback = clean(fallback);
    if !fallback.is_empty() {
        return fallback;
    }
    "plugin".to_string()
}

/// Build the registry id for one plugin item.
fn make_item_id(plugin_name: &str, raw_item_id: &str) -> String {
    format!("plugin.{plugin_name}.{}", strip_controls(raw_item_id))
}

/// Split a registry id back into (index into `names`, plugin item id).
/// Sanitized names contain no dots, but the longest-name match keeps
/// this correct even for names that prefix each other.
fn split_plugin_id<'a>(id: &'a str, names: &[String]) -> Option<(usize, &'a str)> {
    let rest = id.strip_prefix("plugin.")?;
    let mut best: Option<(usize, &'a str)> = None;
    for (i, name) in names.iter().enumerate() {
        let Some(tail) = rest.strip_prefix(name.as_str()) else {
            continue;
        };
        let Some(item) = tail.strip_prefix('.') else {
            continue;
        };
        if item.is_empty() {
            continue;
        }
        let better = match best {
            None => true,
            Some((prev, _)) => names[prev].len() < name.len(),
        };
        if better {
            best = Some((i, item));
        }
    }
    best
}

/// Scan `dir` for plugin executables: regular files with any execute
/// bit set (the same rule script commands use). Non-executables,
/// directories, and unreadable entries are skipped silently; a missing
/// directory means no plugins. The result is sorted by file name so
/// discovery order is deterministic.
fn discover(dir: &Path) -> Vec<PluginDef> {
    let mut defs = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return defs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        defs.push(PluginDef {
            file_name: file_name.to_string_lossy().to_string(),
            path,
        });
    }
    defs.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    defs
}

/// Spawn one plugin and run the manifest handshake. On any failure the
/// child is killed and the error says why; the caller blacklists.
fn spawn_and_handshake(path: &Path, file_name: &str) -> Result<LivePlugin, String> {
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot spawn: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    spawn_stderr_forwarder(stderr, file_name.to_string());
    let rx = spawn_reader(stdout);

    let handshake_id = 1;
    let outcome = (|| -> Result<rpc::Manifest, String> {
        let line = rpc::manifest_request(handshake_id);
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("stdin write failed: {e}"))?;
        let text = recv_line(&rx, REPLY_TIMEOUT)?;
        let resp = rpc::parse_response(&text).map_err(|e| format!("bad manifest response: {e}"))?;
        if resp.id != handshake_id {
            return Err(format!(
                "manifest response id {} does not match request id {handshake_id}",
                resp.id
            ));
        }
        rpc::decode_manifest(&resp.result).map_err(|e| format!("bad manifest: {e}"))
    })();

    match outcome {
        Ok(mut manifest) => {
            manifest.name = sanitize_name(&manifest.name, file_name);
            Ok(LivePlugin {
                child,
                stdin,
                rx,
                manifest,
                next_id: handshake_id + 1,
            })
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// All host state behind the module's free functions. Kept as a plain
/// struct so tests can run against fixture directories without touching
/// the global instance.
struct HostState {
    /// The directory scanned for plugin executables.
    plugins_dir: PathBuf,
    /// Everything discovery found, in file-name order.
    defs: Vec<PluginDef>,
    /// Running plugins, keyed by executable path.
    live: HashMap<PathBuf, LivePlugin>,
    /// Plugins disabled for the rest of the session.
    blacklist: HashSet<PathBuf>,
}

impl HostState {
    fn new(plugins_dir: PathBuf) -> HostState {
        HostState {
            plugins_dir,
            defs: Vec::new(),
            live: HashMap::new(),
            blacklist: HashSet::new(),
        }
    }

    /// Discovery only: scan the directory, spawn nothing.
    fn discover(&mut self) {
        self.defs = discover(&self.plugins_dir);
    }

    /// Make sure the plugin at `path` is running and handshaken. A
    /// failed spawn or handshake blacklists it for the session.
    fn ensure_live(&mut self, path: &Path) -> Result<(), String> {
        if self.blacklist.contains(path) {
            return Err(format!(
                "plugin {} is disabled for this session",
                path.display()
            ));
        }
        if self.live.contains_key(path) {
            return Ok(());
        }
        let file_name = self
            .defs
            .iter()
            .find(|d| d.path == path)
            .map(|d| d.file_name.clone())
            .unwrap_or_else(|| "plugin".to_string());
        match spawn_and_handshake(path, &file_name) {
            Ok(live) => {
                self.live.insert(path.to_path_buf(), live);
                Ok(())
            }
            Err(e) => {
                self.blacklist.insert(path.to_path_buf());
                eprintln!(
                    "beckon: plugin {} disabled for this session: {e}",
                    path.display()
                );
                Err(e)
            }
        }
    }

    /// Call one plugin with the respawn policy: a fatal failure kills
    /// the process and retries once with a fresh spawn; a second fatal
    /// failure blacklists. A non-fatal (JSON-RPC error) failure returns
    /// immediately and the plugin stays alive.
    fn call_plugin(
        &mut self,
        path: &Path,
        build: impl Fn(i128) -> String,
    ) -> Result<beckon_core::persist::Value, String> {
        let mut last_err = String::new();
        for attempt in 0..2 {
            self.ensure_live(path)?;
            let live = self
                .live
                .get_mut(path)
                .ok_or_else(|| "plugin vanished from the live table".to_string())?;
            let id = live.next_id;
            live.next_id += 1;
            let line = build(id);
            match live.call(&line, id) {
                Ok(value) => return Ok(value),
                Err(fail) if !fail.fatal => return Err(fail.msg),
                Err(fail) => {
                    last_err = fail.msg;
                    // Dropping the LivePlugin kills the child.
                    self.live.remove(path);
                    if attempt == 1 {
                        self.blacklist.insert(path.to_path_buf());
                        eprintln!(
                            "beckon: plugin {} disabled for this session: {last_err}",
                            path.display()
                        );
                    }
                }
            }
        }
        Err(last_err)
    }

    /// The trigger table: (keyword, plugin name) for every healthy
    /// plugin. This is the on-demand first use: any plugin not yet
    /// running is spawned and handshaken here.
    fn keywords(&mut self) -> Vec<(String, String)> {
        let paths: Vec<PathBuf> = self.defs.iter().map(|d| d.path.clone()).collect();
        let mut out = Vec::new();
        for path in paths {
            if self.ensure_live(&path).is_err() {
                continue;
            }
            if let Some(live) = self.live.get(&path) {
                out.push((live.manifest.keyword.clone(), live.manifest.name.clone()));
            }
        }
        out
    }

    /// The plugin whose manifest keyword is `keyword`, spawning as
    /// needed. First match in discovery order wins.
    fn path_for_keyword(&mut self, keyword: &str) -> Option<PathBuf> {
        let paths: Vec<PathBuf> = self.defs.iter().map(|d| d.path.clone()).collect();
        for path in paths {
            if self.ensure_live(&path).is_err() {
                continue;
            }
            if self
                .live
                .get(&path)
                .is_some_and(|l| l.manifest.keyword == keyword)
            {
                return Some(path);
            }
        }
        None
    }

    /// Route `query` to the plugin owning `keyword` and map its items
    /// into registry items. Any failure degrades to an empty list (the
    /// launcher shows nothing rather than an error row); the cause goes
    /// to stderr.
    fn query(&mut self, keyword: &str, query: &str) -> Vec<Item> {
        let Some(path) = self.path_for_keyword(keyword) else {
            return Vec::new();
        };
        let result = match self.call_plugin(&path, |id| rpc::query_request(id, query)) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("beckon: plugin query for keyword {keyword:?} failed: {e}");
                return Vec::new();
            }
        };
        let Some(name) = self.live.get(&path).map(|l| l.manifest.name.clone()) else {
            return Vec::new();
        };
        match rpc::decode_query_items(&result) {
            Ok(items) => items
                .into_iter()
                .map(|item| Item {
                    id: make_item_id(&name, &item.id),
                    title: strip_controls(&item.title),
                    subtitle: strip_controls(&item.subtitle),
                    kind: ItemKind::Script,
                })
                .collect(),
            Err(e) => {
                eprintln!("beckon: plugin {name} returned a bad query result: {e}");
                Vec::new()
            }
        }
    }

    /// Activate a `plugin.<name>.<item>` registry id: find the owning
    /// plugin by name, send `beckon.activate`, and map the declared
    /// action. Errors are messages worth showing to the user.
    fn activate(&mut self, id: &str) -> Result<PluginAction, String> {
        // Names come from manifests, so make discovered plugins live
        // before resolving. Failures just shrink the candidate set.
        let paths: Vec<PathBuf> = self.defs.iter().map(|d| d.path.clone()).collect();
        for path in &paths {
            let _ = self.ensure_live(path);
        }
        let named: Vec<(String, PathBuf)> = self
            .live
            .iter()
            .map(|(path, live)| (live.manifest.name.clone(), path.clone()))
            .collect();
        let names: Vec<String> = named.iter().map(|(name, _)| name.clone()).collect();
        let (idx, item_id) =
            split_plugin_id(id, &names).ok_or_else(|| format!("no plugin owns id {id:?}"))?;
        let item_id = item_id.to_string();
        let path = named[idx].1.clone();
        let result = self.call_plugin(&path, |rid| rpc::activate_request(rid, &item_id))?;
        let activation = rpc::decode_activation(&result).map_err(|e| e.to_string())?;
        Ok(match activation {
            rpc::Activation::None => PluginAction::None,
            rpc::Activation::Copy(v) => PluginAction::Copy(v),
            rpc::Activation::Paste(v) => PluginAction::Paste(v),
            rpc::Activation::Open(v) => PluginAction::Open(v),
        })
    }
}

/// The one process-wide host instance behind the public functions.
static STATE: OnceLock<Mutex<HostState>> = OnceLock::new();

fn state() -> MutexGuard<'static, HostState> {
    STATE
        .get_or_init(|| {
            Mutex::new(HostState::new(
                beckon_core::persist::store_root().join("plugins"),
            ))
        })
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Scan `<store_root>/plugins/` for plugin executables. Discovery only:
/// no process is spawned until a plugin's manifest is actually needed.
pub fn start() {
    state().discover();
}

/// The trigger table: (keyword, plugin name) pairs for the integrator.
/// This is the on-demand first use, so it spawns and handshakes any
/// discovered plugin that is not already running.
pub fn keywords() -> Vec<(String, String)> {
    state().keywords()
}

/// Send `query` to the plugin owning `keyword`; items come back as
/// registry [`Item`]s with ids of the form `plugin.<name>.<item id>`
/// and kind [`ItemKind::Script`]. Failures degrade to an empty list.
pub fn query(keyword: &str, query: &str) -> Vec<Item> {
    state().query(keyword, query)
}

/// Activate a `plugin.<name>.<item id>` registry id and report what
/// beckon should do next.
pub fn activate(id: &str) -> Result<PluginAction, String> {
    state().activate(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh per-test plugins directory under the system temp dir.
    fn temp_plugins_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "beckon-plugins-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Drop a file into `dir` with the given mode.
    fn write_file(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod fixture");
        path
    }

    // ---- pure parts: run in the gate -----------------------------------

    #[test]
    fn strip_controls_removes_control_chars_only() {
        assert_eq!(strip_controls("plain"), "plain");
        assert_eq!(strip_controls("a\nb\tc\rd\u{1}e\u{7f}f"), "abcdef");
        assert_eq!(strip_controls("ünïcode 🎉 stays"), "ünïcode 🎉 stays");
        assert_eq!(strip_controls(""), "");
    }

    #[test]
    fn sanitize_name_makes_id_safe_segments() {
        assert_eq!(sanitize_name("demo", "x"), "demo");
        assert_eq!(sanitize_name("my.plugin v2", "x"), "my-plugin-v2");
        assert_eq!(
            sanitize_name("\u{1}\u{2}", "example-plugin.py"),
            "example-plugin-py"
        );
        assert_eq!(sanitize_name("", ""), "plugin");
    }

    #[test]
    fn item_id_round_trip() {
        let names = vec!["demo".to_string(), "other".to_string()];
        let id = make_item_id("demo", "echo");
        assert_eq!(id, "plugin.demo.echo");
        assert_eq!(split_plugin_id(&id, &names), Some((0, "echo")));
        // Item ids may themselves contain dots; the split is at the
        // name boundary, not the last dot.
        let id = make_item_id("demo", "a.b.c");
        assert_eq!(split_plugin_id(&id, &names), Some((0, "a.b.c")));
        // Control characters in the raw item id are stripped.
        assert_eq!(make_item_id("demo", "e\ncho"), "plugin.demo.echo");
    }

    #[test]
    fn split_plugin_id_rejects_foreign_ids() {
        let names = vec!["demo".to_string()];
        assert_eq!(split_plugin_id("app.safari", &names), None);
        assert_eq!(split_plugin_id("plugin.unknown.item", &names), None);
        assert_eq!(split_plugin_id("plugin.demo", &names), None);
        assert_eq!(split_plugin_id("plugin.demo.", &names), None);
        assert_eq!(split_plugin_id("", &names), None);
    }

    #[test]
    fn split_plugin_id_prefers_the_longest_name() {
        let names = vec!["demo".to_string(), "demo-extra".to_string()];
        assert_eq!(
            split_plugin_id("plugin.demo-extra.item", &names),
            Some((1, "item"))
        );
        assert_eq!(
            split_plugin_id("plugin.demo.item", &names),
            Some((0, "item"))
        );
    }

    #[test]
    fn discovery_finds_only_executable_regular_files() {
        let dir = temp_plugins_dir();
        write_file(&dir, "zeta", "#!/bin/sh\n", 0o755);
        write_file(&dir, "alpha", "#!/bin/sh\n", 0o755);
        write_file(&dir, "not-executable", "#!/bin/sh\n", 0o644);
        fs::create_dir(dir.join("subdir")).expect("mkdir");
        let defs = discover(&dir);
        let names: Vec<&str> = defs.iter().map(|d| d.file_name.as_str()).collect();
        // Executables only, sorted by file name.
        assert_eq!(names, vec!["alpha", "zeta"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_of_a_missing_directory_is_empty() {
        let dir = temp_plugins_dir().join("does-not-exist");
        assert!(discover(&dir).is_empty());
    }

    #[test]
    fn host_starts_empty_and_discovery_spawns_nothing() {
        let dir = temp_plugins_dir();
        write_file(&dir, "demo", "#!/bin/sh\nsleep 5\n", 0o755);
        let mut host = HostState::new(dir.clone());
        host.discover();
        assert_eq!(host.defs.len(), 1);
        // Discovery alone never spawns: no live plugins, no blacklist.
        assert!(host.live.is_empty());
        assert!(host.blacklist.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_for_unknown_keyword_is_empty() {
        let mut host = HostState::new(temp_plugins_dir());
        host.discover();
        assert!(host.query("nope", "anything").is_empty());
    }

    #[test]
    fn activate_of_foreign_id_is_an_err_not_a_panic() {
        let mut host = HostState::new(temp_plugins_dir());
        host.discover();
        assert!(host.activate("app.safari").is_err());
        assert!(host.activate("plugin.ghost.item").is_err());
        assert!(host.activate("").is_err());
    }

    // ---- live subprocess tests: hardware-run, ignored in the gate ------

    /// The repo's example plugin, copied into a fixture dir.
    fn install_example_plugin(dir: &Path) {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/plugins/example-plugin.py");
        let body = fs::read_to_string(&source).expect("example plugin exists in docs/plugins");
        write_file(dir, "example-plugin.py", &body, 0o755);
    }

    #[test]
    #[ignore = "spawns live subprocesses (python3, sh); run manually on hardware"]
    fn example_plugin_end_to_end() {
        let dir = temp_plugins_dir();
        install_example_plugin(&dir);
        let mut host = HostState::new(dir.clone());
        host.discover();

        // Handshake happens on demand, at the first keywords() call.
        let kw = host.keywords();
        assert_eq!(kw, vec![("demo".to_string(), "demo".to_string())]);
        println!("keywords: {kw:?}");

        // Query through the real pipes.
        let items = host.query("demo", "hello beckon");
        println!("query 'hello beckon' -> {items:?}");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "plugin.demo.echo");
        assert_eq!(items[0].title, "Echo: hello beckon");
        assert_eq!(items[0].kind, ItemKind::Script);
        assert_eq!(items[1].id, "plugin.demo.upper");
        assert_eq!(items[1].title, "HELLO BECKON");

        // Activate maps the protocol action onto PluginAction.
        let action = host.activate("plugin.demo.echo").expect("activate");
        println!("activate plugin.demo.echo -> {action:?}");
        assert_eq!(action, PluginAction::Copy("hello beckon".to_string()));

        // The plugin answered three requests on one process: alive, not
        // respawned, not blacklisted.
        assert_eq!(host.live.len(), 1);
        assert!(host.blacklist.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns live subprocesses (sh); run manually on hardware"]
    fn garbage_on_stdout_blacklists_at_handshake() {
        let dir = temp_plugins_dir();
        write_file(
            &dir,
            "garbage",
            "#!/bin/sh\nread line\necho this is not json\n",
            0o755,
        );
        let mut host = HostState::new(dir.clone());
        host.discover();
        assert!(host.keywords().is_empty());
        assert_eq!(host.blacklist.len(), 1);
        // Blacklisted for the session: nothing retries it.
        assert!(host.keywords().is_empty());
        assert!(host.live.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns live subprocesses (sh) and waits out the 2s timeout; run manually"]
    fn handshake_timeout_blacklists() {
        let dir = temp_plugins_dir();
        write_file(&dir, "silent", "#!/bin/sh\nsleep 5\n", 0o755);
        let mut host = HostState::new(dir.clone());
        host.discover();
        let started = Instant::now();
        assert!(host.keywords().is_empty());
        let elapsed = started.elapsed();
        println!("silent plugin timed out after {elapsed:?}");
        assert!(elapsed >= Duration::from_secs(2));
        assert!(elapsed < Duration::from_secs(4), "timeout took {elapsed:?}");
        assert_eq!(host.blacklist.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns live subprocesses (sh); run manually on hardware"]
    fn oversized_response_line_is_fatal() {
        let dir = temp_plugins_dir();
        // Answers the handshake with a single line of over 2 MiB.
        write_file(
            &dir,
            "bloat",
            "#!/bin/sh\nread line\nhead -c 2097152 /dev/zero | tr '\\0' 'a'\nprintf '\\n'\n",
            0o755,
        );
        let mut host = HostState::new(dir.clone());
        host.discover();
        assert!(host.keywords().is_empty());
        assert_eq!(host.blacklist.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns live subprocesses (sh); run manually on hardware"]
    fn dead_plugin_is_respawned_once_then_blacklisted() {
        let dir = temp_plugins_dir();
        // Answers the manifest handshake, then exits: every query hits a
        // dead process.
        write_file(
            &dir,
            "dies",
            concat!(
                "#!/bin/sh\n",
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol\":1,",
                "\"name\":\"dies\",\"version\":\"1\",\"keyword\":\"dies\",",
                "\"description\":\"exits after handshake\"}}\\n'\n"
            ),
            0o755,
        );
        let mut host = HostState::new(dir.clone());
        host.discover();
        // Handshake itself succeeds.
        assert_eq!(
            host.keywords(),
            vec![("dies".to_string(), "dies".to_string())]
        );
        // The query hits EOF, respawns once (fresh handshake works),
        // hits EOF again, and blacklists.
        let items = host.query("dies", "x");
        assert!(items.is_empty());
        assert!(
            host.blacklist.len() == 1,
            "expected blacklist after respawn failure"
        );
        assert!(host.live.is_empty());
        // Session-dead: later calls do not resurrect it.
        assert!(host.keywords().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns live subprocesses (python3); run manually on hardware"]
    fn json_rpc_error_response_does_not_kill_the_plugin() {
        let dir = temp_plugins_dir();
        install_example_plugin(&dir);
        let mut host = HostState::new(dir.clone());
        host.discover();
        assert_eq!(host.keywords().len(), 1);
        // The example plugin answers unknown item ids for activate with
        // a normal result, but unknown METHODS with a JSON-RPC error.
        // Drive that path directly through call_plugin.
        let path = host.defs[0].path.clone();
        let err = host
            .call_plugin(&path, |id| {
                rpc::encode_request(
                    id,
                    "beckon.bogus",
                    beckon_core::persist::Value::Object(Default::default()),
                )
            })
            .expect_err("unknown method should be a remote error");
        println!("remote error surfaced as: {err}");
        assert!(err.contains("-32601"), "unexpected error text: {err}");
        // Non-fatal: still live, not blacklisted, and still answering.
        assert_eq!(host.live.len(), 1);
        assert!(host.blacklist.is_empty());
        assert_eq!(host.query("demo", "still alive").len(), 2);
        fs::remove_dir_all(&dir).ok();
    }
}
