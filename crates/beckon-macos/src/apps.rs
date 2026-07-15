//! Application indexer and launcher.
//!
//! Indexing walks the standard application directories with std::fs and
//! asks the frameworks, not the filesystem, what each bundle is called:
//! Info.plist is never parsed by hand (modern ones are binary plists).
//! The localized name comes from NSFileManager displayNameAtPath: (falling
//! back to the .app filename stem) and the stable id from NSBundle
//! bundleIdentifier (falling back to the absolute bundle path). Duplicate
//! ids collapse to the copy from the most canonical location, /Applications
//! first, then user locations, then /System/Applications, and the final
//! list is sorted by (title, id) so the index is a pure function of what
//! is installed.
//!
//! Launching goes through NSWorkspace's modern
//! openApplicationAtURL:configuration:completionHandler:. The deprecated
//! launchApplication: would be simpler, but the modern call is the
//! supported API and its default NSWorkspaceOpenConfiguration has
//! activates = YES, so an already-running app is brought to the front,
//! the exact behavior a launcher needs (verified on hardware with
//! Calculator). The completion handler is nil by design: beckon's run
//! loop stays alive, the call is fire and forget, and the one failure
//! mode worth reporting synchronously (no bundle at the path) is checked
//! before the call.

use crate::ffi::{self, msg, Id};
use beckon_core::router::{Item, ItemKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The directories scanned for .app bundles, paired with whether the scan
/// descends one level into subdirectories (vendor folders like
/// /Applications/Adobe). Utilities folders are listed explicitly; the id
/// dedupe absorbs the overlap with the /Applications descent.
fn roots() -> Vec<(PathBuf, bool)> {
    let mut roots = vec![
        (PathBuf::from("/Applications"), true),
        (PathBuf::from("/Applications/Utilities"), false),
        (PathBuf::from("/System/Applications"), false),
        (PathBuf::from("/System/Applications/Utilities"), false),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push((PathBuf::from(home).join("Applications"), false));
    }
    roots
}

/// Where a bundle path ranks when two copies share a bundle id. Lower
/// wins: the /Applications copy beats the /System/Applications copy.
fn source_rank(path: &str) -> u8 {
    if path.starts_with("/Applications/") {
        0
    } else if path.starts_with("/System/") {
        2
    } else {
        // ~/Applications and anything else user-local.
        1
    }
}

/// Collect .app bundle paths under `dir`, skipping hidden entries.
/// `descend` walks one level into non-bundle subdirectories; recursion
/// stops there because vendor folders do not nest.
fn scan_dir(dir: &Path, descend: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // metadata follows symlinks, so a symlinked bundle in
        // ~/Applications still counts as a directory.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if name.ends_with(".app") {
            out.push(path);
        } else if descend {
            scan_dir(&path, false, out);
        }
    }
}

/// Localized display name for the bundle at `path`, minus any ".app"
/// suffix Finder settings leave on. None when the framework has nothing.
fn display_name(path: &str) -> Option<String> {
    // Safety: displayNameAtPath: takes an NSString and returns an
    // autoreleased NSString or nil, which nsstring_to_string accepts.
    // NSFileManager's shared instance is documented thread-safe.
    let name = unsafe {
        let fm = msg!(Id: ffi::class("NSFileManager"), ffi::sel("defaultManager"));
        let ns = msg!(Id: fm, ffi::sel("displayNameAtPath:"), Id: ffi::nsstring(path));
        ffi::nsstring_to_string(ns)
    };
    let name = name
        .strip_suffix(".app")
        .unwrap_or(&name)
        .trim()
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Stable bundle identifier for the bundle at `path`. None when NSBundle
/// rejects the path or the bundle declares no identifier.
fn bundle_id(path: &str) -> Option<String> {
    // Safety: bundleWithPath: takes an NSString and returns an NSBundle or
    // nil; bundleIdentifier returns an NSString or nil. Both nils are
    // handled here or inside nsstring_to_string.
    let id = unsafe {
        let bundle = msg!(Id: ffi::class("NSBundle"), ffi::sel("bundleWithPath:"),
            Id: ffi::nsstring(path));
        if bundle.is_null() {
            return None;
        }
        ffi::nsstring_to_string(msg!(Id: bundle, ffi::sel("bundleIdentifier")))
    };
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

/// Index every installed application as a registry [`Item`]: id = bundle
/// identifier (or the absolute path when the bundle has none), title =
/// localized display name, subtitle = absolute bundle path, kind = App.
/// Deduped by id and sorted by (title, id), so the output is
/// deterministic for a given set of installed apps.
pub fn index() -> Vec<Item> {
    // The framework calls return autoreleased objects and this can run
    // outside the app run loop (the --index-apps flag), so hold a pool.
    let _pool = ffi::AutoreleasePool::new();

    let mut paths: Vec<PathBuf> = Vec::new();
    for (root, descend) in roots() {
        scan_dir(&root, descend, &mut paths);
    }

    // Best item per id. The tie-break (rank, then path) is order
    // independent, so scan order never changes the result.
    let mut best: HashMap<String, (u8, Item)> = HashMap::new();
    for path in &paths {
        let Some(path) = path.to_str() else {
            continue;
        };
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        let title = display_name(path).unwrap_or_else(|| stem.to_string());
        let id = bundle_id(path).unwrap_or_else(|| path.to_string());
        let rank = source_rank(path);
        let item = Item::new(&id, &title, path, ItemKind::App);
        match best.entry(id) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert((rank, item));
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let (held_rank, held) = slot.get();
                if (rank, &item.subtitle) < (*held_rank, &held.subtitle) {
                    slot.insert((rank, item));
                }
            }
        }
    }

    let mut items: Vec<Item> = best.into_values().map(|(_, item)| item).collect();
    items.sort_by(|a, b| (&a.title, &a.id).cmp(&(&b.title, &b.id)));
    items
}

/// Open the .app bundle at `path` via NSWorkspace. Errors are the
/// synchronous preconditions only (no bundle at the path); the open
/// itself is asynchronous with a nil completion handler, as documented
/// in the module header.
pub fn launch(path: &str) -> Result<(), String> {
    if !path.ends_with(".app") {
        return Err(format!("not an .app bundle: {path}"));
    }
    if !Path::new(path).is_dir() {
        return Err(format!("no app bundle at {path}"));
    }
    let _pool = ffi::AutoreleasePool::new();
    // Safety: fileURLWithPath: takes an NSString and returns an NSURL or
    // nil (handled); openApplicationAtURL:configuration:completionHandler:
    // takes an NSURL, an NSWorkspaceOpenConfiguration, and a nullable
    // block, passed here as nil.
    unsafe {
        let url = msg!(Id: ffi::class("NSURL"), ffi::sel("fileURLWithPath:"),
            Id: ffi::nsstring(path));
        if url.is_null() {
            return Err(format!("NSURL rejected path {path}"));
        }
        let workspace = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
        let config =
            msg!(Id: ffi::class("NSWorkspaceOpenConfiguration"), ffi::sel("configuration"));
        msg!((): workspace,
            ffi::sel("openApplicationAtURL:configuration:completionHandler:"),
            Id: url, Id: config, Id: ffi::NIL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // This module only compiles on macOS (the mod declaration in main.rs
    // is cfg-gated), so these tests run against the real machine.

    #[test]
    fn index_is_nonempty_sorted_deduped_and_all_app_bundles() {
        let items = index();
        assert!(
            items.len() >= 10,
            "expected dozens of apps, found {}",
            items.len()
        );
        // Sorted by (title, id); ids unique makes the order strict.
        for pair in items.windows(2) {
            assert!(
                (&pair[0].title, &pair[0].id) < (&pair[1].title, &pair[1].id),
                "not sorted or not deduped: {:?} then {:?}",
                (&pair[0].title, &pair[0].id),
                (&pair[1].title, &pair[1].id)
            );
        }
        let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate ids survived the dedupe");
        for item in &items {
            assert_eq!(item.kind, ItemKind::App);
            assert!(
                item.subtitle.ends_with(".app"),
                "subtitle is not a bundle path: {}",
                item.subtitle
            );
            assert!(
                item.subtitle.starts_with('/'),
                "subtitle is not absolute: {}",
                item.subtitle
            );
        }
    }

    #[test]
    fn index_dedupes_toward_slash_applications() {
        // Rank order is the whole dedupe policy; lock it.
        assert!(
            source_rank("/Applications/Safari.app") < source_rank("/Users/x/Applications/A.app")
        );
        assert!(
            source_rank("/Users/x/Applications/A.app")
                < source_rank("/System/Applications/Calculator.app")
        );
    }

    #[test]
    fn launch_rejects_missing_or_non_bundle_paths() {
        assert!(launch("/Applications/Definitely Not Installed.app").is_err());
        assert!(launch("/usr/bin/true").is_err());
    }

    // Hardware check, excluded from the gate because it visibly opens an
    // app. Run manually:
    //     cargo test -p beckon-macos launch_calculator -- --ignored
    #[test]
    #[ignore = "opens Calculator; run manually on hardware"]
    fn launch_calculator_on_hardware() {
        launch("/System/Applications/Calculator.app").expect("launch dispatch failed");
        // The open request travels over XPC on background threads; keep
        // the process alive long enough for it to leave. (An NSRunLoop
        // pump returns immediately here because the test thread's loop
        // has no sources.)
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
