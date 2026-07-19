//! System command source: sleep, lock, trash, dark mode, volume, and
//! friends as registry [`Item`]s, plus the activation that makes each
//! one happen.
//!
//! Every command is a fixed entry in `SPECS`, so [`items`] is a constant
//! list: deterministic order, stable `system.*` ids that double as
//! frecency keys. [`activate`] maps an id to a `Mechanism` through the
//! pure builder `mechanism`, then executes it. That split is the testing
//! story: the builders are golden-tested exhaustively without executing
//! anything, because most of these commands (sleep, lock, empty trash)
//! are not things a test run should do to the machine it runs on.
//!
//! Mechanisms are local executables spawned with std::process, which is
//! airgap clean: /usr/bin/pmset, /usr/bin/osascript, and /usr/bin/killall
//! are OS binaries and nothing here touches the network. The one
//! exception is the lock screen, a direct dlopen call. Per command:
//!
//! - Sleep: `pmset sleepnow`. Works unprivileged, no permission prompt.
//! - Lock Screen: SACLockScreenImmediate from the private
//!   login.framework, resolved at call time with dlopen/dlsym. The old
//!   CGSession helper is gone (verified absent on macOS 26) and
//!   keystroke emulation would drag in the Accessibility permission, so
//!   the private call is the reliable choice. No permission prompt. If a
//!   future macOS removes the symbol, dlsym fails and activate returns a
//!   clear Err instead of crashing. The function reports nothing back,
//!   so Ok means "the call was made".
//! - Empty Trash: osascript telling Finder. Deletes without Finder's
//!   confirmation dialog. First use from a new host process triggers the
//!   macOS Automation consent prompt for Finder; a denial surfaces as
//!   osascript stderr in the Err.
//! - Toggle Dark Mode: osascript telling System Events appearance
//!   preferences. Same Automation prompt story, for System Events.
//! - Toggle Mute, Volume Up, Volume Down: osascript `set volume`
//!   (StandardAdditions, no app targeted, so no permission prompt).
//!   Volume moves in steps of 6 out of 100, about one hardware volume
//!   key tick (100/16); AppleScript clamps out-of-range values into the
//!   0 to 100 range.
//! - Quit Frontmost App: osascript asks System Events for the frontmost
//!   process name, then tells that app to quit (a normal quit, so the
//!   app may prompt to save). Automation prompts: one for System Events,
//!   then one per distinct app the first time it is quit this way.
//! - Restart Finder: `killall Finder`; launchd relaunches Finder
//!   automatically. No permission prompt.
//!
//! Show Desktop is deliberately absent: there is no stable public
//! mechanism. The Mission Control binary's undocumented argv trick and
//! F11 keystroke emulation both depend on private behavior or on
//! user-remappable keyboard settings (plus the Accessibility
//! permission), so it is dropped rather than shipped flaky.
//!
//! Activation runs the child to completion to capture stderr; osascript
//! normally returns in tens of milliseconds, but a first-use Automation
//! prompt blocks the child (and therefore activate, and therefore the
//! main thread) until the user answers it. That is the honest cost of
//! surfacing the denial text as the Err the launcher can show.

// Nothing here has a caller yet: the integrator wires items() and

use beckon_core::router::{Item, ItemKind};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

/// The one scripting runner every osascript mechanism spawns.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// The private framework holding the lock-screen entry point. The
/// Versions/A path is the one that resolves on current macOS (the
/// framework lives in the dyld shared cache, not as a plain file, but
/// dlopen resolves it; verified on macOS 26).
const LOGIN_FRAMEWORK: &str = "/System/Library/PrivateFrameworks/login.framework/Versions/A/login";

/// The lock-screen function inside [`LOGIN_FRAMEWORK`].
const LOCK_SYMBOL: &str = "SACLockScreenImmediate";

/// RTLD_NOW from dlfcn.h: resolve everything at load time so a broken
/// framework fails inside dlopen with a clean Err, not mid-call.
const RTLD_NOW: c_int = 2;

/// One system command: the registry identity. The action lives in
/// `mechanism`, keyed by id, so the table stays a plain constant.
struct Spec {
    /// Stable unique id; also the frecency key. Never rename one.
    id: &'static str,
    /// What the user sees and fuzzy-matches against.
    title: &'static str,
    /// One-line description shown under the title.
    subtitle: &'static str,
}

/// Every system command, in registry order. The order is fixed here (not
/// sorted at runtime) so items() is deterministic by construction.
/// System Settings panes, openable by their long-stable URL scheme ids
/// (macOS 13+ extension identifiers; an id macOS does not recognize
/// still opens the Settings window, so unknown-pane failure is soft).
/// Titles end in "Settings" so queries like "keyboard" rank them.
const SETTINGS_PANES: &[(&str, &str, &str)] = &[
    (
        "keyboard",
        "Keyboard Settings",
        "com.apple.Keyboard-Settings.extension",
    ),
    (
        "displays",
        "Displays Settings",
        "com.apple.Displays-Settings.extension",
    ),
    (
        "bluetooth",
        "Bluetooth Settings",
        "com.apple.BluetoothSettings",
    ),
    (
        "wifi",
        "Wi-Fi Settings",
        "com.apple.wifi-settings-extension",
    ),
    (
        "battery",
        "Battery Settings",
        "com.apple.Battery-Settings.extension",
    ),
    (
        "sound",
        "Sound Settings",
        "com.apple.Sound-Settings.extension",
    ),
    (
        "notifications",
        "Notifications Settings",
        "com.apple.Notifications-Settings.extension",
    ),
    (
        "appearance",
        "Appearance Settings",
        "com.apple.Appearance-Settings.extension",
    ),
    (
        "accessibility",
        "Accessibility Settings",
        "com.apple.Accessibility-Settings.extension",
    ),
    (
        "privacy",
        "Privacy & Security Settings",
        "com.apple.settings.PrivacySecurity.extension",
    ),
    (
        "trackpad",
        "Trackpad Settings",
        "com.apple.Trackpad-Settings.extension",
    ),
    (
        "dock",
        "Desktop & Dock Settings",
        "com.apple.Desktop-Settings.extension",
    ),
    (
        "software-update",
        "Software Update Settings",
        "com.apple.Software-Update-Settings.extension",
    ),
    (
        "network",
        "Network Settings",
        "com.apple.Network-Settings.extension",
    ),
    (
        "screen-time",
        "Screen Time Settings",
        "com.apple.Screen-Time-Settings.extension",
    ),
];

const SPECS: &[Spec] = &[
    Spec {
        id: "system.sleep",
        title: "Sleep",
        subtitle: "Put the Mac to sleep now",
    },
    Spec {
        id: "system.lock-screen",
        title: "Lock Screen",
        subtitle: "Lock the screen immediately",
    },
    Spec {
        id: "system.empty-trash",
        title: "Empty Trash",
        subtitle: "Permanently delete everything in the Trash",
    },
    Spec {
        id: "system.toggle-dark-mode",
        title: "Toggle Dark Mode",
        subtitle: "Switch between light and dark appearance",
    },
    Spec {
        id: "system.toggle-mute",
        title: "Toggle Mute",
        subtitle: "Mute or unmute the output volume",
    },
    Spec {
        id: "system.volume-up",
        title: "Volume Up",
        subtitle: "Raise the output volume one step",
    },
    Spec {
        id: "system.volume-down",
        title: "Volume Down",
        subtitle: "Lower the output volume one step",
    },
    Spec {
        id: "system.quit-frontmost",
        title: "Quit Frontmost App",
        subtitle: "Ask the app in front to quit",
    },
    Spec {
        id: "system.restart-finder",
        title: "Restart Finder",
        subtitle: "Relaunch Finder (fixes a stuck desktop)",
    },
];

/// How a command happens. Built by the pure `mechanism` function so
/// tests can lock every argv and FFI target without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mechanism {
    /// Spawn element 0 with the remaining elements as arguments, wait
    /// for exit, and surface stderr as the Err on failure.
    Spawn(Vec<String>),
    /// dlopen `framework`, dlsym `symbol`, and call it as a
    /// zero-argument C function.
    Call {
        framework: &'static str,
        symbol: &'static str,
    },
}

/// Owned-argv convenience for the Spawn arm.
fn spawn(parts: &[&str]) -> Mechanism {
    Mechanism::Spawn(parts.iter().map(|p| p.to_string()).collect())
}

/// The pure command-to-action map. Multiple `-e` arguments to osascript
/// are lines of one script, so variables carry across them. Returns None
/// for ids this module does not own.
fn mechanism(id: &str) -> Option<Mechanism> {
    if let Some(slug) = id.strip_prefix("settings.") {
        let (_, _, pane) = SETTINGS_PANES.iter().find(|(s, _, _)| *s == slug)?;
        return Some(spawn(&[
            "/usr/bin/open",
            &format!("x-apple.systempreferences:{pane}"),
        ]));
    }
    match id {
        "system.sleep" => Some(spawn(&["/usr/bin/pmset", "sleepnow"])),
        "system.lock-screen" => Some(Mechanism::Call {
            framework: LOGIN_FRAMEWORK,
            symbol: LOCK_SYMBOL,
        }),
        "system.empty-trash" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            r#"tell application "Finder" to empty trash"#,
        ])),
        "system.toggle-dark-mode" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            r#"tell application "System Events" to tell appearance preferences to set dark mode to not dark mode"#,
        ])),
        "system.toggle-mute" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            "set volume output muted (not (output muted of (get volume settings)))",
        ])),
        "system.volume-up" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            "set cur to output volume of (get volume settings)",
            "-e",
            "set volume output volume (cur + 6)",
        ])),
        "system.volume-down" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            "set cur to output volume of (get volume settings)",
            "-e",
            "set volume output volume (cur - 6)",
        ])),
        "system.quit-frontmost" => Some(spawn(&[
            OSASCRIPT,
            "-e",
            r#"tell application "System Events" to set frontName to name of first application process whose frontmost is true"#,
            "-e",
            "tell application frontName to quit",
        ])),
        "system.restart-finder" => Some(spawn(&["/usr/bin/killall", "Finder"])),
        _ => None,
    }
}

/// Every system command as a registry [`Item`]: id = the stable
/// `system.*` key, title = the display name, subtitle = a one-line
/// description, kind = SystemCommand. The list is a constant, so order
/// and content are deterministic by construction.
pub fn items() -> Vec<Item> {
    let mut out: Vec<Item> = SPECS
        .iter()
        .map(|s| Item::new(s.id, s.title, s.subtitle, ItemKind::SystemCommand))
        .collect();
    out.extend(SETTINGS_PANES.iter().map(|(slug, title, _)| {
        Item::new(
            &format!("settings.{slug}"),
            title,
            "Open in System Settings",
            ItemKind::SystemCommand,
        )
    }));
    out
}

/// Execute the system command `id`. An unknown id is an Err, never a
/// panic. For spawned mechanisms the child runs to completion and a
/// failure's Err is its stderr (this is where the macOS Automation
/// denial text lands, so the launcher can show the user why nothing
/// happened); see the module docs for the first-use prompt blocking
/// behavior.
pub fn activate(id: &str) -> Result<(), String> {
    let Some(mech) = mechanism(id) else {
        return Err(format!("unknown system command id: {id}"));
    };
    match mech {
        Mechanism::Spawn(argv) => run_spawn(&argv),
        Mechanism::Call { framework, symbol } => run_call(framework, symbol),
    }
}

/// Run one Spawn mechanism: spawn, wait, map failure to a message worth
/// showing. A nonzero exit prefers the child's stderr; an empty stderr
/// falls back to the exit status.
fn run_spawn(argv: &[String]) -> Result<(), String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("empty argv".to_string());
    };
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("{program} failed with {}", output.status))
    } else {
        Err(stderr.to_string())
    }
}

// The two dlfcn calls behind the Call mechanism. These live in libSystem,
// which every macOS binary links, so no #[link] attribute is needed.
extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// Run one Call mechanism: resolve `symbol` from `framework` and invoke
/// it as a zero-argument C function. The dlopen handle is deliberately
/// never closed; the framework staying resident is fine (and unloading
/// system frameworks is not supported anyway).
fn run_call(framework: &str, symbol: &str) -> Result<(), String> {
    let path = CString::new(framework).map_err(|e| format!("bad framework path: {e}"))?;
    let name = CString::new(symbol).map_err(|e| format!("bad symbol name: {e}"))?;
    // Safety: dlopen and dlsym are declared with their documented C
    // signatures and both results are null-checked before use. The
    // transmute asserts the symbol is a zero-argument C function, which
    // is SACLockScreenImmediate's shape; its return value has no
    // documented meaning, so the call goes through a fn() type that
    // never reads a return register.
    unsafe {
        let handle = dlopen(path.as_ptr(), RTLD_NOW);
        if handle.is_null() {
            return Err(format!("dlopen failed for {framework}"));
        }
        let sym = dlsym(handle, name.as_ptr());
        if sym.is_null() {
            return Err(format!("{symbol} not found in {framework}"));
        }
        let func = std::mem::transmute::<*mut c_void, extern "C" fn()>(sym);
        func();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Safety rule for this module's tests: sleep, lock screen, empty
    // trash, quit frontmost, and restart Finder are NEVER executed here;
    // their mechanisms are locked by the golden test instead. The only
    // live executions are harmless: failure paths (unknown id, missing
    // binary, nonzero exit), /usr/bin/true, and the ignored hardware
    // test, which restores the exact volume and mute state it touches.

    #[test]
    fn items_shape_and_order_are_golden() {
        let items = items();
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "system.sleep",
                "system.lock-screen",
                "system.empty-trash",
                "system.toggle-dark-mode",
                "system.toggle-mute",
                "system.volume-up",
                "system.volume-down",
                "system.quit-frontmost",
                "system.restart-finder",
                "settings.keyboard",
                "settings.displays",
                "settings.bluetooth",
                "settings.wifi",
                "settings.battery",
                "settings.sound",
                "settings.notifications",
                "settings.appearance",
                "settings.accessibility",
                "settings.privacy",
                "settings.trackpad",
                "settings.dock",
                "settings.software-update",
                "settings.network",
                "settings.screen-time",
            ]
        );
        for item in &items {
            assert_eq!(item.kind, ItemKind::SystemCommand);
            assert!(
                item.id.starts_with("system.") || item.id.starts_with("settings."),
                "bad id: {}",
                item.id
            );
            assert!(!item.title.is_empty());
            assert!(!item.subtitle.is_empty(), "no subtitle on {}", item.id);
            assert!(
                !item.subtitle.contains('\n'),
                "subtitle is not one line on {}",
                item.id
            );
        }
    }

    #[test]
    fn items_is_deterministic_with_unique_ids() {
        assert_eq!(items(), items());
        let mut ids: Vec<String> = items().into_iter().map(|i| i.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate ids in SPECS");
    }

    #[test]
    fn unknown_id_is_an_err_not_a_panic() {
        assert!(activate("system.does-not-exist").is_err());
        assert!(activate("").is_err());
        assert!(activate("app.safari").is_err());
        assert!(mechanism("system.does-not-exist").is_none());
    }

    #[test]
    fn every_item_has_a_mechanism() {
        for spec in SPECS {
            assert!(mechanism(spec.id).is_some(), "no mechanism for {}", spec.id);
        }
    }

    // Golden: the exact argv (or FFI target) of every command. This is
    // the whole activation behavior for the destructive commands, which
    // are verified here and never executed by tests.
    #[test]
    fn golden_mechanism_per_command() {
        let argv = |parts: &[&str]| {
            Some(Mechanism::Spawn(
                parts.iter().map(|p| p.to_string()).collect(),
            ))
        };
        assert_eq!(
            mechanism("system.sleep"),
            argv(&["/usr/bin/pmset", "sleepnow"])
        );
        assert_eq!(
            mechanism("system.lock-screen"),
            Some(Mechanism::Call {
                framework: "/System/Library/PrivateFrameworks/login.framework/Versions/A/login",
                symbol: "SACLockScreenImmediate",
            })
        );
        assert_eq!(
            mechanism("system.empty-trash"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "tell application \"Finder\" to empty trash",
            ])
        );
        assert_eq!(
            mechanism("system.toggle-dark-mode"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "tell application \"System Events\" to tell appearance preferences \
                 to set dark mode to not dark mode",
            ])
        );
        assert_eq!(
            mechanism("system.toggle-mute"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "set volume output muted (not (output muted of (get volume settings)))",
            ])
        );
        assert_eq!(
            mechanism("system.volume-up"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "set cur to output volume of (get volume settings)",
                "-e",
                "set volume output volume (cur + 6)",
            ])
        );
        assert_eq!(
            mechanism("system.volume-down"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "set cur to output volume of (get volume settings)",
                "-e",
                "set volume output volume (cur - 6)",
            ])
        );
        assert_eq!(
            mechanism("system.quit-frontmost"),
            argv(&[
                "/usr/bin/osascript",
                "-e",
                "tell application \"System Events\" to set frontName to name of \
                 first application process whose frontmost is true",
                "-e",
                "tell application frontName to quit",
            ])
        );
        assert_eq!(
            mechanism("system.restart-finder"),
            argv(&["/usr/bin/killall", "Finder"])
        );
    }

    #[test]
    fn spawn_outcomes_map_to_results() {
        // Success: a real binary that exits 0.
        assert_eq!(run_spawn(&["/usr/bin/true".to_string()]), Ok(()));
        // Missing binary: the spawn itself fails, named in the message.
        let err = run_spawn(&["/nonexistent/beckon-test-binary".to_string()]).unwrap_err();
        assert!(err.contains("/nonexistent/beckon-test-binary"), "{err}");
        // Nonzero exit with empty stderr: the status is the message.
        let err = run_spawn(&["/usr/bin/false".to_string()]).unwrap_err();
        assert!(err.contains("/usr/bin/false"), "{err}");
        // Degenerate input never panics.
        assert!(run_spawn(&[]).is_err());
    }

    /// Read one osascript expression's stdout, for the hardware test.
    fn osascript_read(script: &str) -> String {
        let out = std::process::Command::new(OSASCRIPT)
            .args(["-e", script])
            .output()
            .expect("osascript runs");
        assert!(out.status.success(), "osascript read failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn read_volume() -> i64 {
        osascript_read("output volume of (get volume settings)")
            .parse()
            .expect("volume is an integer")
    }

    fn read_muted() -> String {
        osascript_read("output muted of (get volume settings)")
    }

    // Hardware check, excluded from the gate because it briefly moves
    // the output volume and mute state (both restored exactly). Run
    // manually:
    //     cargo test -p beckon-macos volume_round_trip -- --ignored
    #[test]
    #[ignore = "briefly changes output volume and mute; run manually on hardware"]
    fn volume_round_trip_on_hardware() {
        let original = read_volume();
        let original_muted = read_muted();

        activate("system.volume-up").expect("volume up");
        let up = read_volume();
        assert_eq!(up, (original + 6).min(100));

        activate("system.volume-down").expect("volume down");
        assert_eq!(read_volume(), up - 6);

        activate("system.toggle-mute").expect("mute toggle");
        assert_ne!(read_muted(), original_muted);
        activate("system.toggle-mute").expect("mute toggle back");
        assert_eq!(read_muted(), original_muted);

        // Restore the exact starting volume, then prove it stuck.
        run_spawn(&[
            OSASCRIPT.to_string(),
            "-e".to_string(),
            format!("set volume output volume {original}"),
        ])
        .expect("volume restore");
        assert_eq!(read_volume(), original);
    }
}
