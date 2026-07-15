//! Synthesized paste: post a Cmd+V keystroke to the frontmost app, the
//! piece that turns clipboard history from "copy back to the pasteboard"
//! into an actual paste. The flow the integrator wires up is: the user
//! picks a history entry, `pasteboard::activate` writes it to the general
//! pasteboard (and calls `note_own_write`), the panel is hidden, and then
//! [`paste_to_frontmost`] sends the chord.
//!
//! Caller contract, load-bearing:
//!
//! 1. Hide the panel BEFORE calling [`paste_to_frontmost`]. beckon's
//!    panel is a non-activating NSPanel, so the target app keeps key
//!    focus the whole time the panel is up; hiding first only removes
//!    the overlay, it does not change who receives the keystroke. If a
//!    future caller ever activates beckon, the paste would land in
//!    beckon itself.
//! 2. The pasteboard must already hold the text to paste (and
//!    `pasteboard::note_own_write` must already have run) before the
//!    chord is posted; this module only synthesizes the keystroke.
//!
//! Event tap choice: events are posted to kCGSessionEventTap, the point
//! where events enter the current login session, DOWNSTREAM of the HID
//! event system. Posting to kCGHIDEventTap instead would push the
//! synthetic chord through HID-level processing, where third-party
//! remappers (Karabiner and friends) and user keyboard remappings could
//! intercept or rewrite it; this chord means "deliver Cmd+V to the
//! focused app", not "pretend the physical keyboard sent it", so the
//! session tap is the correct entry point (and the one clipboard
//! managers conventionally use).
//!
//! Permissions: posting synthesized keyboard events requires the
//! Accessibility grant. Without it macOS silently drops the events (or
//! fails without any user-visible sign), so [`paste_to_frontmost`]
//! refuses up front with a clear Err instead of no-opping; onboarding.rs
//! owns getting the grant.
//!
//! Testing safety, non-negotiable: no test in this module ever posts a
//! real Cmd+V. Focus on a developer machine is unpredictable and pasting
//! arbitrary clipboard content into a focused terminal can execute
//! commands. The module is therefore split so everything up to the post
//! is pure and injectable: [`paste_gated`] drives an injected poster,
//! tests record the exact (keycode, flags, down/up) sequence and the
//! trust-gate refusal, and the only live FFI a test performs is creating
//! one CGEvent WITHOUT posting it, to prove the symbols link and
//! construction returns non-null. That is the hardware limit; posting is
//! verified by construction plus sequence tests, never fired.
//!
//! Safety invariants for this module, referenced by the unsafe blocks:
//!
//! 1. Ownership: CGEventCreateKeyboardEvent follows the CF Create rule,
//!    so its +1 result goes into an [`ax::CfGuard`] immediately and the
//!    matching CFRelease is structural (Drop), exactly as in ax.rs.
//! 2. Signatures: every extern declaration spells the real C prototype
//!    from the CoreGraphics SDK headers (CGEventTypes.h, CGEvent.h).
//!    CGEventRef is an opaque CF pointer, CGKeyCode is uint16_t,
//!    CGEventFlags is uint64_t, CGEventTapLocation is uint32_t, and the
//!    keyDown parameter is a C _Bool, ABI-identical to Rust bool.
//! 3. Threading: CGEventPost is not main-thread bound, but callers here
//!    run on the main thread like the rest of the shell.

// Wired into the engine by the integrator; until that lands nothing in
// main calls this module, so the dead-code lint is silenced file-wide.
// Remove the allow with the first caller.
#![allow(dead_code)]

use crate::ax::{self, CFTypeRef, CfGuard};

/// CGEventRef from CGEventTypes.h: an opaque CF object pointer.
type CGEventRef = CFTypeRef;

/// CGEventSourceRef; only ever passed as null here (the "combined
/// session state" default source).
type CGEventSourceRef = CFTypeRef;

/// CGKeyCode from CGRemoteOperation.h: uint16_t.
type CGKeyCode = u16;

/// CGEventFlags from CGEventTypes.h: uint64_t bitmask.
type CGEventFlags = u64;

/// CGEventTapLocation from CGEventTypes.h: uint32_t enum.
type CGEventTapLocation = u32;

/// kCGSessionEventTap: events enter at the current login session, past
/// HID-level remapping. See the module docs for why this tap and not
/// kCGHIDEventTap (which is 0).
const SESSION_EVENT_TAP: CGEventTapLocation = 1;

/// kCGEventFlagMaskCommand from CGEventTypes.h.
const FLAG_COMMAND: CGEventFlags = 1 << 20;

/// kVK_ANSI_V from Carbon's Events.h. Virtual keycodes name positions on
/// the ANSI layout, so this is "the key that is V on a US keyboard";
/// macOS translates it through the active layout exactly as it would a
/// hardware press, which is what Cmd+V handling keys off anyway (the
/// command chord is layout-stable in every mainstream layout).
const KEYCODE_V: CGKeyCode = 9;

#[allow(non_snake_case)]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: CGKeyCode,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    fn CGEventPost(tap: CGEventTapLocation, event: CGEventRef);
}

/// One synthesized keystroke, the unit the injected poster receives.
/// Everything that decides WHAT to post is expressed in these, so tests
/// can lock the exact sequence without any event ever being posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyStroke {
    keycode: CGKeyCode,
    flags: CGEventFlags,
    down: bool,
}

/// The complete paste chord, in posting order: V down with the command
/// flag, then V up with the command flag. The flag rides on both halves
/// so the up event is unambiguous to apps that track modifier state per
/// event (synthesized events have no surrounding real modifier-change
/// events to lean on).
const PASTE_SEQUENCE: [KeyStroke; 2] = [
    KeyStroke {
        keycode: KEYCODE_V,
        flags: FLAG_COMMAND,
        down: true,
    },
    KeyStroke {
        keycode: KEYCODE_V,
        flags: FLAG_COMMAND,
        down: false,
    },
];

/// The Err returned when the Accessibility grant is missing.
const UNTRUSTED_ERR: &str = "Accessibility permission not granted: macOS silently drops \
     synthesized keystrokes without it, so the paste would do nothing. \
     Run \"Grant Accessibility Access\" and approve beckon in System \
     Settings, then try again";

/// The trust gate plus the drive loop, with the poster injected: this is
/// [`paste_to_frontmost`] minus the live trust read and the live poster,
/// which makes both the refusal path and the exact posted sequence unit
/// testable. Strokes post in order; the first poster error aborts the
/// sequence and surfaces as the Err.
fn paste_gated(
    trusted: bool,
    post: &mut dyn FnMut(KeyStroke) -> Result<(), String>,
) -> Result<(), String> {
    if !trusted {
        return Err(UNTRUSTED_ERR.to_string());
    }
    for stroke in PASTE_SEQUENCE {
        post(stroke)?;
    }
    Ok(())
}

/// Create, flag, and post one keystroke event at the session tap. The
/// only function in this module that posts; nothing under #[cfg(test)]
/// ever calls it.
fn post_stroke(stroke: KeyStroke) -> Result<(), String> {
    // Safety: signatures per module invariant 2; a null source selects
    // the default combined-session event source. The +1 event goes
    // straight into a CfGuard (module invariant 1), which outlives both
    // calls that borrow it and releases it on scope exit. SetFlags and
    // Post only read our arguments; Post does not consume the event.
    unsafe {
        let event = CfGuard::new(CGEventCreateKeyboardEvent(
            std::ptr::null(),
            stroke.keycode,
            stroke.down,
        ));
        if event.is_null() {
            return Err("CGEventCreateKeyboardEvent returned null".to_string());
        }
        CGEventSetFlags(event.as_ptr(), stroke.flags);
        CGEventPost(SESSION_EVENT_TAP, event.as_ptr());
    }
    Ok(())
}

/// Post Cmd+V to whatever app currently has key focus. See the module
/// docs for the caller contract: hide the panel first (the panel is
/// non-activating, so the target app held key focus throughout) and have
/// the pasteboard already loaded. Refuses with a clear Err when the
/// Accessibility grant is missing, because without it the events are
/// silently dropped and an honest error beats a silent no-op.
pub fn paste_to_frontmost() -> Result<(), String> {
    paste_gated(ax::is_trusted(), &mut post_stroke)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Safety rule for this module's tests, non-negotiable: CGEventPost
    // is NEVER called from a test. These tests run on a live
    // workstation where focus is unpredictable and a real Cmd+V could
    // paste arbitrary clipboard content into a focused terminal. The
    // recorded-poster tests lock the full sequence; the construction
    // test creates (and releases) one event without posting it.

    /// A poster that records every stroke and never posts anything.
    fn recording(log: &mut Vec<KeyStroke>) -> impl FnMut(KeyStroke) -> Result<(), String> + '_ {
        |stroke| {
            log.push(stroke);
            Ok(())
        }
    }

    #[test]
    fn golden_paste_sequence_when_trusted() {
        let mut log = Vec::new();
        let result = paste_gated(true, &mut recording(&mut log));
        assert_eq!(result, Ok(()));
        assert_eq!(
            log,
            vec![
                KeyStroke {
                    keycode: 9,
                    flags: 1 << 20,
                    down: true,
                },
                KeyStroke {
                    keycode: 9,
                    flags: 1 << 20,
                    down: false,
                },
            ],
            "the chord must be exactly V-down then V-up, both command-flagged"
        );
    }

    #[test]
    fn untrusted_refuses_and_posts_nothing() {
        let mut log = Vec::new();
        let result = paste_gated(false, &mut recording(&mut log));
        let err = result.unwrap_err();
        assert!(
            err.contains("Accessibility permission not granted"),
            "refusal must name the missing grant: {err}"
        );
        assert!(
            log.is_empty(),
            "no stroke may reach the poster when untrusted"
        );
    }

    #[test]
    fn poster_error_aborts_the_sequence() {
        let mut calls = 0usize;
        let result = paste_gated(true, &mut |_stroke| {
            calls += 1;
            Err("post failed".to_string())
        });
        assert_eq!(result, Err("post failed".to_string()));
        assert_eq!(calls, 1, "the first failure must stop the sequence");
    }

    // The hardware limit: prove the CoreGraphics symbols link and that
    // event construction succeeds, WITHOUT posting. The guard drop
    // exercises the CFRelease discipline on the created event.
    #[test]
    fn cgevent_construction_links_and_returns_non_null() {
        for stroke in PASTE_SEQUENCE {
            // Safety: as in post_stroke, minus the post: null source is
            // legal, the +1 event is guarded immediately, and SetFlags
            // only mutates the event we own. Nothing is posted.
            unsafe {
                let event = CfGuard::new(CGEventCreateKeyboardEvent(
                    std::ptr::null(),
                    stroke.keycode,
                    stroke.down,
                ));
                assert!(
                    !event.is_null(),
                    "CGEventCreateKeyboardEvent returned null for {stroke:?}"
                );
                CGEventSetFlags(event.as_ptr(), stroke.flags);
            }
        }
    }
}
