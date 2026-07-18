//! The walkthrough: beckon teaching itself, inside its own panel.
//!
//! On a fresh install the blank panel's first row (selected by default)
//! is the walkthrough, so pressing Return on an empty query starts it.
//! Each step is one screen of rows: the top row advances on Return, the
//! rows beneath are literal queries worth trying later. Typing anything
//! exits the tour (the query pipeline takes over); Escape hides the
//! panel as always. Finishing writes a marker file under the store root
//! so the blank-screen row retires; the tour stays reachable forever by
//! typing "walkthrough".
//!
//! This module owns the step content and row shaping; the engine owns
//! the step state and activation wiring.

use beckon_core::persist;
use std::path::PathBuf;

/// The marker file that retires the blank-screen walkthrough row.
const DONE_FILE: &str = "walkthrough_done";

/// One step of the tour: a headline, a one-line summary, and example
/// (query, explanation) rows.
pub struct Step {
    pub title: &'static str,
    pub summary: &'static str,
    pub examples: &'static [(&'static str, &'static str)],
}

/// The tour. Order is the story: launch, navigate, clipboard, paths,
/// calculate, deep cuts, make it yours.
pub const STEPS: &[Step] = &[
    Step {
        title: "Welcome to beckon",
        summary: "type to search; it learns your habits",
        examples: &[
            ("saf", "fuzzy matches Safari"),
            ("terminal", "every app on the Mac"),
        ],
    },
    Step {
        title: "Drive your windows",
        summary: "window actions, no mouse",
        examples: &[
            ("window left half", "halves, thirds, quarters"),
            ("win", "switch to any open window"),
            ("menu export", "search the app's menus"),
        ],
    },
    Step {
        title: "Clipboard history",
        summary: "secrets are never captured",
        examples: &[
            ("clip", "Return pastes it back"),
            ("clip invoice", "search your history"),
        ],
    },
    Step {
        title: "Stop hunting files",
        summary: "walk the disk from the keyboard",
        examples: &[
            ("~/", "browse; Return drills in"),
            ("~/Downloads", "Open, Reveal, Quick Look"),
            ("file report", "fuzzy file search"),
        ],
    },
    Step {
        title: "It does math properly",
        summary: "exact answers, no float drift",
        examples: &[
            ("0.1 + 0.2", "exactly 0.3"),
            ("5 km in mi", "units and bases"),
            ("uuid", "b64, sha256, epoch too"),
        ],
    },
    Step {
        title: "The deep cuts",
        summary: "emoji, snippets, quicklinks",
        examples: &[
            ("emoji fire", "Return pastes the glyph"),
            ("snip", "snippets with {date}"),
            ("go google rust", "quicklinks with {query}"),
        ],
    },
    Step {
        title: "Make it yours",
        summary: "plain files in ~/.beckon",
        examples: &[
            ("~/.beckon/config.json", "hotkey, theme, aliases"),
            ("~/.beckon/scripts", "scripts become commands"),
            ("hello", "the example script"),
        ],
    },
];

fn done_path_in(root: &std::path::Path) -> PathBuf {
    root.join(DONE_FILE)
}

/// Whether the tour has been finished on this store (retires the
/// blank-screen row; the "walkthrough" command remains).
pub fn is_done() -> bool {
    done_path_in(&persist::store_root()).exists()
}

/// Persist the finish so the blank-screen row retires. Best effort: a
/// write failure just means the row shows again next launch.
pub fn mark_done() {
    let result = persist::ensure_store_root()
        .and_then(|root| std::fs::write(done_path_in(&root), b"").map(|()| root));
    if let Err(e) = result {
        eprintln!("beckon: cannot record walkthrough completion: {e}");
    }
}

/// The rows for step `index`. Row 0 advances (or finishes) on Return;
/// the example rows are inert. Returns None past the end.
pub fn step_rows(index: usize) -> Option<Vec<(String, String, bool)>> {
    let step = STEPS.get(index)?;
    let last = index + 1 == STEPS.len();
    let lead = if last {
        format!("\u{2713} {}", step.title)
    } else {
        format!("\u{25B8} {}/{} {}", index + 1, STEPS.len(), step.title)
    };
    let mut rows = vec![(lead, step.summary.to_string(), true)];
    for (query, why) in step.examples {
        rows.push((format!("try: {query}"), (*why).to_string(), false));
    }
    Some(rows)
}

/// Every example from every step as one flat (query, why) list: the
/// home view's rotating discovery slots draw from this, so the tour and
/// the home screen teach from the same curriculum.
pub fn tips() -> Vec<(&'static str, &'static str)> {
    STEPS
        .iter()
        .flat_map(|s| s.examples.iter().copied())
        .collect()
}

/// The grey footer hint on a fresh install's blank screen. Return
/// starts the tour from there; the hint retires with the marker.
pub const FOOTER_HINT: &str = "Return: take the walkthrough";

/// The grey footer hint while the tour is showing.
pub const TOUR_FOOTER: &str = "Return: next \u{00B7} type anything to exit";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_well_formed() {
        assert!(STEPS.len() >= 5);
        for step in STEPS {
            assert!(!step.title.is_empty());
            assert!(!step.summary.is_empty());
            assert!(!step.examples.is_empty());
            for (q, why) in step.examples {
                assert!(!q.is_empty() && !why.is_empty());
            }
        }
    }

    #[test]
    fn rows_lead_with_the_advance_row_and_end() {
        for i in 0..STEPS.len() {
            let rows = step_rows(i).unwrap();
            assert!(rows[0].2, "row 0 must be the activatable one");
            assert!(rows[1..].iter().all(|r| !r.2));
            if i + 1 == STEPS.len() {
                assert!(rows[0].0.starts_with('\u{2713}'));
            } else {
                assert!(rows[0].0.contains(&format!("{}/{}", i + 1, STEPS.len())));
            }
        }
        assert!(step_rows(STEPS.len()).is_none());
    }

    #[test]
    fn every_row_fits_the_panel_width() {
        // Rows render title plus subtitle on one unwrapped line; at the
        // panel's width and fonts, roughly 62 combined characters fit.
        // This budget is what keeps tour text from clipping.
        const BUDGET: usize = 62;
        for i in 0..STEPS.len() {
            for (title, subtitle, _) in step_rows(i).unwrap() {
                let total = title.chars().count() + subtitle.chars().count();
                assert!(
                    total <= BUDGET,
                    "step {i} row too wide ({total} > {BUDGET}): {title:?} {subtitle:?}"
                );
            }
        }
        for (q, why) in tips() {
            let total = "try: ".len() + q.chars().count() + why.chars().count();
            assert!(total <= BUDGET, "tip too wide: {q:?} {why:?}");
        }
    }

    #[test]
    fn done_marker_round_trips() {
        // Via the path seam, not BECKON_HOME: mutating process env in a
        // parallel test suite races every store_root reader.
        let dir = std::env::temp_dir().join(format!("beckon-walkthrough-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = done_path_in(&dir);
        assert!(!marker.exists());
        std::fs::write(&marker, b"").unwrap();
        assert!(marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
