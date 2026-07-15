//! The window switcher: every on-screen window of every app, fuzzy
//! searchable, one Return away.
//!
//! Enumeration reads the CoreGraphics window list
//! (CGWindowListCopyWindowInfo over kCGNullWindowID, on-screen windows
//! only, desktop elements excluded) and keeps layer 0, which is where
//! normal document and app windows live; menu bars, the Dock, overlays,
//! and status items sit on other layers. Filtering policy, in order:
//!
//! 1. Windows owned by beckon itself are dropped (the switcher must not
//!    offer its own panel).
//! 2. Windows with an empty owner name are dropped (nothing usable to
//!    show or match against).
//! 3. Untitled windows of an app that also has titled windows are
//!    dropped. Apps park invisible helper windows at layer 0 (Finder
//!    and Chrome both do); when real titles exist, the untitled rows are
//!    noise. An app with only untitled windows keeps them all, titled
//!    with the app name, so the no-titles world below still lists every
//!    app.
//!
//! Two permission worlds, handled without special casing at call sites:
//!
//! - Screen Recording: on modern macOS kCGWindowName is null for other
//!   apps' windows unless the user granted Screen Recording. With the
//!   grant, rows read "window title / app name"; without it, rows
//!   degrade to "app name / app name" (rule 3 keeps them all). The
//!   hardware test reports which world the machine is in.
//! - Accessibility: focusing a specific window needs the AX grant.
//!   Without it, [`activate`] falls back to activating the owning app,
//!   which raises that app's windows without selecting a specific one.
//!
//! Focusing strategy, honestly stated: the stable id carries the CG
//! window number, but the AX API has no public bridge from a CG window
//! number to an AXUIElement. The private symbol _AXUIElementGetWindow
//! is that bridge and is what every serious switcher uses. It is looked
//! up with dlsym at runtime, never linked: when present (it has been,
//! for many macOS releases) matching is exact; when absent the fallback
//! matches the first AX window whose AXTitle equals the CG title, which
//! can pick the wrong twin when two windows share a title; when nothing
//! matches, activation degrades to app-level. Bringing the app forward
//! goes through the assistive path (AXMain on the window, AXRaise,
//! AXFrontmost on the app element) rather than relying on the
//! cooperative one (NSRunningApplication activateWithOptions:),
//! because since macOS 14 cooperative activation lets the system deny
//! focus to a process that is not itself frontmost; the assistive
//! sequence is permission-gated instead and is hardware-verified on
//! macOS 26 (the round-trip test flips focus to another app's window
//! and back). Every AX call is bounded by a messaging timeout so an
//! unresponsive app cannot hang the launcher.
//!
//! Contract for the engine: [`items`] returns Window-kind rows in a
//! deterministic order (frontmost app's windows first, then app name,
//! pid, window number; the empty query returns at most
//! [`MAX_EMPTY_QUERY_ITEMS`], a non-empty query returns every
//! case-insensitive substring match on title or app name), and
//! [`activate`] focuses one row by id ("winsw.<pid>.<windowNumber>").

use crate::ax::{self, AXError, Boolean, CFIndex, CFStringRef, CFTypeRef, CfGuard};
use crate::ffi::{self, msg, Bool, Id};
use beckon_core::router::{Item, ItemKind};
use std::ffi::{c_char, c_void};

/// Cap on the rows the empty query returns. Non-empty queries return
/// every match; the engine applies its own display cap.
const MAX_EMPTY_QUERY_ITEMS: usize = 9;

/// Id prefix; the full scheme is "winsw.<pid>.<windowNumber>".
const ID_PREFIX: &str = "winsw.";

/// CGWindowListOption bits from CGWindow.h.
const CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
const CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

/// kCGNullWindowID.
const CG_NULL_WINDOW_ID: u32 = 0;

/// kCFNumberSInt64Type from CFNumber.h.
const CF_NUMBER_SINT64_TYPE: CFIndex = 4;

/// kCFStringEncodingUTF8 from CFString.h.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// kAXErrorSuccess.
const AX_SUCCESS: AXError = 0;

/// NSApplicationActivationOptions bits: activate the app bringing all
/// of its windows forward, and take focus even though beckon itself is
/// an accessory app that was not frontmost.
const NS_ACTIVATE_ALL_WINDOWS: usize = 1 << 0;
const NS_ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;

/// Seconds an AX call may block on an unresponsive app before erroring.
/// A float, but only as the literal argument the AX FFI edge demands
/// (AXUIElementSetMessagingTimeout takes a C float); no float arithmetic
/// happens anywhere in this module.
const AX_MESSAGING_TIMEOUT_SECS: f32 = 1.0;

/// dlsym's pseudo-handle for "search every image", from dlfcn.h.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// The signature of the private _AXUIElementGetWindow: element in, CG
/// window number out, AXError back.
type AxGetWindowFn = unsafe extern "C" fn(CFTypeRef, *mut u32) -> AXError;

#[allow(non_snake_case, non_upper_case_globals)]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// CFString keys of the window-info dictionaries, from CGWindow.h.
    /// Framework-lifetime constants: borrowed, never released.
    static kCGWindowOwnerName: CFStringRef;
    static kCGWindowName: CFStringRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowNumber: CFStringRef;
    static kCGWindowLayer: CFStringRef;

    /// Returns a +1 CFArray of CFDictionary describing the windows, or
    /// null when the list cannot be produced.
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
}

// CoreFoundation accessors ax.rs does not need; declared here per the
// module-boundary rule (reuse ax.rs as-is, declare missing externs
// locally). Get functions return borrowed references, Copy/Create
// return +1 (guarded at every call site).
#[allow(non_snake_case)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: CFIndex) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
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

// AX entry points ax.rs declares privately or not at all; same
// boundary rule as above.
#[allow(non_snake_case)]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout_seconds: f32) -> AXError;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    /// Framework-lifetime constant: borrowed, never released.
    static kCFBooleanTrue: CFTypeRef;
}

extern "C" {
    /// Runtime symbol lookup from libSystem, used for the private
    /// _AXUIElementGetWindow so a future macOS that drops the symbol
    /// degrades instead of failing to load.
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// One layer-0 window as read from the CG window list. `title` is empty
/// when kCGWindowName was absent (no Screen Recording grant) or blank.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WinInfo {
    pid: i32,
    number: u32,
    owner: String,
    title: String,
}

/// The switcher rows for `query`, shaped for the registry. Enumerates
/// live, so every call reflects the current desktop; ordering and
/// filtering are the pure pipeline documented on [`build_items`].
pub fn items(query: &str) -> Vec<Item> {
    let own_pid = std::process::id() as i32;
    build_items(list_windows(), own_pid, ax::frontmost_app_pid(), query)
}

/// Focus one window by its "winsw.<pid>.<windowNumber>" id.
///
/// With the Accessibility grant: resolve the pid's AX window list,
/// match the window (exactly via _AXUIElementGetWindow when the symbol
/// resolves, else first-title-match against the current CG title), make
/// it the app's main window, raise it, and bring the app frontmost via
/// AXFrontmost. Without the grant, or when no AX window matches:
/// activate the owning app as a whole, the best macOS allows (and the
/// untrusted cooperative request may still be denied when beckon is not
/// frontmost). Errs, never panics, and every AX call is
/// timeout-bounded, so it never hangs.
pub fn activate(id: &str) -> Result<(), String> {
    let (pid, number) = parse_id(id)?;
    if !ax::is_trusted() {
        return activate_app(pid, true).map_err(|e| {
            format!(
                "cannot focus the specific window: macOS has not granted beckon the \
                 Accessibility permission, and the app-level fallback also failed: {e}"
            )
        });
    }
    // The current CG title of the target, for the no-private-symbol
    // fallback match. A stale id (window closed since enumeration)
    // yields None and degrades to app activation below.
    let cg_title = list_windows()
        .into_iter()
        .find(|w| w.pid == pid && w.number == number)
        .map(|w| w.title);
    match focus_ax_window(pid, number, cg_title.as_deref()) {
        Ok(()) => Ok(()),
        // No matching AX window (stale id, or a title-only world with
        // no title match): bring the whole app frontmost instead, via
        // AX first (trust is already established here) and the
        // cooperative path second.
        Err(FocusMiss::NotFound) => frontmost_via_ax(pid).or_else(|_| activate_app(pid, true)),
        Err(FocusMiss::Ax(e)) => Err(e),
    }
}

/// Make the app owning `pid` frontmost through the assistive path:
/// AXFrontmost on the application element. Unlike NSRunningApplication
/// activation this is not subject to cooperative-activation denial; it
/// is gated by the Accessibility grant the caller already checked.
fn frontmost_via_ax(pid: i32) -> Result<(), String> {
    // Safety: any pid is a legal argument; the +1 element is guarded
    // immediately; the timeout bounds the write below.
    let app = CfGuard::new(unsafe { AXUIElementCreateApplication(pid) });
    if app.is_null() {
        return Err("AXUIElementCreateApplication returned null".to_string());
    }
    // Safety: app guards a live element.
    unsafe { AXUIElementSetMessagingTimeout(app.as_ptr(), AX_MESSAGING_TIMEOUT_SECS) };
    set_true_attr(app.as_ptr(), "AXFrontmost")
}

/// Why the AX focus path did not focus a window: the window was not
/// found (degrade to app activation) versus a real AX failure (report).
enum FocusMiss {
    NotFound,
    Ax(String),
}

/// Find the AX window of `pid` whose CG window number is `number` and
/// focus it. `cg_title` feeds the fallback matcher when the private
/// bridge symbol is unavailable.
fn focus_ax_window(pid: i32, number: u32, cg_title: Option<&str>) -> Result<(), FocusMiss> {
    // Safety: any pid is a legal argument; the +1 element is guarded
    // immediately (ax module invariant 1 applies to this module too).
    let app = CfGuard::new(unsafe { AXUIElementCreateApplication(pid) });
    if app.is_null() {
        return Err(FocusMiss::Ax(
            "AXUIElementCreateApplication returned null".to_string(),
        ));
    }
    // Safety: app guards a live element; the timeout bounds every later
    // call on it so an unresponsive target app cannot hang beckon.
    unsafe { AXUIElementSetMessagingTimeout(app.as_ptr(), AX_MESSAGING_TIMEOUT_SECS) };
    let windows = copy_attr(app.as_ptr(), "AXWindows").map_err(FocusMiss::Ax)?;
    // Safety: windows guards a live CFArray for the whole loop; values
    // at index are borrowed AXUIElements kept alive by that array.
    let count = unsafe { CFArrayGetCount(windows.as_ptr()) };
    let bridge = ax_get_window_fn();
    for i in 0..count {
        // Safety: i is in 0..count of the guarded array; the borrowed
        // element is only used while the guard lives.
        let win = unsafe { CFArrayGetValueAtIndex(windows.as_ptr(), i) };
        if win.is_null() {
            continue;
        }
        let matched = match bridge {
            Some(get_window) => {
                let mut num: u32 = 0;
                // Safety: the private bridge has carried this exact
                // signature (element, out CGWindowID) across releases;
                // it is only called when dlsym resolved it, on a live
                // borrowed element, with a plain out-pointer.
                unsafe { get_window(win, &mut num) == AX_SUCCESS && num == number }
            }
            // Honest heuristic: without the bridge, the first window
            // whose AXTitle equals the CG title wins; twins with the
            // same title are indistinguishable here (module docs).
            None => match cg_title {
                Some(t) if !t.is_empty() => {
                    let title = copy_attr(win, "AXTitle")
                        .ok()
                        .and_then(|v| {
                            // Safety: v guards a live value; cf_string
                            // type-checks before reading.
                            unsafe { cf_string(v.as_ptr()) }
                        })
                        .unwrap_or_default();
                    title == t
                }
                _ => false,
            },
        };
        if matched {
            // The assistive activation sequence: make the window the
            // app's main window (best effort; some windows refuse),
            // raise it, then bring the app frontmost via AXFrontmost.
            // NSRunningApplication rides along as a cooperative nudge,
            // best effort: on modern macOS it is denied unless beckon
            // was itself frontmost, which it is in real use.
            let _ = set_true_attr(win, "AXMain");
            raise(win)?;
            set_true_attr(app.as_ptr(), "AXFrontmost").map_err(FocusMiss::Ax)?;
            let _ = activate_app(pid, false);
            return Ok(());
        }
    }
    Err(FocusMiss::NotFound)
}

/// Set a boolean AX attribute to true on a live (borrowed or guarded)
/// AX element.
fn set_true_attr(element: CFTypeRef, name: &str) -> Result<(), String> {
    let attr = cfstring(name);
    if attr.is_null() {
        return Err("CFString allocation failed".to_string());
    }
    // Safety: element is live for the whole call (caller's guard), attr
    // is guarded, and kCFBooleanTrue is a framework-lifetime constant
    // that is borrowed, never released.
    let err = unsafe { AXUIElementSetAttributeValue(element, attr.as_ptr(), kCFBooleanTrue) };
    if err != AX_SUCCESS {
        return Err(format!("writing {name} failed: AXError {err}"));
    }
    Ok(())
}

/// Perform AXRaise on a borrowed AX window.
fn raise(win: CFTypeRef) -> Result<(), FocusMiss> {
    let action = cfstring("AXRaise");
    if action.is_null() {
        return Err(FocusMiss::Ax("CFString allocation failed".to_string()));
    }
    // Safety: win is a live borrowed element (its owning CFArray guard
    // is alive in the caller); action is guarded for the whole call.
    let err = unsafe { AXUIElementPerformAction(win, action.as_ptr()) };
    if err != AX_SUCCESS {
        return Err(FocusMiss::Ax(format!("AXRaise failed: AXError {err}")));
    }
    Ok(())
}

/// Activate the app owning `pid` via NSRunningApplication. With
/// `all_windows` every window comes forward (the no-AX fallback);
/// without it only the app activates, preserving the AXRaise ordering
/// the caller just established.
fn activate_app(pid: i32, all_windows: bool) -> Result<(), String> {
    let _pool = ffi::AutoreleasePool::new();
    let opts = if all_windows {
        NS_ACTIVATE_ALL_WINDOWS | NS_ACTIVATE_IGNORING_OTHER_APPS
    } else {
        NS_ACTIVATE_IGNORING_OTHER_APPS
    };
    // Safety: main-thread caller (engine activation path);
    // runningApplicationWithProcessIdentifier: returns an autoreleased
    // instance or nil, kept alive by the pool; activateWithOptions:
    // takes NSUInteger and returns BOOL. Signatures spelled exactly
    // (ffi module invariant 1).
    unsafe {
        let app = msg!(Id: ffi::class("NSRunningApplication"),
            ffi::sel("runningApplicationWithProcessIdentifier:"), i32: pid);
        if app.is_null() {
            return Err(format!("no running application with pid {pid}"));
        }
        let ok = msg!(Bool: app, ffi::sel("activateWithOptions:"), usize: opts);
        if ok == 0 {
            return Err(format!("macOS declined to activate the app with pid {pid}"));
        }
    }
    Ok(())
}

/// Resolve _AXUIElementGetWindow at runtime. None when the symbol is
/// gone, which flips the matcher to the title heuristic (module docs).
fn ax_get_window_fn() -> Option<AxGetWindowFn> {
    static NAME: &[u8] = b"_AXUIElementGetWindow\0";
    // Safety: dlsym with RTLD_DEFAULT and a NUL-terminated name is
    // always safe to call; the transmute to a fn pointer is sound
    // because the symbol, when present, is exactly a function of the
    // AxGetWindowFn signature (module docs, focusing strategy).
    unsafe {
        let sym = dlsym(RTLD_DEFAULT, NAME.as_ptr().cast());
        if sym.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, AxGetWindowFn>(sym))
        }
    }
}

/// Snapshot every on-screen layer-0 window (module docs, enumeration).
/// An unavailable window list yields an empty vec, never an error: the
/// switcher shows nothing rather than breaking the panel.
fn list_windows() -> Vec<WinInfo> {
    // Safety: the +1 array is guarded immediately; every value fetched
    // from it is borrowed and used only while the guard lives; the
    // dictionary key statics are framework-lifetime constants. Values
    // are type-checked (cf_string, cf_i64) before being read, so a
    // malformed dictionary entry is skipped, not crashed on.
    unsafe {
        let arr = CfGuard::new(CGWindowListCopyWindowInfo(
            CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
            CG_NULL_WINDOW_ID,
        ));
        if arr.is_null() {
            return Vec::new();
        }
        let count = CFArrayGetCount(arr.as_ptr());
        let mut out = Vec::new();
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(arr.as_ptr(), i);
            if dict.is_null() {
                continue;
            }
            if cf_i64(CFDictionaryGetValue(dict, kCGWindowLayer)) != Some(0) {
                continue;
            }
            let Some(pid) = cf_i64(CFDictionaryGetValue(dict, kCGWindowOwnerPID)) else {
                continue;
            };
            let Some(number) = cf_i64(CFDictionaryGetValue(dict, kCGWindowNumber)) else {
                continue;
            };
            let owner = cf_string(CFDictionaryGetValue(dict, kCGWindowOwnerName));
            let title = cf_string(CFDictionaryGetValue(dict, kCGWindowName));
            out.push(WinInfo {
                pid: pid as i32,
                number: number as u32,
                owner: owner.unwrap_or_default(),
                title: title.unwrap_or_default(),
            });
        }
        out
    }
}

/// The pure pipeline from raw windows to registry rows: filter (module
/// docs, rules 1 to 3), order (frontmost app's windows first, then app
/// name, pid, window number; fully deterministic), shape (title falls
/// back to the app name, subtitle is the app name, id is
/// "winsw.<pid>.<windowNumber>"), then query: empty returns the first
/// [`MAX_EMPTY_QUERY_ITEMS`], non-empty keeps case-insensitive
/// substring matches on title or app name.
fn build_items(wins: Vec<WinInfo>, own_pid: i32, frontmost: Option<i32>, query: &str) -> Vec<Item> {
    let mut kept = sanitize(wins, own_pid);
    kept.sort_by(|a, b| {
        let a_front = Some(a.pid) != frontmost;
        let b_front = Some(b.pid) != frontmost;
        a_front
            .cmp(&b_front)
            .then_with(|| a.owner.cmp(&b.owner))
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.number.cmp(&b.number))
    });
    let all: Vec<Item> = kept
        .iter()
        .map(|w| {
            let title = if w.title.is_empty() {
                w.owner.as_str()
            } else {
                w.title.as_str()
            };
            Item::new(
                &format!("{ID_PREFIX}{}.{}", w.pid, w.number),
                title,
                &w.owner,
                ItemKind::Window,
            )
        })
        .collect();
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all.into_iter().take(MAX_EMPTY_QUERY_ITEMS).collect();
    }
    all.into_iter()
        .filter(|item| {
            item.title.to_lowercase().contains(&q) || item.subtitle.to_lowercase().contains(&q)
        })
        .collect()
}

/// Filtering rules 1 to 3 from the module docs, as a pure function.
fn sanitize(wins: Vec<WinInfo>, own_pid: i32) -> Vec<WinInfo> {
    let titled_pids: Vec<i32> = wins
        .iter()
        .filter(|w| !w.title.is_empty())
        .map(|w| w.pid)
        .collect();
    wins.into_iter()
        .filter(|w| {
            w.pid != own_pid
                && !w.owner.is_empty()
                && (!w.title.is_empty() || !titled_pids.contains(&w.pid))
        })
        .collect()
}

/// Split a "winsw.<pid>.<windowNumber>" id back into its parts.
fn parse_id(id: &str) -> Result<(i32, u32), String> {
    let bad = || format!("not a window switcher id: {id}");
    let rest = id.strip_prefix(ID_PREFIX).ok_or_else(bad)?;
    let (pid_s, num_s) = rest.split_once('.').ok_or_else(bad)?;
    let pid: i32 = pid_s.parse().map_err(|_| bad())?;
    let number: u32 = num_s.parse().map_err(|_| bad())?;
    Ok((pid, number))
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

    fn w(pid: i32, number: u32, owner: &str, title: &str) -> WinInfo {
        WinInfo {
            pid,
            number,
            owner: owner.to_string(),
            title: title.to_string(),
        }
    }

    fn desktop() -> Vec<WinInfo> {
        vec![
            w(300, 31, "Terminal", "zsh: ~/geist"),
            w(100, 12, "Safari", "Rust std docs"),
            w(100, 11, "Safari", ""),
            w(100, 10, "Safari", "GitHub"),
            w(200, 20, "Preview", ""),
            w(999, 90, "beckon", "beckon"),
            w(400, 40, "", "orphan"),
        ]
    }

    #[test]
    fn id_round_trips() {
        assert_eq!(parse_id("winsw.1234.567").unwrap(), (1234, 567));
        assert_eq!(parse_id("winsw.-2.9").unwrap(), (-2, 9));
    }

    #[test]
    fn bad_ids_are_rejected_not_panicked_on() {
        for bad in [
            "",
            "winsw.",
            "winsw.12",
            "winsw.12.",
            "winsw..34",
            "winsw.a.b",
            "winsw.12.-3",
            "window.left-half",
            "app.safari",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn sanitize_applies_the_three_documented_rules() {
        let kept = sanitize(desktop(), 999);
        // beckon itself gone (rule 1), empty owner gone (rule 2),
        // Safari's untitled helper gone because Safari has titled
        // windows (rule 3), Preview's untitled-only window kept.
        let ids: Vec<(i32, u32)> = kept.iter().map(|x| (x.pid, x.number)).collect();
        assert_eq!(ids, vec![(300, 31), (100, 12), (100, 10), (200, 20)]);
    }

    #[test]
    fn untitled_only_apps_survive_the_no_titles_world() {
        // No Screen Recording grant: every title is empty; nothing may
        // be dropped by rule 3 or the list would be empty.
        let wins = vec![w(1, 1, "Safari", ""), w(2, 2, "Terminal", "")];
        assert_eq!(sanitize(wins, 999).len(), 2);
    }

    #[test]
    fn ordering_is_frontmost_first_then_owner_pid_number() {
        let items = build_items(desktop(), 999, Some(200), "");
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        // Preview (frontmost) first, then Safari by owner name with
        // windows by number, then Terminal.
        assert_eq!(
            ids,
            vec![
                "winsw.200.20",
                "winsw.100.10",
                "winsw.100.12",
                "winsw.300.31"
            ]
        );
    }

    #[test]
    fn shaping_titles_subtitles_and_kind() {
        let items = build_items(desktop(), 999, Some(200), "");
        // Untitled Preview window falls back to the app name.
        assert_eq!(items[0].title, "Preview");
        assert_eq!(items[0].subtitle, "Preview");
        // Titled windows show the window title over the app name.
        assert_eq!(items[1].title, "GitHub");
        assert_eq!(items[1].subtitle, "Safari");
        for item in &items {
            assert_eq!(item.kind, ItemKind::Window);
            assert!(item.id.starts_with(ID_PREFIX));
        }
    }

    #[test]
    fn query_filters_case_insensitively_on_title_or_app_name() {
        let by_title = build_items(desktop(), 999, None, "github");
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].title, "GitHub");
        // App name matches too, catching both of Safari's kept rows.
        let by_app = build_items(desktop(), 999, None, "SAFARI");
        assert_eq!(by_app.len(), 2);
        // No match, no rows.
        assert!(build_items(desktop(), 999, None, "xyzzy").is_empty());
    }

    #[test]
    fn empty_query_caps_at_nine_and_is_deterministic() {
        let mut wins = Vec::new();
        for n in 0..25u32 {
            wins.push(w(50 + n as i32, n, &format!("App{n:02}"), "doc"));
        }
        let a = build_items(wins.clone(), 999, None, "");
        let b = build_items(wins, 999, None, "");
        assert_eq!(a.len(), MAX_EMPTY_QUERY_ITEMS);
        assert_eq!(a, b);
        assert_eq!(a[0].id, "winsw.50.0");
    }

    #[test]
    fn activate_rejects_garbage_ids_before_touching_anything() {
        let err = activate("winsw.garbage").unwrap_err();
        assert!(err.contains("not a window switcher id"), "{err}");
    }

    // Hardware tests. Run one at a time, by name, from a terminal:
    //
    //     cargo test -p beckon-macos hw_switcher_enumerate -- --ignored --nocapture
    //     cargo test -p beckon-macos hw_switcher_activate_and_restore -- --ignored --nocapture
    //
    // The second one flips focus to another app's window and back; it
    // needs the terminal to hold the Accessibility grant for exact
    // window matching (it degrades to app activation without it).

    #[test]
    #[ignore = "hardware: reads the live CG window list"]
    fn hw_switcher_enumerate() {
        let own = std::process::id() as i32;
        let wins = list_windows();
        println!("raw layer-0 windows: {}", wins.len());
        let titled_other = wins
            .iter()
            .filter(|x| x.pid != own && !x.title.is_empty())
            .count();
        println!(
            "screen recording world: {}",
            if titled_other > 0 {
                "titles available (Screen Recording granted)"
            } else {
                "titles unavailable (app names only)"
            }
        );
        for x in wins.iter().take(8) {
            println!(
                "  pid={} num={} owner={:?} title={:?}",
                x.pid, x.number, x.owner, x.title
            );
        }
        assert!(
            !wins.is_empty(),
            "a live desktop has at least one layer-0 window"
        );
        let rows = items("");
        println!("items(\"\") rows: {}", rows.len());
        for r in &rows {
            println!("  [{}] {} / {}", r.id, r.title, r.subtitle);
        }
        assert!(!rows.is_empty());
        assert!(rows.len() <= MAX_EMPTY_QUERY_ITEMS);
        println!(
            "_AXUIElementGetWindow resolves: {}",
            ax_get_window_fn().is_some()
        );
    }

    /// The focus oracle for hardware tests. NSWorkspace's frontmost
    /// application is refreshed by notifications on the main run loop,
    /// which a test process never pumps, so ax::frontmost_app_pid
    /// returns a stale launch-time snapshot here (observed live: the
    /// flip happened on screen while NSWorkspace kept reporting the old
    /// pid). The CG window list has no run loop dependency and is
    /// ordered front to back, so the first layer-0 window's owner is
    /// the frontmost app, fresh on every call. Inside the real app the
    /// run loop runs and ax::frontmost_app_pid is fine.
    fn cg_frontmost_pid() -> Option<i32> {
        list_windows().first().map(|w| w.pid)
    }

    #[test]
    #[ignore = "hardware: flips focus to another app's window, then restores the frontmost app"]
    fn hw_switcher_activate_and_restore() {
        let prev = cg_frontmost_pid().expect("a frontmost layer-0 window");
        let own = std::process::id() as i32;
        println!("AX trusted = {}", ax::is_trusted());
        println!("previous frontmost pid = {prev}");
        let wins = sanitize(list_windows(), own);
        let target = wins.iter().find(|x| x.pid != prev).cloned();
        let Some(target) = target else {
            println!("no window of another app on screen; nothing to flip to");
            return;
        };
        let id = format!("{ID_PREFIX}{}.{}", target.pid, target.number);
        println!(
            "activating [{}] owner={:?} title={:?}",
            id, target.owner, target.title
        );
        activate(&id).expect("activate the target window");
        std::thread::sleep(std::time::Duration::from_millis(600));
        let mid = cg_frontmost_pid();
        println!(
            "frontmost after activate = {mid:?} (want Some({}))",
            target.pid
        );
        // Restore the previous frontmost app before asserting, so a
        // failure does not leave focus stolen. Same assistive path the
        // switcher itself uses, because the cooperative one is denied
        // to a non-frontmost process (module docs).
        frontmost_via_ax(prev)
            .or_else(|_| activate_app(prev, true))
            .expect("re-activate the previous frontmost app");
        std::thread::sleep(std::time::Duration::from_millis(600));
        let after = cg_frontmost_pid();
        println!("frontmost after restore = {after:?} (want Some({prev}))");
        assert_eq!(
            mid,
            Some(target.pid),
            "activation did not bring the target app frontmost"
        );
        assert_eq!(
            after,
            Some(prev),
            "the previous frontmost app was not restored"
        );
    }
}
