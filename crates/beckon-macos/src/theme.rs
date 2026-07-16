//! Theme application: turns the config file's theme section into live
//! panel styling. The deterministic model (hex strings, validation,
//! defaults) lives in beckon-core's config module; this file is the shell
//! edge that converts channels to AppKit colors via panel::set_colors.
//!
//! Integrator wiring (this wave lands the parts, the integrator connects
//! them; nothing here is called yet):
//!
//!   1. In engine::init, after the store root resolves, load the config:
//!      `config::load(&root.join(config::CONFIG_FILE))`, falling back to
//!      `Config::default()` on missing (and logging plus defaulting on
//!      corrupt, like the other stores).
//!   2. Call `theme::apply(&Theme::from_config(&config))` after
//!      panel::init so the panel and query field restyle before first
//!      show.
//!   3. Rows: ui.rs is not touched by this wave. When row styling is
//!      wired, ui.rs should call `theme::row_style()` where it creates or
//!      styles row text; title text uses `foreground`, the selection
//!      highlight uses `accent`, and `font_size` scales row fonts if
//!      desired. Until then rows keep their built-in colors.
//!
//! Threading: apply() must run on the main thread (it calls into
//! panel.rs); row_style() is safe from the main thread callbacks ui.rs
//! runs in. State is stored in packed atomics, std only.

// Nothing references this module until the integrator wires engine::init
// to it (see the module docs); the allow retires with that wiring.
#![allow(dead_code)]

use crate::panel;
use beckon_core::config::{self, Config};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// A resolved theme: channel tuples instead of hex strings, ready for the
/// FFI edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: (u8, u8, u8),
    pub foreground: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub font_size: u32,
}

/// What ui.rs needs to theme result rows, read via [`row_style`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowStyle {
    pub foreground: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub font_size: u32,
}

impl Default for Theme {
    /// The built-in panel look, matching the config defaults.
    fn default() -> Self {
        Theme::from_config(&Config::default())
    }
}

impl Theme {
    /// Resolve a config's theme section. The config parser already
    /// validated the hex strings, so the fallbacks here are unreachable
    /// for any Config that came through config::parse; they exist so a
    /// hand-built Config can never panic the shell.
    pub fn from_config(cfg: &Config) -> Theme {
        Theme {
            background: config::parse_hex_color(&cfg.theme.background).unwrap_or((28, 28, 33)),
            foreground: config::parse_hex_color(&cfg.theme.foreground).unwrap_or((255, 255, 255)),
            accent: config::parse_hex_color(&cfg.theme.accent).unwrap_or((90, 200, 250)),
            font_size: cfg.theme.font_size,
        }
    }
}

// The applied theme, packed for lock-free reads: 0x00RRGGBB per color.
static APPLIED: AtomicBool = AtomicBool::new(false);
static FOREGROUND: AtomicU32 = AtomicU32::new(0);
static ACCENT: AtomicU32 = AtomicU32::new(0);
static FONT_SIZE: AtomicU32 = AtomicU32::new(0);

fn pack(c: (u8, u8, u8)) -> u32 {
    (u32::from(c.0) << 16) | (u32::from(c.1) << 8) | u32::from(c.2)
}

fn unpack(n: u32) -> (u8, u8, u8) {
    (
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    )
}

/// Apply a theme: restyle the panel window and query field now, and
/// record the row-facing parts for ui.rs to read later. Main thread only
/// (panel invariant). Safe before panel::init (set_colors no-ops) and
/// idempotent, so the integrator may call it at startup and again
/// whenever the config reloads.
pub fn apply(theme: &Theme) {
    FOREGROUND.store(pack(theme.foreground), Ordering::Relaxed);
    ACCENT.store(pack(theme.accent), Ordering::Relaxed);
    FONT_SIZE.store(theme.font_size, Ordering::Relaxed);
    APPLIED.store(true, Ordering::Relaxed);
    panel::set_colors(theme.background, theme.foreground, theme.font_size);
}

/// The row-facing slice of the applied theme, or None if apply() has not
/// run; ui.rs keeps its built-in colors in that case.
pub fn row_style() -> Option<RowStyle> {
    if !APPLIED.load(Ordering::Relaxed) {
        return None;
    }
    Some(RowStyle {
        foreground: unpack(FOREGROUND.load(Ordering::Relaxed)),
        accent: unpack(ACCENT.load(Ordering::Relaxed)),
        font_size: FONT_SIZE.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_matches_the_builtin_panel_look() {
        let t = Theme::default();
        assert_eq!(t.background, (0x1C, 0x1C, 0x21));
        assert_eq!(t.foreground, (0xFF, 0xFF, 0xFF));
        assert_eq!(t.accent, (0x5A, 0xC8, 0xFA));
        assert_eq!(t.font_size, 22);
    }

    #[test]
    fn pack_unpack_round_trip() {
        for c in [(0, 0, 0), (255, 255, 255), (0x1C, 0x1C, 0x21), (1, 2, 3)] {
            assert_eq!(unpack(pack(c)), c);
        }
    }

    #[test]
    fn apply_before_panel_init_is_safe_and_records_row_style() {
        // PANEL is null in the test process, so panel::set_colors must
        // return without touching AppKit; the row style still lands.
        let t = Theme {
            background: (10, 20, 30),
            foreground: (200, 210, 220),
            accent: (1, 2, 3),
            font_size: 14,
        };
        apply(&t);
        let style = row_style().expect("row style after apply");
        assert_eq!(style.foreground, (200, 210, 220));
        assert_eq!(style.accent, (1, 2, 3));
        assert_eq!(style.font_size, 14);
    }
}
