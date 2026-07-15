//! The launcher panel: a borderless, non-activating floating NSPanel with
//! a query text field on top and the results list (ui.rs) beneath it. It
//! appears above normal windows on every space, takes keyboard focus
//! without activating the app (the Spotlight trick), and hides on Escape
//! or on losing key status.
//!
//! The panel is a runtime subclass (BeckonPanel) because a borderless
//! window refuses key status unless canBecomeKeyWindow is overridden, and
//! because Escape and focus loss arrive as cancelOperation: and
//! resignKeyWindow on the window itself.

use crate::ffi::{self, msg, Bool, Id, ObjcObject, Sel, NO, YES};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Once;

pub const PANEL_WIDTH: f64 = 640.0;
pub const PANEL_HEIGHT: f64 = 400.0;

// NSWindowStyleMask bits.
const STYLE_BORDERLESS: usize = 0;
const STYLE_NONACTIVATING_PANEL: usize = 1 << 7;
// NSBackingStoreBuffered.
const BACKING_BUFFERED: usize = 2;
// kCGFloatingWindowLevel; above normal windows, below the menu bar.
const LEVEL_FLOATING: isize = 3;
// NSWindowCollectionBehavior bits: follow the user to every space and
// coexist with full-screen apps.
const COLLECTION_CAN_JOIN_ALL_SPACES: usize = 1 << 0;
const COLLECTION_FULL_SCREEN_AUXILIARY: usize = 1 << 8;
// NSFocusRingTypeNone.
const FOCUS_RING_NONE: usize = 1;

static PANEL: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static FIELD: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static CLASS: AtomicPtr<ObjcObject> = AtomicPtr::new(ptr::null_mut());
static DEFINE: Once = Once::new();

/// Define the BeckonPanel subclass once and return it.
fn panel_class() -> Id {
    DEFINE.call_once(|| {
        // Safety: each Imp below is transmuted from an extern "C" fn whose
        // real signature matches the paired encoding, and the class name
        // is registered exactly once (guarded by the Once).
        let cls = unsafe {
            ffi::define_class(
                "BeckonPanel",
                "NSPanel",
                &[
                    (
                        "canBecomeKeyWindow",
                        std::mem::transmute::<extern "C" fn(Id, Sel) -> Bool, ffi::Imp>(
                            can_become_key_window,
                        ),
                        "c@:",
                    ),
                    (
                        "cancelOperation:",
                        std::mem::transmute::<extern "C" fn(Id, Sel, Id), ffi::Imp>(
                            cancel_operation,
                        ),
                        "v@:@",
                    ),
                    (
                        "resignKeyWindow",
                        std::mem::transmute::<extern "C" fn(Id, Sel), ffi::Imp>(resign_key_window),
                        "v@:",
                    ),
                ],
            )
        };
        CLASS.store(cls, Ordering::Relaxed);
    });
    CLASS.load(Ordering::Relaxed)
}

/// Borderless windows refuse key status by default; the query field can
/// only receive keystrokes if we say yes here.
extern "C" fn can_become_key_window(_this: Id, _sel: Sel) -> Bool {
    YES
}

/// Escape reaches the window as cancelOperation: when the responder chain
/// does not handle it earlier.
extern "C" fn cancel_operation(_this: Id, _sel: Sel, _sender: Id) {
    hide();
}

/// Losing key status (click elsewhere, another window summoned) dismisses
/// the launcher, matching Spotlight and Raycast behavior.
extern "C" fn resign_key_window(this: Id, _sel: Sel) {
    // Safety: this is a BeckonPanel instance, NSPanel is its superclass,
    // and resignKeyWindow has encoding v@: all the way up the chain.
    unsafe { ffi::msg_super_void(this, ffi::class("NSPanel"), ffi::sel("resignKeyWindow")) };
    hide();
}

/// Build the panel and its query field. Called once at startup, on the
/// main thread, before the run loop starts. `field_delegate` becomes the
/// text field's delegate so Escape can be intercepted while the field
/// editor has focus (control:textView:doCommandBySelector:).
pub fn init(field_delegate: Id) {
    let cls = panel_class();
    // Safety: main thread, before the run loop; every msg! spells the
    // documented AppKit signature (ffi module invariant 1). The panel and
    // field are never released (invariant 3).
    unsafe {
        let rect = ffi::NSRect::new(0.0, 0.0, PANEL_WIDTH, PANEL_HEIGHT);
        let panel = msg!(Id: msg!(Id: cls, ffi::sel("alloc")),
            ffi::sel("initWithContentRect:styleMask:backing:defer:"),
            ffi::NSRect: rect,
            usize: STYLE_BORDERLESS | STYLE_NONACTIVATING_PANEL,
            usize: BACKING_BUFFERED,
            Bool: NO);
        assert!(!panel.is_null(), "NSPanel init returned nil");

        msg!((): panel, ffi::sel("setLevel:"), isize: LEVEL_FLOATING);
        msg!((): panel, ffi::sel("setCollectionBehavior:"),
            usize: COLLECTION_CAN_JOIN_ALL_SPACES | COLLECTION_FULL_SCREEN_AUXILIARY);
        // We manage lifetime and visibility ourselves.
        msg!((): panel, ffi::sel("setReleasedWhenClosed:"), Bool: NO);
        msg!((): panel, ffi::sel("setHidesOnDeactivate:"), Bool: NO);
        // Transparent window, rounded dark layer underneath.
        msg!((): panel, ffi::sel("setOpaque:"), Bool: NO);
        let clear = msg!(Id: ffi::class("NSColor"), ffi::sel("clearColor"));
        msg!((): panel, ffi::sel("setBackgroundColor:"), Id: clear);
        msg!((): panel, ffi::sel("setHasShadow:"), Bool: YES);

        let content = msg!(Id: panel, ffi::sel("contentView"));
        msg!((): content, ffi::sel("setWantsLayer:"), Bool: YES);
        let layer = msg!(Id: content, ffi::sel("layer"));
        msg!((): layer, ffi::sel("setCornerRadius:"), f64: 14.0);
        let dark = msg!(Id: ffi::class("NSColor"),
            ffi::sel("colorWithCalibratedRed:green:blue:alpha:"),
            f64: 0.11, f64: 0.11, f64: 0.13, f64: 0.97);
        let dark_cg = msg!(Id: dark, ffi::sel("CGColor"));
        msg!((): layer, ffi::sel("setBackgroundColor:"), Id: dark_cg);

        let field_rect = ffi::NSRect::new(24.0, PANEL_HEIGHT - 64.0, PANEL_WIDTH - 48.0, 40.0);
        let field = msg!(Id: msg!(Id: ffi::class("NSTextField"), ffi::sel("alloc")),
            ffi::sel("initWithFrame:"), ffi::NSRect: field_rect);
        assert!(!field.is_null(), "NSTextField init returned nil");
        msg!((): field, ffi::sel("setEditable:"), Bool: YES);
        msg!((): field, ffi::sel("setSelectable:"), Bool: YES);
        msg!((): field, ffi::sel("setBezeled:"), Bool: NO);
        msg!((): field, ffi::sel("setBordered:"), Bool: NO);
        msg!((): field, ffi::sel("setDrawsBackground:"), Bool: NO);
        msg!((): field, ffi::sel("setFocusRingType:"), usize: FOCUS_RING_NONE);
        let font = msg!(Id: ffi::class("NSFont"), ffi::sel("systemFontOfSize:"), f64: 22.0);
        msg!((): field, ffi::sel("setFont:"), Id: font);
        let white = msg!(Id: ffi::class("NSColor"), ffi::sel("whiteColor"));
        msg!((): field, ffi::sel("setTextColor:"), Id: white);
        msg!((): field, ffi::sel("setPlaceholderString:"), Id: ffi::nsstring("Type to search"));
        msg!((): field, ffi::sel("setDelegate:"), Id: field_delegate);
        msg!((): content, ffi::sel("addSubview:"), Id: field);

        PANEL.store(panel, Ordering::Relaxed);
        FIELD.store(field, Ordering::Relaxed);

        // The results table lives in ui.rs; it sits under the field.
        crate::ui::install(content);
    }
}

/// Center the panel on the main screen (a little above center, launcher
/// style), order it front, and put the caret in the query field.
pub fn show() {
    let panel = PANEL.load(Ordering::Relaxed);
    if panel.is_null() {
        return;
    }
    // Safety: main thread; signatures match AppKit. mainScreen can be nil
    // (headless); positioning is skipped in that case.
    unsafe {
        let screen = msg!(Id: ffi::class("NSScreen"), ffi::sel("mainScreen"));
        if !screen.is_null() {
            let vf = ffi::msg_send_nsrect(screen, ffi::sel("visibleFrame"));
            let origin = ffi::NSPoint {
                x: vf.origin.x + (vf.size.width - PANEL_WIDTH) / 2.0,
                y: vf.origin.y + (vf.size.height - PANEL_HEIGHT) * 0.6,
            };
            msg!((): panel, ffi::sel("setFrameOrigin:"), ffi::NSPoint: origin);
        }
        msg!((): panel, ffi::sel("makeKeyAndOrderFront:"), Id: ffi::NIL);
        let field = FIELD.load(Ordering::Relaxed);
        let _ = msg!(Bool: panel, ffi::sel("makeFirstResponder:"), Id: field);
    }
}

/// Order the panel out. Idempotent; also called from resignKeyWindow.
pub fn hide() {
    let panel = PANEL.load(Ordering::Relaxed);
    if panel.is_null() {
        return;
    }
    if !is_visible() {
        return;
    }
    // Safety: main thread; orderOut: takes a nullable sender.
    unsafe { msg!((): panel, ffi::sel("orderOut:"), Id: ffi::NIL) };
}

pub fn toggle() {
    if is_visible() {
        hide();
    } else {
        show();
    }
}

pub fn is_visible() -> bool {
    let panel = PANEL.load(Ordering::Relaxed);
    if panel.is_null() {
        return false;
    }
    // Safety: main thread; isVisible returns BOOL.
    unsafe { msg!(Bool: panel, ffi::sel("isVisible")) != 0 }
}

/// Replace the query field's text (used by the smoke test to prove the
/// NSString round trip).
pub fn set_query(text: &str) {
    let field = FIELD.load(Ordering::Relaxed);
    if field.is_null() {
        return;
    }
    // Safety: main thread; setStringValue: takes an NSString.
    unsafe { msg!((): field, ffi::sel("setStringValue:"), Id: ffi::nsstring(text)) };
}

/// The query NSTextField, for callers that need to message it directly
/// (the smoke test posts the text-did-change notification against it).
/// Null until init has run.
pub fn field() -> Id {
    FIELD.load(Ordering::Relaxed)
}

/// Read the query field's current text.
pub fn query() -> String {
    let field = FIELD.load(Ordering::Relaxed);
    if field.is_null() {
        return String::new();
    }
    // Safety: main thread; stringValue returns an NSString.
    unsafe {
        let ns = msg!(Id: field, ffi::sel("stringValue"));
        ffi::nsstring_to_string(ns)
    }
}
