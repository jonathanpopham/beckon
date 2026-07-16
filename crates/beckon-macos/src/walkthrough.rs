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
        summary: "type to search, Return to launch; it learns your habits",
        examples: &[
            ("saf", "fuzzy matches Safari; launching teaches the ranking"),
            ("terminal", "every app, /Applications and /System alike"),
        ],
    },
    Step {
        title: "Drive your windows",
        summary: "window actions and a switcher, no mouse involved",
        examples: &[
            ("window left half", "halves, thirds, quarters, displays"),
            ("win", "every open window of every app; Return focuses it"),
            ("menu export", "search the frontmost app's entire menu bar"),
        ],
    },
    Step {
        title: "Your clipboard remembers",
        summary: "everything you copy, searchable; secrets are never captured",
        examples: &[
            ("clip", "recent copies; Return pastes into the app you left"),
            ("clip invoice", "search inside your clipboard history"),
        ],
    },
    Step {
        title: "Stop hunting files",
        summary: "paste any path or walk the tree from the keyboard",
        examples: &[
            ("~/", "browse home; Return drills into folders"),
            (
                "~/Downloads/report.pdf",
                "Open, Reveal, Quick Look, Copy Path",
            ),
            ("file report", "fuzzy file search with Spotlight backfill"),
        ],
    },
    Step {
        title: "It does math properly",
        summary: "fixed-point arithmetic, units, bases, and dev utilities",
        examples: &[
            ("0.1 + 0.2", "exactly 0.3; Return copies the answer"),
            ("5 km in mi", "units, temperatures, data sizes, bases"),
            ("uuid", "plus b64, sha256, json, epoch, count"),
        ],
    },
    Step {
        title: "The deep cuts",
        summary: "emoji, snippets, quicklinks, scripts, plugins",
        examples: &[
            ("emoji fire", "Return pastes the glyph where you were"),
            ("snip", "snippets with {date}, {clipboard}, {cursor}"),
            ("go google anything", "parameterized quicklinks"),
        ],
    },
    Step {
        title: "Make it yours",
        summary: "plain files in ~/.beckon; the config file is the truth",
        examples: &[
            ("~/.beckon/config.json", "aliases, hotkey, theme, triggers"),
            ("~/.beckon/scripts", "annotated executables become commands"),
            ("hello", "the bootstrapped example script is already there"),
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
        format!("\u{2713} {} (Return to finish)", step.title)
    } else {
        format!(
            "\u{25B8} Step {}/{}: {} (Return to continue)",
            index + 1,
            STEPS.len(),
            step.title
        )
    };
    let mut rows = vec![(lead, step.summary.to_string(), true)];
    for (query, why) in step.examples {
        rows.push((format!("try: {query}"), (*why).to_string(), false));
    }
    Some(rows)
}

/// The blank-screen invitation row (title, subtitle).
pub fn invite_row() -> (String, String) {
    (
        "\u{25B8} Take the walkthrough".to_string(),
        format!(
            "{} steps, one Return each; type anything to skip",
            STEPS.len()
        ),
    )
}

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
                assert!(rows[0]
                    .0
                    .contains(&format!("Step {}/{}", i + 1, STEPS.len())));
            }
        }
        assert!(step_rows(STEPS.len()).is_none());
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
