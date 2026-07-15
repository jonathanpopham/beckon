//! Global hotkey via Carbon RegisterEventHotKey. This is the one Carbon
//! API with no AppKit replacement; it still works on current macOS, needs
//! no Accessibility permission, and fires even when another app is
//! frontmost.
//!
//! The hotkey definition lives here in one place (KEY_CODE and MODIFIERS)
//! so a future config file has exactly one thing to feed.

use std::ffi::c_void;
use std::ptr;

/// kVK_Space from Carbon's Events.h.
pub const KEY_CODE: u32 = 49;
/// Carbon modifier mask: optionKey. The default chord is Option+Space.
pub const MODIFIERS: u32 = OPTION_KEY;

const OPTION_KEY: u32 = 0x0800;
const K_EVENT_CLASS_KEYBOARD: u32 = four_char(b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const SIGNATURE: u32 = four_char(b"bckn");

const fn four_char(b: &[u8; 4]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | (b[3] as u32)
}

pub type OSStatus = i32;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerProc = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> OSStatus;

#[allow(non_snake_case)]
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandlerProc,
        num_types: usize,
        type_list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_handler: *mut *mut c_void,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hotkey_id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> OSStatus;
}

/// Carbon calls this on the main thread whenever the registered hotkey
/// fires. user_data smuggles the Rust callback through as a fn pointer.
extern "C" fn hotkey_handler(
    _call_ref: *mut c_void,
    _event: *mut c_void,
    user_data: *mut c_void,
) -> OSStatus {
    // Safety: user_data is exactly the fn pointer register() passed to
    // InstallEventHandler; nothing else ever installs this handler.
    let callback = unsafe { std::mem::transmute::<*mut c_void, extern "C" fn()>(user_data) };
    callback();
    0
}

/// Register the global hotkey and route presses to `callback`. Call once,
/// on the main thread, with the app run loop about to run (the handler is
/// dispatched by that loop). Returns the failing OSStatus on error; a
/// chord already claimed by another app typically fails here.
pub fn register(callback: extern "C" fn()) -> Result<(), OSStatus> {
    // Safety: all pointers handed to Carbon are valid for the duration of
    // the call (it copies the spec) or for the process (the callback).
    // The out parameters are only written by Carbon.
    unsafe {
        let target = GetApplicationEventTarget();
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };
        let mut handler_ref: *mut c_void = ptr::null_mut();
        let status = InstallEventHandler(
            target,
            hotkey_handler,
            1,
            &spec,
            callback as *mut c_void,
            &mut handler_ref,
        );
        if status != 0 {
            return Err(status);
        }
        let hotkey_id = EventHotKeyID {
            signature: SIGNATURE,
            id: 1,
        };
        let mut hotkey_ref: *mut c_void = ptr::null_mut();
        let status =
            RegisterEventHotKey(KEY_CODE, MODIFIERS, hotkey_id, target, 0, &mut hotkey_ref);
        if status != 0 {
            return Err(status);
        }
    }
    Ok(())
}
