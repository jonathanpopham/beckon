//! Accessibility (AX) and CoreFoundation FFI: the plain-C side of the
//! shell. Everything Objective-C goes through ffi.rs; this module links
//! the C entry points of ApplicationServices and CoreFoundation directly
//! (no wrapper crates) and wraps them in small safe helpers so callers
//! like winmgmt.rs never touch a raw CF pointer.
//!
//! Safety invariants for this module, referenced by the unsafe blocks:
//!
//! 1. Ownership: every CF object a Create or Copy function returns
//!    arrives at +1 and goes into a [`CfGuard`] immediately, so the
//!    matching CFRelease is structural (Drop), never a convention.
//!    Framework constants (kCFBooleanTrue, the dictionary callback
//!    tables) are borrowed and never released.
//! 2. Signatures: every extern declaration below spells the real C
//!    prototype from the macOS SDK headers. NSPoint and NSSize from
//!    ffi.rs are layout-identical to CGPoint and CGSize (two f64 each),
//!    so they cross the AXValue boundary directly.
//! 3. Threading: unlike AppKit, the AX client API talks to the target
//!    app over IPC and may be called from any thread; it can also block
//!    while that app is unresponsive. Callers stay simple and
//!    synchronous and check trust first, because an untrusted process
//!    gets fast errors instead of hangs.

// Wired into the engine by the integrator; until that lands nothing in
// main calls this module, so the dead-code lint is silenced file-wide.
// Remove the allow with the first caller.
#![allow(dead_code)]

use crate::ffi::{self, msg, Id, NSPoint, NSSize};
use std::ffi::c_void;

/// Any CoreFoundation object. CF is C, so these are plain pointers, not
/// Objective-C ids (though toll-free bridging makes many interchangeable).
pub type CFTypeRef = *const c_void;
pub type CFStringRef = CFTypeRef;
pub type CFDictionaryRef = CFTypeRef;
pub type AXUIElementRef = CFTypeRef;
pub type AXValueRef = CFTypeRef;

/// CFIndex is a signed long: 64 bits on every Apple target we build for.
pub type CFIndex = isize;

/// Darwin Boolean is an unsigned char, distinct from Objective-C BOOL.
pub type Boolean = u8;

/// AXError from AXError.h. Zero is success; failures are negative.
pub type AXError = i32;

/// AXValueType from AXValue.h.
pub type AXValueType = u32;

/// kAXValueTypeCGPoint and kAXValueTypeCGSize from AXValue.h.
const AX_VALUE_TYPE_CGPOINT: AXValueType = 1;
const AX_VALUE_TYPE_CGSIZE: AXValueType = 2;

/// kAXErrorSuccess.
const AX_SUCCESS: AXError = 0;

/// kCFStringEncodingUTF8 from CFString.h.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// CFDictionary callback tables from CFDictionary.h. Only their
/// addresses are taken, but the statics are declared with their true
/// layouts so the types are honest.
#[repr(C)]
pub struct CFDictionaryKeyCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
pub struct CFDictionaryValueCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[allow(non_snake_case, non_upper_case_globals)]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// CFString key for the options dictionary of
    /// AXIsProcessTrustedWithOptions; kCFBooleanTrue asks for the prompt.
    static kAXTrustedCheckOptionPrompt: CFStringRef;

    fn AXIsProcessTrusted() -> Boolean;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_seconds: f32) -> AXError;
    fn AXValueCreate(the_type: AXValueType, value_ptr: *const c_void) -> AXValueRef;
    fn AXValueGetValue(value: AXValueRef, the_type: AXValueType, value_ptr: *mut c_void)
        -> Boolean;
}

#[allow(non_snake_case, non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;

    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: Boolean,
    ) -> CFStringRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const CFDictionaryKeyCallBacks,
        value_callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFDictionaryRef;
}

/// Owns one CoreFoundation reference (+1) and releases it exactly once,
/// on Drop (module invariant 1). Null is representable so fallible
/// Create calls can be checked after wrapping; Drop skips it.
pub struct CfGuard(CFTypeRef);

impl CfGuard {
    /// Take ownership of a +1 reference (or null from a failed Create).
    pub fn new(ptr: CFTypeRef) -> CfGuard {
        CfGuard(ptr)
    }

    /// Borrow the reference for an FFI call. The guard must outlive the
    /// call, which scoping guarantees at every call site here.
    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

impl Drop for CfGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: self.0 is the +1 reference taken at construction,
            // released exactly once, here (module invariant 1).
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Build an owned CFString from a Rust string.
fn cfstring(s: &str) -> CfGuard {
    // Safety: the byte pointer and length describe the live &str for the
    // duration of the call and CFString copies the bytes; the +1 result
    // goes straight into the guard. UTF-8 in, UTF-8 declared, so the
    // call cannot fail for any &str, but null is still handled by the
    // callers through CfGuard::is_null.
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

/// Human-readable rendering of the AX error codes worth explaining; the
/// raw code rides along either way.
fn ax_err(code: AXError) -> String {
    let name = match code {
        -25200 => "kAXErrorFailure",
        -25201 => "kAXErrorIllegalArgument",
        -25202 => "kAXErrorInvalidUIElement",
        -25204 => "kAXErrorCannotComplete (is the target app responding?)",
        -25205 => "kAXErrorAttributeUnsupported",
        -25211 => "kAXErrorAPIDisabled (Accessibility permission missing)",
        -25212 => "kAXErrorNoValue",
        _ => return format!("AXError {code}"),
    };
    format!("{name} [{code}]")
}

/// Copy one attribute of an AX element. The result is owned (+1).
fn copy_attr(element: &CfGuard, name: &str) -> Result<CfGuard, String> {
    let attr = cfstring(name);
    let mut out: CFTypeRef = std::ptr::null();
    // Safety: element and attr are live guarded references for the whole
    // call; out is a plain out-pointer written by the framework. On
    // success the +1 result moves into a guard (module invariant 1).
    let err = unsafe { AXUIElementCopyAttributeValue(element.as_ptr(), attr.as_ptr(), &mut out) };
    if err != AX_SUCCESS {
        return Err(format!("reading {name} failed: {}", ax_err(err)));
    }
    if out.is_null() {
        return Err(format!("reading {name} returned no value"));
    }
    Ok(CfGuard::new(out))
}

/// Set one attribute of an AX element.
fn set_attr(element: &CfGuard, name: &str, value: &CfGuard) -> Result<(), String> {
    let attr = cfstring(name);
    // Safety: element, attr, and value are live guarded references for
    // the whole call; the framework retains what it keeps.
    let err =
        unsafe { AXUIElementSetAttributeValue(element.as_ptr(), attr.as_ptr(), value.as_ptr()) };
    if err != AX_SUCCESS {
        return Err(format!("writing {name} failed: {}", ax_err(err)));
    }
    Ok(())
}

/// Bound how long any AX call to this element may block on the target app.
/// Every read and write below is synchronous IPC into another process; on a
/// busy or unresponsive app the default timeout is long enough to look like
/// beckon hung. Best effort: if the framework refuses the hint the (longer)
/// default still applies.
fn set_messaging_timeout(element: &CfGuard, seconds: f32) {
    // Safety: element is a live guarded AXUIElement; the call only records
    // the timeout on it. The AXError result is advisory here.
    unsafe {
        let _ = AXUIElementSetMessagingTimeout(element.as_ptr(), seconds);
    }
}

/// True when macOS has granted this process (or its responsible process,
/// for an unbundled binary run from a terminal) the Accessibility
/// permission.
pub fn is_trusted() -> bool {
    // Safety: AXIsProcessTrusted takes nothing and returns a Boolean.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Ask macOS to show the Accessibility grant dialog for this process.
/// The system shows it at most once per grant state; calling again while
/// a decision is pending is a no-op. Returns the current trust state.
pub fn prompt_for_trust() -> bool {
    // Safety: kAXTrustedCheckOptionPrompt and kCFBooleanTrue are
    // framework-lifetime constants (borrowed, never released, module
    // invariant 1); CFDictionaryCreate copies the one-entry key and
    // value arrays before returning and its +1 result is guarded.
    // AXIsProcessTrustedWithOptions only reads the dictionary.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let dict = CfGuard::new(CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ));
        if dict.is_null() {
            // No dictionary means no prompt; fall back to the bare check.
            return AXIsProcessTrusted() != 0;
        }
        AXIsProcessTrustedWithOptions(dict.as_ptr()) != 0
    }
}

/// The pid of the frontmost application via NSWorkspace. None when
/// nothing is frontmost (login and fast-user-switch edge states).
pub fn frontmost_app_pid() -> Option<i32> {
    let _pool = ffi::AutoreleasePool::new();
    // Safety: sharedWorkspace and frontmostApplication return
    // autoreleased objects kept alive by the pool for this scope;
    // processIdentifier returns pid_t (i32). All three signatures are
    // spelled exactly (ffi module invariant 1).
    unsafe {
        let ws = msg!(Id: ffi::class("NSWorkspace"), ffi::sel("sharedWorkspace"));
        if ws.is_null() {
            return None;
        }
        let app = msg!(Id: ws, ffi::sel("frontmostApplication"));
        if app.is_null() {
            return None;
        }
        Some(msg!(i32: app, ffi::sel("processIdentifier")))
    }
}

/// The focused window of the frontmost application, held as a guarded
/// AXUIElement. Position is in AX coordinates: origin at the top-left
/// corner of the primary display, y growing downward, CGFloat points.
pub struct FocusedWindow {
    window: CfGuard,
}

/// Resolve the frontmost app (NSWorkspace pid, falling back to the
/// system-wide element's AXFocusedApplication) and copy its
/// AXFocusedWindow. Fails with a clear message instead of panicking when
/// there is no window to manage.
pub fn focused_window() -> Result<FocusedWindow, String> {
    let app = match frontmost_app_pid() {
        Some(pid) => {
            // Safety: any pid is a legal argument; the +1 element is
            // guarded immediately.
            let el = CfGuard::new(unsafe { AXUIElementCreateApplication(pid) });
            if el.is_null() {
                return Err("AXUIElementCreateApplication returned null".to_string());
            }
            el
        }
        None => {
            // Safety: CreateSystemWide takes nothing and returns a +1
            // element, guarded immediately.
            let sys = CfGuard::new(unsafe { AXUIElementCreateSystemWide() });
            if sys.is_null() {
                return Err("AXUIElementCreateSystemWide returned null".to_string());
            }
            copy_attr(&sys, "AXFocusedApplication")
                .map_err(|e| format!("no frontmost application: {e}"))?
        }
    };
    // Never let a slow app hang the launcher: bound every AX call on this
    // app and, once resolved, its window.
    set_messaging_timeout(&app, 1.0);
    let window = copy_attr(&app, "AXFocusedWindow")
        .map_err(|e| format!("no focused window on the frontmost app: {e}"))?;
    set_messaging_timeout(&window, 1.0);
    Ok(FocusedWindow { window })
}

impl FocusedWindow {
    /// Read AXPosition (top-left corner, AX coordinates).
    pub fn position(&self) -> Result<NSPoint, String> {
        let value = copy_attr(&self.window, "AXPosition")?;
        let mut p = NSPoint::default();
        // Safety: value guards a live AXValue; the out pointer is a
        // CGPoint-layout slot (module invariant 2), written only when
        // the call reports success.
        let ok = unsafe {
            AXValueGetValue(
                value.as_ptr(),
                AX_VALUE_TYPE_CGPOINT,
                std::ptr::from_mut(&mut p).cast(),
            )
        };
        if ok == 0 {
            return Err("AXPosition did not decode as a CGPoint".to_string());
        }
        Ok(p)
    }

    /// Read AXSize.
    pub fn size(&self) -> Result<NSSize, String> {
        let value = copy_attr(&self.window, "AXSize")?;
        let mut s = NSSize::default();
        // Safety: as in position, with the CGSize layout.
        let ok = unsafe {
            AXValueGetValue(
                value.as_ptr(),
                AX_VALUE_TYPE_CGSIZE,
                std::ptr::from_mut(&mut s).cast(),
            )
        };
        if ok == 0 {
            return Err("AXSize did not decode as a CGSize".to_string());
        }
        Ok(s)
    }

    /// Write AXPosition (top-left corner, AX coordinates).
    pub fn set_position(&self, p: NSPoint) -> Result<(), String> {
        // Safety: AXValueCreate copies the CGPoint-layout bytes before
        // returning (module invariant 2); the +1 value is guarded.
        let value = CfGuard::new(unsafe {
            AXValueCreate(AX_VALUE_TYPE_CGPOINT, std::ptr::from_ref(&p).cast())
        });
        if value.is_null() {
            return Err("AXValueCreate(CGPoint) failed".to_string());
        }
        set_attr(&self.window, "AXPosition", &value)
    }

    /// Write AXSize.
    pub fn set_size(&self, s: NSSize) -> Result<(), String> {
        // Safety: as in set_position, with the CGSize layout.
        let value = CfGuard::new(unsafe {
            AXValueCreate(AX_VALUE_TYPE_CGSIZE, std::ptr::from_ref(&s).cast())
        });
        if value.is_null() {
            return Err("AXValueCreate(CGSize) failed".to_string());
        }
        set_attr(&self.window, "AXSize", &value)
    }
}
