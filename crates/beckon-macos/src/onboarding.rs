//! Accessibility permission onboarding: detect the missing grant,
//! explain what needs it, and hand the user a one-keystroke path to the
//! right System Settings pane. Window management (winmgmt.rs), menu item
//! search, and synthesized paste (paste.rs) all require the Accessibility
//! grant; everything else in beckon works without it, so the launcher
//! degrades gracefully and this module is the explanation.
//!
//! Shape: while the process is NOT trusted, [`items`] contributes exactly
//! one registry item, "Grant Accessibility Access"; once the grant lands
//! the item disappears (the list goes empty), so onboarding is
//! self-retiring. [`activate`] fires the system grant dialog via
//! [`ax::prompt_for_trust`] AND opens the Accessibility privacy pane
//! directly, because the system dialog is shown at most once per grant
//! state and a user who dismissed it once would otherwise be stranded.
//!
//! The pane is opened with /usr/bin/open on the URL
//! "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility".
//! That URL scheme is the long-stable one: it has survived every release
//! from pre-Yosemite System Preferences through the macOS 13 System
//! Settings rewrite and still resolves on current macOS. If open fails
//! anyway (scheme retired by some future release), the fallback launches
//! the plain Settings app by bundle id and the user navigates to Privacy
//! and Security manually; both failing is the Err.
//!
//! Note on the trust reads: AXIsProcessTrusted reflects the grant of the
//! RUNNING process (or its responsible process when launched from a
//! terminal). Trust state can change while beckon runs; macOS does not
//! reliably re-evaluate the grant for a live process, so the item
//! subtitle tells the user to relaunch after granting.
//!
//! Testing: the two trust worlds cannot be forced from a test, so the
//! public fns delegate to pure `items_for(trusted)` and
//! `status_for(trusted)` twins, which are golden-tested in both worlds;
//! activate's argv is locked by golden tests without spawning anything
//! (tests never pop System Settings on the machine running the gate).

// Wired into the engine by the integrator; until that lands nothing in
// main calls this module, so the dead-code lint is silenced file-wide.
// Remove the allow with the first caller.
#![allow(dead_code)]

use crate::ax;
use beckon_core::router::{Item, ItemKind};

/// The one onboarding item's stable id (also its frecency key).
pub const ACCESSIBILITY_ID: &str = "onboarding.accessibility";

/// The System Settings deep link to the Accessibility privacy pane. See
/// the module docs for the stability story of this scheme.
const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Bundle id of the Settings app (System Preferences before macOS 13,
/// System Settings after; the bundle id never changed). The fallback
/// target when the deep link fails.
const SETTINGS_BUNDLE_ID: &str = "com.apple.systempreferences";

/// Pure twin of [`items`]: the registry contribution for a given trust
/// state. Trusted means nothing to onboard, so the list is empty and the
/// item disappears from the launcher; untrusted means exactly one item.
fn items_for(trusted: bool) -> Vec<Item> {
    if trusted {
        return Vec::new();
    }
    vec![Item::new(
        ACCESSIBILITY_ID,
        "Grant Accessibility Access",
        "Window management and paste need this; approve beckon in System Settings, then relaunch",
        ItemKind::SystemCommand,
    )]
}

/// Onboarding items for the current live trust state. Empty once the
/// grant is in place.
pub fn items() -> Vec<Item> {
    items_for(ax::is_trusted())
}

/// Pure twin of [`status_line`].
fn status_for(trusted: bool) -> String {
    if trusted {
        "Accessibility: granted".to_string()
    } else {
        "Accessibility: not granted (window management and paste disabled)".to_string()
    }
}

/// One-line trust status for logs and a future doctor flag.
pub fn status_line() -> String {
    status_for(ax::is_trusted())
}

/// The argv that opens the Accessibility privacy pane directly.
fn settings_argv() -> Vec<String> {
    vec!["/usr/bin/open".to_string(), SETTINGS_URL.to_string()]
}

/// The argv of the fallback: launch the plain Settings app by bundle id.
fn settings_fallback_argv() -> Vec<String> {
    vec![
        "/usr/bin/open".to_string(),
        "-b".to_string(),
        SETTINGS_BUNDLE_ID.to_string(),
    ]
}

/// Activate the onboarding item: ask macOS to show the grant dialog
/// (a no-op if it was already shown for this grant state), then open the
/// Accessibility pane so the user can flip the switch either way. An
/// unknown id is an Err, never a panic.
pub fn activate(id: &str) -> Result<(), String> {
    if id != ACCESSIBILITY_ID {
        return Err(format!("unknown onboarding command id: {id}"));
    }
    // The prompt result is deliberately ignored: it reports the CURRENT
    // trust state, and whether the dialog appeared or not the settings
    // pane is the reliable destination.
    let _ = ax::prompt_for_trust();
    match run_spawn(&settings_argv()) {
        Ok(()) => Ok(()),
        Err(first) => run_spawn(&settings_fallback_argv()).map_err(|second| {
            format!(
                "cannot open Accessibility settings: {first}; \
                 launching System Settings failed too: {second}"
            )
        }),
    }
}

/// Spawn argv[0] with the rest as arguments, wait, and surface stderr as
/// the Err on failure. A local mirror of the Spawn mechanism runner in
/// system.rs, which is private to that module (and this module must not
/// modify it; the integrator may unify the two later).
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

#[cfg(test)]
mod tests {
    use super::*;

    // Safety rule for this module's tests: activate(ACCESSIBILITY_ID) is
    // NEVER called here, because it would fire the system trust prompt
    // and pop System Settings on the machine running the gate. The open
    // argv is locked by golden tests instead; the only live executions
    // are pure functions and the unknown-id failure path (which spawns
    // nothing).

    #[test]
    fn golden_item_when_untrusted() {
        let items = items_for(false);
        assert_eq!(items.len(), 1, "exactly one onboarding item");
        let item = &items[0];
        assert_eq!(item.id, "onboarding.accessibility");
        assert_eq!(item.title, "Grant Accessibility Access");
        assert_eq!(
            item.subtitle,
            "Window management and paste need this; approve beckon in System Settings, \
             then relaunch"
        );
        assert_eq!(item.kind, ItemKind::SystemCommand);
        assert!(!item.subtitle.contains('\n'), "subtitle is one line");
    }

    #[test]
    fn no_items_when_trusted() {
        assert!(
            items_for(true).is_empty(),
            "the item must disappear once the grant lands"
        );
    }

    #[test]
    fn golden_status_lines() {
        assert_eq!(status_for(true), "Accessibility: granted");
        assert_eq!(
            status_for(false),
            "Accessibility: not granted (window management and paste disabled)"
        );
    }

    #[test]
    fn golden_open_argv() {
        assert_eq!(
            settings_argv(),
            vec![
                "/usr/bin/open".to_string(),
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                    .to_string(),
            ]
        );
        assert_eq!(
            settings_fallback_argv(),
            vec![
                "/usr/bin/open".to_string(),
                "-b".to_string(),
                "com.apple.systempreferences".to_string(),
            ]
        );
    }

    #[test]
    fn unknown_id_is_an_err_not_a_panic() {
        assert!(activate("onboarding.does-not-exist").is_err());
        assert!(activate("").is_err());
        assert!(activate("system.sleep").is_err());
    }

    // The public fns must be exactly their pure twins evaluated at the
    // live trust state. Run with --nocapture to see the observed state
    // on this machine.
    #[test]
    fn public_fns_delegate_to_the_pure_twins() {
        let trusted = ax::is_trusted();
        println!("observed live trust state: trusted={trusted}");
        assert_eq!(items(), items_for(trusted));
        assert_eq!(status_line(), status_for(trusted));
    }
}
