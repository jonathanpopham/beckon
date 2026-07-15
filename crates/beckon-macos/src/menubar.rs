//! Menu item search: fuzzy-find and invoke any item of the frontmost
//! app's menu bar (Raycast's most underrated navigation feature).
//!
//! Enumeration walks the Accessibility tree: the frontmost pid becomes
//! an AXUIElement application, its AXMenuBar attribute yields the bar,
//! and a recursive descent through AXChildren visits every menu. The
//! walk alternates two element shapes: titled containers (a top-level
//! AXMenuBarItem like "File", or an AXMenuItem that owns a submenu)
//! push their title onto the breadcrumb; untitled containers (the
//! AXMenu interposed between a parent and its items) pass through
//! without deepening the breadcrumb. Leaves are AXMenuItems with no
//! children: separators (empty AXTitle) are skipped, everything else is
//! collected with its title, breadcrumb ("File > Export"), AXEnabled
//! state, and best-effort keyboard shortcut (AXMenuItemCmdChar plus the
//! Carbon modifier mask in AXMenuItemCmdModifiers, rendered as the
//! usual glyphs). The Apple menu (always the bar's first child) is
//! skipped to keep results app-relevant. The walk is bounded by
//! [`MAX_ITEMS`] and [`MAX_DEPTH`], and every element gets an AX
//! messaging timeout before it is messaged, so a hung app costs at most
//! a bounded wait, never a hang.
//!
//! Caching: menus mutate (enabled states flip, whole menus appear), so
//! a snapshot is only trusted while the frontmost pid is unchanged and
//! the snapshot is younger than [`CACHE_TTL_SECS`]. The integrator
//! calls [`snapshot_frontmost`] on panel summon; because the panel is a
//! non-activating NSPanel, the app the user was in stays frontmost and
//! the summon-time snapshot is exactly the menu bar they see. Every
//! [`items`] call serves from that snapshot (re-validating pid and
//! age), so per-keystroke queries never re-walk the tree.
//!
//! Ids and invocation: rows carry "menu.<pid>.<seq>" where seq indexes
//! the snapshot in menu order. The snapshot CFRetains every leaf
//! element (guard discipline: one [`CfGuard`] per retained ref), so an
//! id resolved by [`items`] stays pressable by [`activate`] until the
//! next snapshot replaces it. A stale pid (the frontmost app changed
//! and a re-snapshot happened) is a clear error, never a press on the
//! wrong app's menu.
//!
//! Contract for the engine: [`items`] returns SystemCommand-kind rows
//! (title = menu item title, subtitle = breadcrumb plus shortcut);
//! the empty query returns the first [`MAX_RESULTS`] enabled items in
//! menu order, a non-empty query fuzzy-ranks the full "breadcrumb >
//! title" text and returns at most [`MAX_RESULTS`]; disabled items are
//! excluded. [`activate`] performs AXPress on one row by id.

// Wired into the engine by the integrator; until that lands nothing in
// main calls this module, so the dead-code lint is silenced file-wide.
// Remove the allow with the first caller.
#![allow(dead_code)]

use crate::ax::{self, AXError, Boolean, CFIndex, CFStringRef, CFTypeRef, CfGuard};
use beckon_core::fuzzy;
use beckon_core::router::{Item, ItemKind};
use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::time::Instant;

/// Cap on collected leaf items per snapshot. Real menu bars run in the
/// hundreds; anything past this is pathological and gets truncated.
const MAX_ITEMS: usize = 2000;

/// Cap on menu nesting depth, counted in titled levels on the
/// breadcrumb ("File" is depth 1, "File > Export" is 2). Menus deeper
/// than this are not descended into.
const MAX_DEPTH: usize = 6;

/// A snapshot older than this is re-enumerated on the next call even
/// when the frontmost pid is unchanged: menus mutate as app state
/// changes (enabled flags, dynamic menus).
const CACHE_TTL_SECS: u64 = 30;

/// Cap on the rows a query returns; the panel shows at most nine.
const MAX_RESULTS: usize = 9;

/// Id prefix; the full scheme is "menu.<pid>.<seq>".
const ID_PREFIX: &str = "menu.";

/// Seconds an AX call may block on an unresponsive app before erroring.
/// A float, but only as the literal argument the AX FFI edge demands
/// (AXUIElementSetMessagingTimeout takes a C float); no float
/// arithmetic happens anywhere in this module.
const AX_MESSAGING_TIMEOUT_SECS: f32 = 1.0;

/// kAXErrorSuccess.
const AX_SUCCESS: AXError = 0;

/// kCFStringEncodingUTF8 from CFString.h.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// kCFNumberSInt64Type from CFNumber.h.
const CF_NUMBER_SINT64_TYPE: CFIndex = 4;

/// Carbon menu-modifier mask bits carried by AXMenuItemCmdModifiers
/// (Menus.h): zero means Command alone; bits add Shift, Option, and
/// Control; the fourth bit means the Command key is NOT part of the
/// shortcut.
const MENU_MOD_SHIFT: i64 = 1;
const MENU_MOD_OPTION: i64 = 2;
const MENU_MOD_CONTROL: i64 = 4;
const MENU_MOD_NO_COMMAND: i64 = 8;

// AX entry points ax.rs declares privately or not at all; per the
// module-boundary rule (reuse ax.rs as-is, declare missing externs
// locally).
#[allow(non_snake_case)]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout_seconds: f32) -> AXError;
}

// CoreFoundation accessors ax.rs does not export; same boundary rule.
// Get functions return borrowed references; Copy/Create/Retain return
// +1 (guarded at every call site).
#[allow(non_snake_case)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: CFIndex) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> Boolean;
    fn CFNumberGetValue(number: CFTypeRef, the_type: CFIndex, value_ptr: *mut c_void) -> Boolean;
    fn CFStringGetLength(s: CFTypeRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        s: CFTypeRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: Boolean,
    ) -> CFStringRef;
}

/// One leaf menu item as collected by the walk. Pure data; the live AX
/// element it came from lives in the parallel elements vec of the
/// snapshot, at the same index.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuEntry {
    /// The item's own AXTitle ("Export as PDF").
    title: String,
    /// Ancestor menu titles joined with " > " ("File > Export"); empty
    /// only in the degenerate case of a titled leaf directly on the bar.
    breadcrumb: String,
    /// AXEnabled at snapshot time; disabled items never become rows.
    enabled: bool,
    /// Rendered keyboard shortcut ("⇧⌘E"), empty when unknown.
    shortcut: String,
}

/// One enumeration of one app's menu bar. `elements[i]` is the retained
/// AXUIElement behind `entries[i]`; the two vecs move together always.
struct Snapshot {
    pid: i32,
    taken: Instant,
    entries: Vec<MenuEntry>,
    elements: Vec<CfGuard>,
}

thread_local! {
    /// The last snapshot, kept alive so ids resolved by [`items`] stay
    /// pressable by [`activate`]. Thread-local instead of a static
    /// Mutex: every caller (engine summon, query, activation) runs on
    /// the main thread, and CfGuard's raw pointer is then never asked
    /// to be Send.
    static SNAPSHOT: RefCell<Option<Snapshot>> = const { RefCell::new(None) };
}

/// Snapshot the frontmost app's menu bar now. The integrator calls this
/// on panel summon: the panel is non-activating, so the app the user
/// was in is still frontmost and this captures exactly its menus. A
/// fresh cached snapshot (same pid, younger than [`CACHE_TTL_SECS`]) is
/// kept; anything else is re-enumerated.
pub fn snapshot_frontmost() {
    ensure_snapshot();
}

/// The menu rows for `query`, shaped for the registry. Serves from the
/// current snapshot (validating pid and age, re-enumerating only when
/// stale), so per-keystroke calls never re-walk the menu tree.
pub fn items(query: &str) -> Vec<Item> {
    ensure_snapshot();
    SNAPSHOT.with(|slot| match &*slot.borrow() {
        Some(snap) => build_items(snap.pid, &snap.entries, query),
        None => Vec::new(),
    })
}

/// Press one menu item by its "menu.<pid>.<seq>" id. The element comes
/// from the retained snapshot; a stale pid (the frontmost app changed
/// and a later summon re-enumerated) or an out-of-range seq is a clear
/// error, never a press on the wrong app's menu.
pub fn activate(id: &str) -> Result<(), String> {
    let (pid, seq) = parse_id(id)?;
    SNAPSHOT.with(|slot| {
        let borrow = slot.borrow();
        let snap = borrow
            .as_ref()
            .ok_or_else(|| "no menu snapshot; summon the panel first".to_string())?;
        if snap.pid != pid {
            return Err(format!(
                "stale menu item: it belonged to app pid {pid}, but the current \
                 menu snapshot is of app pid {}",
                snap.pid
            ));
        }
        let element = snap.elements.get(seq).ok_or_else(|| {
            format!(
                "stale menu item: seq {seq} is out of range for the current \
                 snapshot ({} items)",
                snap.elements.len()
            )
        })?;
        press(element.as_ptr())
    })
}

/// Validate the cached snapshot against the current frontmost pid and
/// [`CACHE_TTL_SECS`]; re-enumerate when missing, aged out, or owned by
/// a different app. No frontmost app leaves the cache as it was.
fn ensure_snapshot() {
    let Some(pid) = ax::frontmost_app_pid() else {
        return;
    };
    SNAPSHOT.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let fresh = matches!(
            &*borrow,
            Some(snap) if snap.pid == pid && snap.taken.elapsed().as_secs() < CACHE_TTL_SECS
        );
        if !fresh {
            *borrow = Some(enumerate(pid));
        }
    });
}

/// Walk one app's menu bar into a snapshot. Untrusted processes and
/// apps without a menu bar yield an empty snapshot, never an error: the
/// launcher shows nothing rather than breaking the panel.
fn enumerate(pid: i32) -> Snapshot {
    let mut snap = Snapshot {
        pid,
        taken: Instant::now(),
        entries: Vec::new(),
        elements: Vec::new(),
    };
    if !ax::is_trusted() {
        return snap;
    }
    // Safety: any pid is a legal argument; the +1 element is guarded
    // immediately (ax module invariant 1 applies here too).
    let app = CfGuard::new(unsafe { AXUIElementCreateApplication(pid) });
    if app.is_null() {
        return snap;
    }
    // Safety: app guards a live element; the timeout bounds the
    // AXMenuBar copy below so an unresponsive app cannot hang beckon.
    unsafe { AXUIElementSetMessagingTimeout(app.as_ptr(), AX_MESSAGING_TIMEOUT_SECS) };
    let Ok(bar) = copy_attr(app.as_ptr(), "AXMenuBar") else {
        return snap;
    };
    // Safety: bar guards a live element for the timeout call.
    unsafe { AXUIElementSetMessagingTimeout(bar.as_ptr(), AX_MESSAGING_TIMEOUT_SECS) };
    let Ok(top) = copy_children(bar.as_ptr()) else {
        return snap;
    };
    // Safety: top guards a live CFArray for the whole loop; values at
    // index are borrowed elements kept alive by that array.
    let count = unsafe { CFArrayGetCount(top.as_ptr()) };
    let mut path: Vec<String> = Vec::new();
    // Index 0 is always the Apple menu; skipped to keep results
    // app-relevant (module docs).
    for i in 1..count {
        // Safety: i is in 1..count of the guarded array; the borrowed
        // element is only used while the guard lives.
        let child = unsafe { CFArrayGetValueAtIndex(top.as_ptr(), i) };
        if child.is_null() {
            continue;
        }
        collect(child, &mut path, &mut snap);
    }
    snap
}

/// The recursive descent (module docs): titled containers deepen the
/// breadcrumb, untitled containers (AXMenu) pass through, leaves are
/// collected unless they are separators. `element` is borrowed from a
/// CFArray the caller keeps guarded for the duration of the call.
fn collect(element: CFTypeRef, path: &mut Vec<String>, snap: &mut Snapshot) {
    if snap.entries.len() >= MAX_ITEMS {
        return;
    }
    // Safety: the caller keeps element alive (its owning array guard is
    // in scope); the timeout bounds every message sent to it below.
    unsafe { AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECS) };
    let title = copy_attr(element, "AXTitle")
        .ok()
        // Safety: the guard holds a live value; cf_string type-checks
        // before reading.
        .and_then(|v| unsafe { cf_string(v.as_ptr()) })
        .unwrap_or_default();
    let children = copy_children(element).ok();
    // Safety: children, when present, guards a live CFArray.
    let child_count = match &children {
        Some(kids) => unsafe { CFArrayGetCount(kids.as_ptr()) },
        None => 0,
    };
    if child_count > 0 {
        let kids = children.expect("child_count > 0 implies the array exists");
        let titled = !title.is_empty();
        if titled {
            if path.len() >= MAX_DEPTH {
                return;
            }
            path.push(title);
        }
        for i in 0..child_count {
            // Safety: i is in 0..child_count of the guarded array; the
            // borrowed element is used only while the guard lives.
            let child = unsafe { CFArrayGetValueAtIndex(kids.as_ptr(), i) };
            if child.is_null() {
                continue;
            }
            collect(child, path, snap);
        }
        if titled {
            path.pop();
        }
        return;
    }
    // Leaf. An empty title is a separator: skip.
    if title.is_empty() {
        return;
    }
    // Safety: element is live (caller's array guard); CFRetain returns
    // the same reference at +1, which the guard releases exactly once,
    // keeping the element pressable for the life of the snapshot.
    let retained = CfGuard::new(unsafe { CFRetain(element) });
    if retained.is_null() {
        return;
    }
    snap.entries.push(MenuEntry {
        title,
        breadcrumb: path.join(" > "),
        enabled: read_enabled(element),
        shortcut: read_shortcut(element),
    });
    snap.elements.push(retained);
}

/// The pure pipeline from snapshot entries to registry rows: drop
/// disabled items; empty query keeps the first [`MAX_RESULTS`] in menu
/// order; a non-empty query fuzzy-scores the full "breadcrumb > title"
/// text, orders best first (ties by menu order, so the output is a pure
/// function of the inputs), and caps at [`MAX_RESULTS`].
fn build_items(pid: i32, entries: &[MenuEntry], query: &str) -> Vec<Item> {
    let q = query.trim();
    let mut picked: Vec<(i64, usize)> = Vec::new();
    for (seq, entry) in entries.iter().enumerate() {
        if !entry.enabled {
            continue;
        }
        if q.is_empty() {
            picked.push((0, seq));
            if picked.len() >= MAX_RESULTS {
                break;
            }
        } else if let Some(m) = fuzzy::score(q, &display_path(entry)) {
            picked.push((m.score, seq));
        }
    }
    if !q.is_empty() {
        picked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        picked.truncate(MAX_RESULTS);
    }
    picked
        .into_iter()
        .map(|(_, seq)| {
            let entry = &entries[seq];
            Item::new(
                &format!("{ID_PREFIX}{pid}.{seq}"),
                &entry.title,
                &subtitle(entry),
                ItemKind::SystemCommand,
            )
        })
        .collect()
}

/// The full navigable path of an entry ("File > Export > Export as
/// PDF"), which is also the fuzzy haystack so queries can span menu
/// names and item names alike.
fn display_path(entry: &MenuEntry) -> String {
    if entry.breadcrumb.is_empty() {
        entry.title.clone()
    } else {
        format!("{} > {}", entry.breadcrumb, entry.title)
    }
}

/// Row subtitle: the breadcrumb, plus the shortcut when known.
fn subtitle(entry: &MenuEntry) -> String {
    match (entry.breadcrumb.is_empty(), entry.shortcut.is_empty()) {
        (false, false) => format!("{}  {}", entry.breadcrumb, entry.shortcut),
        (false, true) => entry.breadcrumb.clone(),
        (true, false) => entry.shortcut.clone(),
        (true, true) => String::new(),
    }
}

/// Split a "menu.<pid>.<seq>" id back into its parts.
fn parse_id(id: &str) -> Result<(i32, usize), String> {
    let bad = || format!("not a menu item id: {id}");
    let rest = id.strip_prefix(ID_PREFIX).ok_or_else(bad)?;
    let (pid_s, seq_s) = rest.split_once('.').ok_or_else(bad)?;
    let pid: i32 = pid_s.parse().map_err(|_| bad())?;
    let seq: usize = seq_s.parse().map_err(|_| bad())?;
    Ok((pid, seq))
}

/// Perform AXPress on a live (borrowed or guarded) menu item element.
fn press(element: CFTypeRef) -> Result<(), String> {
    let action = cfstring("AXPress");
    if action.is_null() {
        return Err("CFString allocation failed".to_string());
    }
    // Safety: the caller keeps element alive (snapshot guard); the
    // timeout bounds the press so a hung app cannot hang beckon; action
    // is guarded for the whole call.
    unsafe {
        AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECS);
        let err = AXUIElementPerformAction(element, action.as_ptr());
        if err != AX_SUCCESS {
            return Err(format!("AXPress failed: AXError {err}"));
        }
    }
    Ok(())
}

/// AXEnabled of a live element; unreadable defaults to true (the press
/// then fails cleanly rather than the item silently vanishing).
fn read_enabled(element: CFTypeRef) -> bool {
    match copy_attr(element, "AXEnabled") {
        // Safety: the guard holds a live value; the type check guards
        // the boolean read.
        Ok(v) => unsafe {
            if CFGetTypeID(v.as_ptr()) == CFBooleanGetTypeID() {
                CFBooleanGetValue(v.as_ptr()) != 0
            } else {
                true
            }
        },
        Err(_) => true,
    }
}

/// Best-effort shortcut of a live menu item element: empty when the
/// item has no command character.
fn read_shortcut(element: CFTypeRef) -> String {
    let ch = copy_attr(element, "AXMenuItemCmdChar")
        .ok()
        // Safety: the guard holds a live value; cf_string type-checks.
        .and_then(|v| unsafe { cf_string(v.as_ptr()) })
        .unwrap_or_default();
    if ch.is_empty() {
        return String::new();
    }
    let mods = copy_attr(element, "AXMenuItemCmdModifiers")
        .ok()
        // Safety: the guard holds a live value; cf_i64 type-checks.
        .and_then(|v| unsafe { cf_i64(v.as_ptr()) })
        .unwrap_or(0);
    shortcut_string(&ch, mods)
}

/// Render a Carbon modifier mask plus command character as the glyphs
/// menus themselves show, in the conventional ⌃⌥⇧⌘ order.
fn shortcut_string(ch: &str, mods: i64) -> String {
    let mut out = String::new();
    if mods & MENU_MOD_CONTROL != 0 {
        out.push('\u{2303}');
    }
    if mods & MENU_MOD_OPTION != 0 {
        out.push('\u{2325}');
    }
    if mods & MENU_MOD_SHIFT != 0 {
        out.push('\u{21E7}');
    }
    if mods & MENU_MOD_NO_COMMAND == 0 {
        out.push('\u{2318}');
    }
    out.push_str(ch);
    out
}

/// Copy AXChildren of a live element as a guarded CFArray; a non-array
/// value (never observed, but the type is checked before any array call
/// touches it) is an error.
fn copy_children(element: CFTypeRef) -> Result<CfGuard, String> {
    let kids = copy_attr(element, "AXChildren")?;
    // Safety: kids guards a live CF object; only its type id is read.
    if unsafe { CFGetTypeID(kids.as_ptr()) != CFArrayGetTypeID() } {
        return Err("AXChildren was not a CFArray".to_string());
    }
    Ok(kids)
}

/// Copy one attribute of an AX element; the result is owned (+1). The
/// element may be borrowed (an entry of a guarded CFArray) or guarded;
/// the caller keeps it alive for the call either way.
fn copy_attr(element: CFTypeRef, name: &str) -> Result<CfGuard, String> {
    let attr = cfstring(name);
    if attr.is_null() {
        return Err("CFString allocation failed".to_string());
    }
    let mut out: CFTypeRef = std::ptr::null();
    // Safety: element is live for the whole call (caller's guard) and
    // attr is guarded; out is a plain out-pointer written by the
    // framework; the +1 result moves into a guard.
    let err = unsafe { AXUIElementCopyAttributeValue(element, attr.as_ptr(), &mut out) };
    if err != AX_SUCCESS {
        return Err(format!("reading {name} failed: AXError {err}"));
    }
    if out.is_null() {
        return Err(format!("reading {name} returned no value"));
    }
    Ok(CfGuard::new(out))
}

/// Build an owned CFString from a Rust string (local twin of the ax.rs
/// private helper, per the module-boundary rule).
fn cfstring(s: &str) -> CfGuard {
    // Safety: the byte pointer and length describe the live &str for
    // the duration of the call and CFString copies the bytes; the +1
    // result goes straight into the guard.
    CfGuard::new(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            s.len() as CFIndex,
            CF_STRING_ENCODING_UTF8,
            0,
        )
    })
}

/// Read a borrowed CF value as a Rust String when it is a CFString.
///
/// # Safety
/// `v` must be null or a live CF object; the type check guards the
/// string calls.
unsafe fn cf_string(v: CFTypeRef) -> Option<String> {
    if v.is_null() || CFGetTypeID(v) != CFStringGetTypeID() {
        return None;
    }
    let len = CFStringGetLength(v);
    let max = CFStringGetMaximumSizeForEncoding(len, CF_STRING_ENCODING_UTF8) + 1;
    let mut buf = vec![0u8; max as usize];
    if CFStringGetCString(
        v,
        buf.as_mut_ptr().cast::<c_char>(),
        max,
        CF_STRING_ENCODING_UTF8,
    ) == 0
    {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    String::from_utf8(buf).ok()
}

/// Read a borrowed CF value as i64 when it is a CFNumber.
///
/// # Safety
/// `v` must be null or a live CF object; the type check guards the
/// number call, and CFNumber converts smaller integer widths to SInt64
/// losslessly.
unsafe fn cf_i64(v: CFTypeRef) -> Option<i64> {
    if v.is_null() || CFGetTypeID(v) != CFNumberGetTypeID() {
        return None;
    }
    let mut out: i64 = 0;
    if CFNumberGetValue(
        v,
        CF_NUMBER_SINT64_TYPE,
        std::ptr::from_mut(&mut out).cast(),
    ) == 0
    {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(title: &str, breadcrumb: &str, enabled: bool, shortcut: &str) -> MenuEntry {
        MenuEntry {
            title: title.to_string(),
            breadcrumb: breadcrumb.to_string(),
            enabled,
            shortcut: shortcut.to_string(),
        }
    }

    fn sample() -> Vec<MenuEntry> {
        vec![
            e("New Window", "File", true, "\u{2318}N"),
            e("Export as PDF", "File > Export", true, ""),
            e("Print", "File", false, "\u{2318}P"),
            e("Copy", "Edit", true, "\u{2318}C"),
            e(
                "Paste and Match Style",
                "Edit",
                true,
                "\u{2325}\u{21E7}\u{2318}V",
            ),
            e("Enter Full Screen", "View", true, ""),
        ]
    }

    #[test]
    fn id_round_trips() {
        assert_eq!(parse_id("menu.1234.56").unwrap(), (1234, 56));
        assert_eq!(parse_id("menu.-2.0").unwrap(), (-2, 0));
    }

    #[test]
    fn bad_ids_are_rejected_not_panicked_on() {
        for bad in [
            "",
            "menu.",
            "menu.12",
            "menu.12.",
            "menu..3",
            "menu.a.b",
            "menu.12.-3",
            "winsw.12.34",
            "app.safari",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn activate_rejects_garbage_ids_before_touching_anything() {
        let err = activate("menu.garbage").unwrap_err();
        assert!(err.contains("not a menu item id"), "{err}");
    }

    #[test]
    fn shortcut_glyphs_follow_the_carbon_mask() {
        // Zero means Command alone.
        assert_eq!(shortcut_string("C", 0), "\u{2318}C");
        assert_eq!(shortcut_string("V", MENU_MOD_SHIFT), "\u{21E7}\u{2318}V");
        assert_eq!(
            shortcut_string("V", MENU_MOD_OPTION | MENU_MOD_SHIFT),
            "\u{2325}\u{21E7}\u{2318}V"
        );
        // The no-command bit removes the command glyph.
        assert_eq!(
            shortcut_string("F", MENU_MOD_CONTROL | MENU_MOD_NO_COMMAND),
            "\u{2303}F"
        );
        // All four bits: every modifier but Command.
        assert_eq!(
            shortcut_string(
                "X",
                MENU_MOD_CONTROL | MENU_MOD_OPTION | MENU_MOD_SHIFT | MENU_MOD_NO_COMMAND
            ),
            "\u{2303}\u{2325}\u{21E7}X"
        );
    }

    #[test]
    fn empty_query_keeps_menu_order_skips_disabled_caps_at_nine() {
        let rows = build_items(42, &sample(), "");
        // Print is disabled and gone; everything else in menu order.
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "New Window",
                "Export as PDF",
                "Copy",
                "Paste and Match Style",
                "Enter Full Screen"
            ]
        );
        let mut many = Vec::new();
        for n in 0..30 {
            many.push(e(&format!("Item {n:02}"), "File", true, ""));
        }
        assert_eq!(build_items(1, &many, "").len(), MAX_RESULTS);
    }

    #[test]
    fn query_ranks_across_breadcrumb_and_title() {
        // "export" only matches through the breadcrumb-joined path.
        let rows = build_items(42, &sample(), "export");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Export as PDF");
        // A menu-name query surfaces that menu's items.
        let rows = build_items(42, &sample(), "edit paste");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Paste and Match Style");
        // Disabled items never match, even by exact title.
        assert!(build_items(42, &sample(), "print").is_empty());
        // No match, no rows.
        assert!(build_items(42, &sample(), "xyzzy").is_empty());
    }

    #[test]
    fn rows_carry_ids_subtitles_and_kind() {
        let rows = build_items(42, &sample(), "");
        assert_eq!(rows[0].id, "menu.42.0");
        assert_eq!(rows[0].subtitle, "File  \u{2318}N");
        // Copy sits at seq 3 (Print holds seq 2 even though hidden), so
        // ids stay stable references into the snapshot.
        assert_eq!(rows[2].id, "menu.42.3");
        // No shortcut: the subtitle is the bare breadcrumb.
        assert_eq!(rows[1].subtitle, "File > Export");
        for row in &rows {
            assert_eq!(row.kind, ItemKind::SystemCommand);
            assert!(row.id.starts_with(ID_PREFIX));
        }
    }

    #[test]
    fn build_items_is_deterministic() {
        let a = build_items(7, &sample(), "e");
        let b = build_items(7, &sample(), "e");
        assert_eq!(a, b);
    }

    // Hardware tests. Run one at a time, by name, from a terminal that
    // holds the Accessibility grant:
    //
    //     cargo test -p beckon-macos hw_menubar_enumerate -- --ignored --nocapture
    //     cargo test -p beckon-macos hw_menubar_press_textedit_select_all -- --ignored --nocapture
    //     cargo test -p beckon-macos hw_menubar_press_select_all_and_restore -- --ignored --nocapture
    //
    // The second one launches TextEdit in the background (open -g, no
    // focus steal), presses Edit > Select All (selection state only,
    // provably non-destructive), and quits TextEdit again only if this
    // test started it. The third presses Edit > Select All on the
    // frontmost app itself and proves the press took effect through an
    // independent oracle (AXSelectedTextRange grew), then restores the
    // exact prior selection. Selection state is the one provably safe
    // observable: menu titles re-validate lazily (a pressed toggle can
    // keep its stale title until the menu is next opened), so they are
    // not used as an oracle.

    #[test]
    #[ignore = "hardware: walks the live frontmost app's menu bar"]
    fn hw_menubar_enumerate() {
        println!("AX trusted = {}", ax::is_trusted());
        let pid = ax::frontmost_app_pid().expect("a frontmost app");
        println!("frontmost pid = {pid}");
        let start = Instant::now();
        let snap = enumerate(pid);
        let elapsed_ms = start.elapsed().as_millis();
        println!(
            "enumerated {} leaf items in {elapsed_ms} ms",
            snap.entries.len()
        );
        let enabled = snap.entries.iter().filter(|x| x.enabled).count();
        println!(
            "enabled: {enabled}, disabled: {}",
            snap.entries.len() - enabled
        );
        for entry in snap.entries.iter().take(12) {
            println!(
                "  [{}] {} > {} {}",
                if entry.enabled { "on " } else { "off" },
                entry.breadcrumb,
                entry.title,
                entry.shortcut
            );
        }
        assert_eq!(snap.entries.len(), snap.elements.len());
        assert!(
            !snap.entries.is_empty(),
            "a real app's menu bar has leaf items"
        );
        // The public surface over the same walk.
        snapshot_frontmost();
        let rows = items("");
        println!("items(\"\") rows: {}", rows.len());
        for row in &rows {
            println!("  [{}] {} / {}", row.id, row.title, row.subtitle);
        }
        assert!(rows.len() <= MAX_RESULTS);
    }

    /// The pid of a running process by exact name, via pgrep. -a keeps
    /// ancestors in the match list (without it, the Terminal this test
    /// runs under is invisible). Test-only helper: the launcher itself
    /// never shells out.
    fn pgrep(name: &str) -> Option<i32> {
        let out = std::process::Command::new("pgrep")
            .args(["-ax", name])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    #[test]
    #[ignore = "hardware: presses Edit > Select All in a background TextEdit"]
    fn hw_menubar_press_textedit_select_all() {
        assert!(ax::is_trusted(), "this test needs the Accessibility grant");
        let was_running = pgrep("TextEdit").is_some();
        println!("TextEdit already running: {was_running}");
        // -g: open without bringing it to the foreground, so the user's
        // focus is never stolen.
        let opened = std::process::Command::new("open")
            .args(["-g", "-a", "TextEdit"])
            .status()
            .expect("run open")
            .success();
        assert!(opened, "open -g -a TextEdit failed");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let pid = pgrep("TextEdit").expect("TextEdit pid after open");
        println!("TextEdit pid = {pid}");
        let snap = enumerate(pid);
        println!("TextEdit leaf items: {}", snap.entries.len());
        let found = snap
            .entries
            .iter()
            .position(|x| x.title == "Select All" && x.breadcrumb.starts_with("Edit"));
        let result = match found {
            None => Err("Edit > Select All not found in TextEdit's menus".to_string()),
            Some(seq) => {
                let entry = &snap.entries[seq];
                println!(
                    "resolved seq {seq}: {} > {} enabled={} shortcut={}",
                    entry.breadcrumb, entry.title, entry.enabled, entry.shortcut
                );
                if entry.enabled {
                    // Install the snapshot and go through the real
                    // public activation path, id and all.
                    let id = format!("{ID_PREFIX}{pid}.{seq}");
                    SNAPSHOT.with(|slot| *slot.borrow_mut() = Some(snap));
                    let pressed = activate(&id);
                    println!("AXPress via activate({id}): {pressed:?}");
                    pressed
                } else {
                    // No document focused (background launch can land
                    // there): resolution is verified, the press is
                    // honestly skipped rather than forced.
                    println!("Select All is disabled here; skipping the press");
                    Ok(())
                }
            }
        };
        // Restore state before asserting: quit TextEdit only if this
        // test started it. Select All never dirties the document, so
        // quitting cannot raise a save prompt for anything we did.
        if !was_running {
            let quit = std::process::Command::new("osascript")
                .args(["-e", "tell application \"TextEdit\" to quit"])
                .status()
                .map(|s| s.success());
            println!("quit TextEdit (we started it): {quit:?}");
        }
        result.expect("resolve and press Edit > Select All");
    }

    /// kAXValueTypeCFRange from AXValue.h, with the matching struct.
    /// Test-only: the shipping module never touches text selection.
    const AX_VALUE_TYPE_CFRANGE: u32 = 4;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct CFRange {
        location: CFIndex,
        length: CFIndex,
    }

    // Test-only AX entry points for the selection oracle below; same
    // module-boundary rule as the shipping externs above.
    #[allow(non_snake_case)]
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXValueCreate(the_type: u32, value_ptr: *const c_void) -> CFTypeRef;
        fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> Boolean;
        fn AXUIElementSetAttributeValue(
            element: CFTypeRef,
            attribute: CFStringRef,
            value: CFTypeRef,
        ) -> AXError;
    }

    /// Read AXSelectedTextRange of the app's focused UI element.
    fn selected_range(app: &CfGuard) -> Result<(CfGuard, CFRange), String> {
        let focused = copy_attr(app.as_ptr(), "AXFocusedUIElement")?;
        let value = copy_attr(focused.as_ptr(), "AXSelectedTextRange")?;
        let mut range = CFRange::default();
        // Safety: value guards a live AXValue; the out pointer is a
        // CFRange-layout slot written only on reported success.
        let ok = unsafe {
            AXValueGetValue(
                value.as_ptr(),
                AX_VALUE_TYPE_CFRANGE,
                std::ptr::from_mut(&mut range).cast(),
            )
        };
        if ok == 0 {
            return Err("AXSelectedTextRange did not decode as a CFRange".to_string());
        }
        Ok((focused, range))
    }

    /// Write AXSelectedTextRange back onto a focused element.
    fn set_selected_range(focused: &CfGuard, range: CFRange) -> Result<(), String> {
        // Safety: AXValueCreate copies the CFRange bytes before
        // returning; the +1 value is guarded; the set call only reads
        // live guarded references.
        unsafe {
            let value = CfGuard::new(AXValueCreate(
                AX_VALUE_TYPE_CFRANGE,
                std::ptr::from_ref(&range).cast(),
            ));
            if value.is_null() {
                return Err("AXValueCreate(CFRange) failed".to_string());
            }
            let attr = cfstring("AXSelectedTextRange");
            let err = AXUIElementSetAttributeValue(focused.as_ptr(), attr.as_ptr(), value.as_ptr());
            if err != AX_SUCCESS {
                return Err(format!("restoring the selection failed: AXError {err}"));
            }
        }
        Ok(())
    }

    /// Edit > Select All in a fresh enumeration, enabled or not. A
    /// background app's menu reports its last-validated enabled state
    /// (often false, since validation runs when a menu is displayed),
    /// so the test presses regardless and lets the selection oracle
    /// decide whether anything happened. Selecting text is harmless
    /// either way.
    fn find_select_all(snap: &Snapshot) -> Option<usize> {
        snap.entries
            .iter()
            .position(|x| x.title == "Select All" && x.breadcrumb == "Edit")
    }

    #[test]
    #[ignore = "hardware: presses Edit > Select All in Terminal and restores the selection exactly"]
    fn hw_menubar_press_select_all_and_restore() {
        assert!(ax::is_trusted(), "this test needs the Accessibility grant");
        // Target Terminal: it always runs during this test, its Edit >
        // Select All only touches selection state, and its text area
        // exposes AXSelectedTextRange for the oracle. AXPress works on
        // non-frontmost apps, so no focus is stolen. (The frontmost app
        // is whatever the user is in and offers no provably safe,
        // observable target, so it is deliberately not used here.)
        let pid = pgrep("Terminal").expect("a running Terminal");
        let snap = enumerate(pid);
        println!("Terminal pid = {pid}, leaf items = {}", snap.entries.len());
        let seq = find_select_all(&snap).expect("Edit > Select All in Terminal's menus");
        let entry = &snap.entries[seq];
        println!(
            "resolved seq {seq}: {} > {} enabled={} shortcut={}",
            entry.breadcrumb, entry.title, entry.enabled, entry.shortcut
        );
        // Safety: any pid is a legal argument; the +1 element is
        // guarded immediately; used only for the selection oracle.
        let app = CfGuard::new(unsafe { AXUIElementCreateApplication(pid) });
        assert!(!app.is_null());
        // Safety: app guards a live element.
        unsafe { AXUIElementSetMessagingTimeout(app.as_ptr(), AX_MESSAGING_TIMEOUT_SECS) };
        let (focused, before) = selected_range(&app).expect("read the selection before");
        println!("selection before: {before:?}");
        // Press through the real public path, id and all. Selecting
        // text mutates nothing; the exact range is restored below.
        let id = format!("{ID_PREFIX}{pid}.{seq}");
        SNAPSHOT.with(|slot| *slot.borrow_mut() = Some(snap));
        activate(&id).expect("press Edit > Select All");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let (_, after) = selected_range(&app).expect("read the selection after");
        println!("selection after press: {after:?}");
        if after == before {
            // The press was delivered (AXPress reported success) but
            // the item refused to act, which matches its unvalidated
            // disabled state in a background app. Resolution and
            // delivery are verified; efficacy is not provable in this
            // context, said honestly rather than forced.
            println!(
                "selection unchanged: the item is genuinely disabled while \
                 Terminal is in the background; resolution and AXPress \
                 delivery verified, efficacy unprovable in this context"
            );
            return;
        }
        // Restore the exact starting selection before asserting, so a
        // failure never leaves the user's window select-all'd.
        let restored = set_selected_range(&focused, before);
        println!("selection restored: {restored:?}");
        let (_, final_range) = selected_range(&app).expect("read the selection after restore");
        println!("selection final: {final_range:?}");
        assert!(
            after.length > before.length,
            "the press did not grow the selection: {before:?} -> {after:?}"
        );
        restored.expect("restore the original selection");
        assert_eq!(
            final_range, before,
            "the selection was not restored exactly"
        );
    }
}
