//! Accessibility window management: halves, thirds, quarters, maximize,
//! center, and display-to-display moves for the focused window of the
//! frontmost app.
//!
//! All layout math is pure integer geometry over (screen rects, window
//! rect) in AX top-left coordinates, so every action is golden-testable
//! with simulated displays and no hardware. CGFloat crosses the FFI
//! boundary (AX and NSScreen speak f64); values are rounded to integer
//! points at the edge, in exactly two places (screens_ax and activate),
//! and floats never enter the geometry functions.
//!
//! Segment boundaries are cumulative integer divisions
//! (start + len * k / n), so halves, thirds, and quarters share
//! boundaries and tile the screen exactly even when the dimension does
//! not divide evenly; the rightmost and bottommost segments absorb the
//! remainder.
//!
//! Degradation: without the Accessibility grant every action returns a
//! clear Err pointing at System Settings; nothing panics and nothing is
//! attempted, because trust is checked before any AX call that could
//! block on another app.
//!
//! Contract for the engine: [`items`] lists the actions as
//! SystemCommand rows in a fixed deterministic order, [`activate`] runs
//! one by id, [`is_trusted`] and [`prompt_for_trust`] expose the
//! permission state.

// Wired into the engine by the integrator; until that lands nothing in

use crate::ax;
use crate::ffi::{self, msg, Id, NSPoint, NSSize};
use beckon_core::router::{Item, ItemKind};

/// Every action, as (id, title), in the order items() presents them.
/// Ids are stable API (frecency keys, hotkey bindings); titles are what
/// fuzzy matching runs against.
const ACTIONS: &[(&str, &str)] = &[
    ("window.left-half", "Window: Left Half"),
    ("window.right-half", "Window: Right Half"),
    ("window.top-half", "Window: Top Half"),
    ("window.bottom-half", "Window: Bottom Half"),
    ("window.left-third", "Window: Left Third"),
    ("window.center-third", "Window: Center Third"),
    ("window.right-third", "Window: Right Third"),
    ("window.left-two-thirds", "Window: Left Two Thirds"),
    ("window.right-two-thirds", "Window: Right Two Thirds"),
    ("window.top-left-quarter", "Window: Top Left Quarter"),
    ("window.top-right-quarter", "Window: Top Right Quarter"),
    ("window.bottom-left-quarter", "Window: Bottom Left Quarter"),
    (
        "window.bottom-right-quarter",
        "Window: Bottom Right Quarter",
    ),
    ("window.maximize", "Window: Maximize"),
    ("window.center", "Window: Center"),
    ("window.next-display", "Window: Next Display"),
    ("window.previous-display", "Window: Previous Display"),
];

/// An integer rectangle in AX coordinates: top-left origin, y growing
/// downward, whole points. The only rect type the geometry functions
/// ever see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// The window-management rows for the registry, in ACTIONS order.
pub fn items() -> Vec<Item> {
    ACTIONS
        .iter()
        .map(|(id, title)| Item::new(id, title, "Window management", ItemKind::SystemCommand))
        .collect()
}

/// True when macOS has granted this process the Accessibility permission
/// (for an unbundled debug binary, trust is inherited from the terminal
/// it was launched from).
pub fn is_trusted() -> bool {
    ax::is_trusted()
}

/// Ask macOS to show the Accessibility grant dialog for this process.
pub fn prompt_for_trust() {
    ax::prompt_for_trust();
}

/// Run one window action by id on the focused window of the frontmost
/// app. Errs (never panics) on an unknown id, a missing permission, a
/// missing window, or an AX refusal from the target app.
pub fn activate(id: &str) -> Result<(), String> {
    if !ACTIONS.iter().any(|(aid, _)| *aid == id) {
        return Err(format!("unknown window action: {id}"));
    }
    // Trust gates every AX call: an untrusted process would only collect
    // kAXErrorAPIDisabled from the calls below, so fail fast with the
    // fix instead (module docs, degradation).
    if !ax::is_trusted() {
        return Err(
            "beckon cannot move windows yet: macOS has not granted it the Accessibility \
             permission. Open System Settings > Privacy & Security > Accessibility, add \
             beckon, and turn it on; then try again."
                .to_string(),
        );
    }
    let win = ax::focused_window()?;
    let p = win.position()?;
    let s = win.size()?;
    // The float-to-integer edge for the window frame (module docs).
    let cur = Rect {
        x: round_i64(p.x),
        y: round_i64(p.y),
        w: round_i64(s.width),
        h: round_i64(s.height),
    };
    let screens = screens_ax()?;
    let target =
        compute_target(id, &screens, cur).ok_or_else(|| format!("no target geometry for {id}"))?;
    apply(&win, target)
}

/// Write a target frame to the window. Position first so a cross-display
/// move lands on the destination before the resize (apps validate sizes
/// against their current screen), then size, then position once more:
/// when the window was too large for the destination the first move may
/// have been clamped by the app while still at the old size.
///
/// The three writes are best-effort: a transient AX failure on one (a busy
/// app letting a single message time out) must NOT abort the sequence, or
/// the window is left moved but not resized, the classic symptom. So the
/// per-call errors are ignored on the first pass and the outcome is judged
/// by reading the frame back. If it did not land (an app that clamped, or
/// applied the writes out of order), one corrective pass runs and this time
/// surfaces a real refusal.
fn apply(win: &ax::FocusedWindow, r: Rect) -> Result<(), String> {
    let origin = NSPoint {
        x: r.x as f64,
        y: r.y as f64,
    };
    let size = NSSize {
        width: r.w as f64,
        height: r.h as f64,
    };
    let _ = win.set_position(origin);
    let _ = win.set_size(size);
    let _ = win.set_position(origin);
    if frame_matches(win, r) {
        return Ok(());
    }
    win.set_position(origin)?;
    win.set_size(size)?;
    win.set_position(origin)?;
    Ok(())
}

/// True when the window's current frame sits within a couple of points of
/// the target on every edge. A read failure, or an app that clamps to a
/// size it will not give up, reads as "not matched", which at worst
/// triggers the single corrective pass in [`apply`].
fn frame_matches(win: &ax::FocusedWindow, r: Rect) -> bool {
    const TOL: i64 = 2;
    match (win.position(), win.size()) {
        (Ok(p), Ok(s)) => {
            (round_i64(p.x) - r.x).abs() <= TOL
                && (round_i64(p.y) - r.y).abs() <= TOL
                && (round_i64(s.width) - r.w).abs() <= TOL
                && (round_i64(s.height) - r.h).abs() <= TOL
        }
        _ => false,
    }
}

/// Round a CGFloat crossing the FFI edge to whole points. With frames
/// and visibleFrames these are integral already except on fractional
/// backing-scale setups, where nearest-point is the right snap.
fn round_i64(v: f64) -> i64 {
    v.round() as i64
}

/// Flip a rect between AppKit and AX vertical coordinates. AppKit rects
/// (NSScreen frame and visibleFrame) have a bottom-left origin with y
/// growing up; the AX API (AXPosition) has a top-left origin at the top
/// left of the primary display with y growing down. The flip is
/// y_ax = primary_height - y_appkit - height, anchored to the primary
/// display's full frame height, and it is its own inverse (the tests
/// exploit that). This is the one place vertical coordinates convert;
/// the classic window-management bug is doing it in two places or zero.
fn appkit_to_ax(primary_height: i64, r: Rect) -> Rect {
    Rect {
        x: r.x,
        y: primary_height - r.y - r.h,
        w: r.w,
        h: r.h,
    }
}

/// Snapshot every display's visible frame (menu bar and Dock excluded)
/// as integer AX rects. NSScreen's first entry is the primary display;
/// its full frame height anchors the coordinate flip. This is the other
/// float-to-integer edge (module docs).
fn screens_ax() -> Result<Vec<Rect>, String> {
    let _pool = ffi::AutoreleasePool::new();
    // Safety: screens returns an autoreleased NSArray of NSScreen kept
    // alive by the pool for this scope; count and objectAtIndex: have
    // the spelled signatures; frame and visibleFrame return NSRect
    // through the struct-return helper (ffi module invariant 1).
    unsafe {
        let screens = msg!(Id: ffi::class("NSScreen"), ffi::sel("screens"));
        if screens.is_null() {
            return Err("NSScreen screens returned nil".to_string());
        }
        let count = msg!(usize: screens, ffi::sel("count"));
        if count == 0 {
            return Err("no displays".to_string());
        }
        let primary = msg!(Id: screens, ffi::sel("objectAtIndex:"), usize: 0);
        let primary_h = round_i64(ffi::msg_send_nsrect(primary, ffi::sel("frame")).size.height);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let s = msg!(Id: screens, ffi::sel("objectAtIndex:"), usize: i);
            let v = ffi::msg_send_nsrect(s, ffi::sel("visibleFrame"));
            let appkit = Rect {
                x: round_i64(v.origin.x),
                y: round_i64(v.origin.y),
                w: round_i64(v.size.width),
                h: round_i64(v.size.height),
            };
            out.push(appkit_to_ax(primary_h, appkit));
        }
        Ok(out)
    }
}

/// The half-open span of grid cells i..j out of n over the segment
/// starting at start with length len, as (start, length). Boundaries are
/// cumulative integer divisions (start + len * k / n), so adjacent spans
/// share boundaries and the n unit spans tile the segment exactly even
/// when len % n != 0 (module docs).
fn span(start: i64, len: i64, i: i64, j: i64, n: i64) -> (i64, i64) {
    let a = start + len * i / n;
    let b = start + len * j / n;
    (a, b - a)
}

/// Which screen the window belongs to: the one containing its center,
/// else the one whose center is nearest (first wins ties). A window can
/// be off every screen mid-drag or right after a display unplugs.
fn screen_index(screens: &[Rect], win: Rect) -> usize {
    let cx = win.x + win.w / 2;
    let cy = win.y + win.h / 2;
    for (i, s) in screens.iter().enumerate() {
        if (s.x..s.x + s.w).contains(&cx) && (s.y..s.y + s.h).contains(&cy) {
            return i;
        }
    }
    let mut best = 0;
    let mut best_d = i64::MAX;
    for (i, s) in screens.iter().enumerate() {
        let sx = s.x + s.w / 2;
        let sy = s.y + s.h / 2;
        let d = (sx - cx) * (sx - cx) + (sy - cy) * (sy - cy);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Move the window step displays along the NSScreen order (wrapping),
/// preserving its position and size relative to the source visible
/// frame by proportional scaling, then clamping so the whole window
/// fits inside the target visible frame. The max(1) guards keep the
/// arithmetic total on degenerate rects; real visibleFrames are never
/// zero-sized.
fn move_display(screens: &[Rect], win: Rect, step: i64) -> Rect {
    let n = screens.len() as i64;
    let idx = screen_index(screens, win) as i64;
    let from = screens[idx as usize];
    let to = screens[(idx + step).rem_euclid(n) as usize];
    let fw = from.w.max(1);
    let fh = from.h.max(1);
    let tw = to.w.max(1);
    let th = to.h.max(1);
    let w = (win.w * tw / fw).clamp(1, tw);
    let h = (win.h * th / fh).clamp(1, th);
    let x = (to.x + (win.x - from.x) * tw / fw).clamp(to.x, to.x + tw - w);
    let y = (to.y + (win.y - from.y) * th / fh).clamp(to.y, to.y + th - h);
    Rect { x, y, w, h }
}

/// The target frame for an action: a pure function of (action id, screen
/// visible frames in AX coordinates, current window frame). None means
/// an unknown id or no screens; every id in ACTIONS yields Some when at
/// least one screen exists (locked by a test).
fn compute_target(id: &str, screens: &[Rect], win: Rect) -> Option<Rect> {
    if screens.is_empty() {
        return None;
    }
    match id {
        "window.next-display" => return Some(move_display(screens, win, 1)),
        "window.previous-display" => return Some(move_display(screens, win, -1)),
        _ => {}
    }
    let s = screens[screen_index(screens, win)];
    let full_x = (s.x, s.w);
    let full_y = (s.y, s.h);
    let (xs, ys) = match id {
        "window.left-half" => (span(s.x, s.w, 0, 1, 2), full_y),
        "window.right-half" => (span(s.x, s.w, 1, 2, 2), full_y),
        "window.top-half" => (full_x, span(s.y, s.h, 0, 1, 2)),
        "window.bottom-half" => (full_x, span(s.y, s.h, 1, 2, 2)),
        "window.left-third" => (span(s.x, s.w, 0, 1, 3), full_y),
        "window.center-third" => (span(s.x, s.w, 1, 2, 3), full_y),
        "window.right-third" => (span(s.x, s.w, 2, 3, 3), full_y),
        "window.left-two-thirds" => (span(s.x, s.w, 0, 2, 3), full_y),
        "window.right-two-thirds" => (span(s.x, s.w, 1, 3, 3), full_y),
        "window.top-left-quarter" => (span(s.x, s.w, 0, 1, 2), span(s.y, s.h, 0, 1, 2)),
        "window.top-right-quarter" => (span(s.x, s.w, 1, 2, 2), span(s.y, s.h, 0, 1, 2)),
        "window.bottom-left-quarter" => (span(s.x, s.w, 0, 1, 2), span(s.y, s.h, 1, 2, 2)),
        "window.bottom-right-quarter" => (span(s.x, s.w, 1, 2, 2), span(s.y, s.h, 1, 2, 2)),
        "window.maximize" => (full_x, full_y),
        "window.center" => {
            return Some(Rect {
                x: s.x + (s.w - win.w) / 2,
                y: s.y + (s.h - win.h) / 2,
                w: win.w,
                h: win.h,
            })
        }
        _ => return None,
    };
    Some(Rect {
        x: xs.0,
        y: ys.0,
        w: xs.1,
        h: ys.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1600x1000 primary display with a 25 point menu bar, already in
    /// AX coordinates (visible frame starts 25 points down).
    const SCREEN: Rect = Rect {
        x: 0,
        y: 25,
        w: 1600,
        h: 975,
    };

    fn win() -> Rect {
        Rect {
            x: 100,
            y: 100,
            w: 800,
            h: 600,
        }
    }

    fn one() -> Vec<Rect> {
        vec![SCREEN]
    }

    fn t(id: &str) -> Rect {
        compute_target(id, &one(), win()).expect("known action")
    }

    fn r(x: i64, y: i64, w: i64, h: i64) -> Rect {
        Rect { x, y, w, h }
    }

    // Golden: every layout action on the canonical screen. Locked so a
    // refactor of span or compute_target cannot silently reshape any
    // action.
    #[test]
    fn golden_halves() {
        assert_eq!(t("window.left-half"), r(0, 25, 800, 975));
        assert_eq!(t("window.right-half"), r(800, 25, 800, 975));
        assert_eq!(t("window.top-half"), r(0, 25, 1600, 487));
        assert_eq!(t("window.bottom-half"), r(0, 512, 1600, 488));
    }

    #[test]
    fn golden_thirds() {
        assert_eq!(t("window.left-third"), r(0, 25, 533, 975));
        assert_eq!(t("window.center-third"), r(533, 25, 533, 975));
        assert_eq!(t("window.right-third"), r(1066, 25, 534, 975));
        assert_eq!(t("window.left-two-thirds"), r(0, 25, 1066, 975));
        assert_eq!(t("window.right-two-thirds"), r(533, 25, 1067, 975));
    }

    #[test]
    fn golden_quarters() {
        assert_eq!(t("window.top-left-quarter"), r(0, 25, 800, 487));
        assert_eq!(t("window.top-right-quarter"), r(800, 25, 800, 487));
        assert_eq!(t("window.bottom-left-quarter"), r(0, 512, 800, 488));
        assert_eq!(t("window.bottom-right-quarter"), r(800, 512, 800, 488));
    }

    #[test]
    fn golden_maximize_and_center() {
        assert_eq!(t("window.maximize"), SCREEN);
        // Center keeps the size and splits the margins evenly:
        // x = (1600 - 800) / 2, y = 25 + (975 - 600) / 2.
        assert_eq!(t("window.center"), r(400, 212, 800, 600));
    }

    // Odd dimensions: segments must tile exactly, remainder to the last.
    #[test]
    fn segments_tile_odd_dimensions_exactly() {
        let s = r(7, 3, 1601, 977);
        let screens = vec![s];
        let w = Rect {
            x: 10,
            y: 10,
            w: 100,
            h: 100,
        };
        let lh = compute_target("window.left-half", &screens, w).unwrap();
        let rh = compute_target("window.right-half", &screens, w).unwrap();
        assert_eq!(lh.x + lh.w, rh.x);
        assert_eq!(lh.w + rh.w, s.w);
        let lt = compute_target("window.left-third", &screens, w).unwrap();
        let ct = compute_target("window.center-third", &screens, w).unwrap();
        let rt = compute_target("window.right-third", &screens, w).unwrap();
        assert_eq!(lt.x + lt.w, ct.x);
        assert_eq!(ct.x + ct.w, rt.x);
        assert_eq!(lt.w + ct.w + rt.w, s.w);
        let th = compute_target("window.top-half", &screens, w).unwrap();
        let bh = compute_target("window.bottom-half", &screens, w).unwrap();
        assert_eq!(th.y + th.h, bh.y);
        assert_eq!(th.h + bh.h, s.h);
        // Two-thirds boundaries coincide with the one-third boundaries.
        let l2 = compute_target("window.left-two-thirds", &screens, w).unwrap();
        let r2 = compute_target("window.right-two-thirds", &screens, w).unwrap();
        assert_eq!(l2.x + l2.w, rt.x);
        assert_eq!(r2.x, ct.x);
        assert_eq!(r2.x + r2.w, s.x + s.w);
    }

    // The coordinate flip: golden values plus the involution property.
    #[test]
    fn appkit_to_ax_flip() {
        // A 1600x1000 primary: an AppKit visible frame sitting on the
        // Dock-free bottom (y 0, height 975, menu bar occupying the top
        // 25) lands 25 points down in AX coordinates.
        assert_eq!(appkit_to_ax(1000, r(0, 0, 1600, 975)), r(0, 25, 1600, 975));
        // A second display to the left, taller than the primary and
        // bottom-aligned with it: AppKit y 0 with height 1400 tops out
        // at AppKit y 1400, which is 400 above the primary's top, so
        // AX y -400 (1000 - 0 - 1400).
        assert_eq!(
            appkit_to_ax(1000, r(-2560, 0, 2560, 1400)),
            r(-2560, -400, 2560, 1400)
        );
        // The flip is its own inverse.
        let rects = [r(0, 25, 1600, 975), r(-3, -7, 11, 13), r(500, 900, 1, 1)];
        for rect in rects {
            assert_eq!(appkit_to_ax(1000, appkit_to_ax(1000, rect)), rect);
        }
    }

    #[test]
    fn screen_index_prefers_containment_then_nearest() {
        let screens = vec![r(0, 0, 1600, 1000), r(1600, 0, 1280, 800)];
        // Center inside the second screen.
        assert_eq!(screen_index(&screens, r(1700, 100, 400, 300)), 1);
        // Center inside the first.
        assert_eq!(screen_index(&screens, r(100, 100, 400, 300)), 0);
        // Straddling: the center decides.
        assert_eq!(screen_index(&screens, r(1500, 100, 400, 300)), 1);
        // Off every screen: nearest center wins.
        assert_eq!(screen_index(&screens, r(-5000, 100, 100, 100)), 0);
        assert_eq!(screen_index(&screens, r(9000, 100, 100, 100)), 1);
    }

    // Golden: display moves preserve relative geometry by proportional
    // scaling.
    #[test]
    fn golden_next_display_scales_proportionally() {
        let screens = vec![r(0, 0, 1600, 1000), r(1600, 0, 800, 500)];
        let w = r(200, 100, 800, 600);
        let moved = compute_target("window.next-display", &screens, w).unwrap();
        // Half the source width and height, offsets halved too.
        assert_eq!(moved, r(1700, 50, 400, 300));
        // And back: previous-display restores the original exactly here
        // because the scale factors are exact inverses.
        let back = compute_target("window.previous-display", &screens, moved).unwrap();
        assert_eq!(back, w);
    }

    #[test]
    fn display_moves_wrap_around() {
        let screens = vec![
            r(0, 0, 1600, 1000),
            r(1600, 0, 1600, 1000),
            r(3200, 0, 1600, 1000),
        ];
        let w = r(100, 100, 800, 600);
        // next from 0 lands on 1, again on 2, again wraps to 0.
        let a = compute_target("window.next-display", &screens, w).unwrap();
        assert_eq!(screen_index(&screens, a), 1);
        let b = compute_target("window.next-display", &screens, a).unwrap();
        assert_eq!(screen_index(&screens, b), 2);
        let c = compute_target("window.next-display", &screens, b).unwrap();
        assert_eq!(screen_index(&screens, c), 0);
        assert_eq!(c, w);
        // previous from 0 wraps to the last.
        let p = compute_target("window.previous-display", &screens, w).unwrap();
        assert_eq!(screen_index(&screens, p), 2);
    }

    #[test]
    fn display_move_clamps_into_the_target_frame() {
        let screens = vec![r(0, 0, 1000, 1000), r(1000, 200, 500, 400)];
        // A window filling the whole source screen.
        let w = r(0, 0, 1000, 1000);
        let moved = compute_target("window.next-display", &screens, w).unwrap();
        assert_eq!(moved, r(1000, 200, 500, 400));
        // A window in the source's bottom-right corner: the scaled
        // offsets (1350, 520) overshoot the target's far edges, so both
        // axes clamp and the window still lands fully inside.
        let corner = r(700, 800, 400, 300);
        let m = compute_target("window.next-display", &screens, corner).unwrap();
        assert_eq!(m, r(1300, 480, 200, 120));
        assert!(m.x >= 1000 && m.x + m.w <= 1500);
        assert!(m.y >= 200 && m.y + m.h <= 600);
    }

    #[test]
    fn every_action_has_a_target_and_ids_are_unique() {
        for (id, _) in ACTIONS {
            assert!(
                compute_target(id, &one(), win()).is_some(),
                "no geometry for {id}"
            );
        }
        let mut ids: Vec<&str> = ACTIONS.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ACTIONS.len(), "duplicate action id");
    }

    #[test]
    fn unknown_id_and_empty_screens_yield_none() {
        assert_eq!(compute_target("window.bogus", &one(), win()), None);
        assert_eq!(compute_target("window.maximize", &[], win()), None);
    }

    #[test]
    fn items_are_deterministic_system_commands_in_actions_order() {
        let a = items();
        let b = items();
        assert_eq!(a, b);
        assert_eq!(a.len(), ACTIONS.len());
        for (item, (id, title)) in a.iter().zip(ACTIONS) {
            assert_eq!(item.id, *id);
            assert_eq!(item.title, *title);
            assert_eq!(item.kind, ItemKind::SystemCommand);
        }
        assert_eq!(a[0].id, "window.left-half");
    }

    #[test]
    fn activate_rejects_unknown_ids_before_touching_ax() {
        let err = activate("window.bogus").unwrap_err();
        assert!(err.contains("unknown window action"), "{err}");
    }

    #[test]
    fn round_i64_snaps_to_nearest_point() {
        assert_eq!(round_i64(1.4), 1);
        assert_eq!(round_i64(1.6), 2);
        assert_eq!(round_i64(-0.6), -1);
        assert_eq!(round_i64(25.0), 25);
    }

    // Hardware tests. Run one at a time, by name, from a terminal that
    // holds the Accessibility grant (an unbundled debug binary inherits
    // trust from its responsible process, the terminal):
    //
    //     cargo test -p beckon-macos hw_center_round_trip -- --ignored --nocapture
    //
    // They move the focused window of the frontmost app, which is the
    // terminal itself when run interactively.

    #[test]
    #[ignore = "hardware: moves the focused window of the frontmost app, then restores it"]
    fn hw_center_round_trip() {
        let trusted = is_trusted();
        println!("AXIsProcessTrusted = {trusted}");
        if !trusted {
            println!("no Accessibility grant in this context; round trip skipped");
            return;
        }
        let win = ax::focused_window().expect("a focused window on the frontmost app");
        let p0 = win.position().expect("read AXPosition");
        let s0 = win.size().expect("read AXSize");
        let cur = Rect {
            x: round_i64(p0.x),
            y: round_i64(p0.y),
            w: round_i64(s0.width),
            h: round_i64(s0.height),
        };
        println!("original frame = {cur:?}");
        let screens = screens_ax().expect("screen snapshot");
        println!("screens (AX coords) = {screens:?}");
        let expected = compute_target("window.center", &screens, cur).expect("center target");
        activate("window.center").expect("apply window.center");
        let p1 = win.position().expect("re-read AXPosition");
        let got = (round_i64(p1.x), round_i64(p1.y));
        // Restore before asserting so a failed assertion does not leave
        // the terminal window moved. Same write discipline as apply, and
        // a beat before the verification read: the target app services
        // AX writes asynchronously, and an immediate read can return the
        // pre-restore frame (observed live with the first cut of this
        // test).
        win.set_position(p0).expect("restore AXPosition");
        win.set_size(s0).expect("restore AXSize");
        win.set_position(p0).expect("restore AXPosition again");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let p2 = win.position().expect("read restored AXPosition");
        let s2 = win.size().expect("read restored AXSize");
        println!("expected center origin = ({}, {})", expected.x, expected.y);
        println!("observed center origin = ({}, {})", got.0, got.1);
        assert_eq!(
            got,
            (expected.x, expected.y),
            "center did not land where computed"
        );
        assert_eq!(
            (round_i64(p2.x), round_i64(p2.y)),
            (cur.x, cur.y),
            "restore did not return the original origin"
        );
        assert_eq!(
            (round_i64(s2.width), round_i64(s2.height)),
            (cur.w, cur.h),
            "restore did not return the original size"
        );
        println!("round trip OK: centered as computed, original frame restored");
    }

    #[test]
    #[ignore = "hardware: applies BECKON_WINMGMT_ACTION (default window.center) and leaves it"]
    fn hw_apply_action_from_env() {
        let id =
            std::env::var("BECKON_WINMGMT_ACTION").unwrap_or_else(|_| "window.center".to_string());
        println!("AXIsProcessTrusted = {}", is_trusted());
        match activate(&id) {
            Ok(()) => println!("applied {id}"),
            Err(e) => panic!("{e}"),
        }
    }
}
