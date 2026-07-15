//! Query parsing, the command item model, and ranked merge.
//!
//! Three jobs live here:
//!
//! 1. [`QueryIntent::parse`] decides what the raw input wants: nothing
//!    (`Empty`), arithmetic (`Calc`), or a search over items (`Search`).
//!    The calculator heuristic is deliberately shallow and does not depend
//!    on the calc module: input starting with a digit, `(`, `-`, or `.`
//!    leans Calc, as does input containing both a digit and one of the
//!    operators `+ * % ^`. `/` and mid-word `-` are excluded from the
//!    contains check so paths and names like "spider-man" stay searches.
//!    The calculator itself is the final judge of what actually evaluates.
//! 2. [`Item`] and [`ItemKind`] are the registry currency every provider
//!    (apps, files, clipboard, snippets, quicklinks, scripts, system
//!    commands) speaks.
//! 3. [`rank`] merges fuzzy quality with learned usage into one ordered
//!    list. The integer weighting, locked by golden tests:
//!
//!    ```text
//!    combined = fuzzy_score * FUZZY_WEIGHT + min(frecency_millis, FRECENCY_BOOST_CAP)
//!    ```
//!
//!    with `FUZZY_WEIGHT = 100` and `FRECENCY_BOOST_CAP = 4000`. One fuzzy
//!    point is worth 100 combined points; the frecency boost is the raw
//!    millipoint score (1000 per fresh use, 14-day half-life) capped at
//!    4000. So learned usage can bridge a fuzzy deficit of at most 40
//!    points: enough for a habitual item to win a close race, never enough
//!    to bury a strong match under a weak one. Fuzzy stays dominant.
//!
//! Ordering is fully deterministic: combined score descending, then title
//! ascending, then id ascending. Items whose title does not match the
//! query at all are excluded. The empty query matches everything with
//! fuzzy 0, which makes the empty panel a pure frecency list: recent
//! habits first, then alphabetical.

use crate::frecency::FrecencyStore;
use crate::fuzzy;

/// One combined-score point per hundredth of a fuzzy point.
const FUZZY_WEIGHT: i64 = 100;

/// Cap on the frecency boost, in millipoints (40 fuzzy points).
const FRECENCY_BOOST_CAP: i64 = 4000;

/// What the raw query line is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryIntent {
    /// Fuzzy-search the item registry.
    Search(String),
    /// Hand the expression to the calculator.
    Calc(String),
    /// Nothing typed (or only whitespace).
    Empty,
}

impl QueryIntent {
    /// Classify `raw`. Whitespace is trimmed first; the trimmed text is
    /// what `Search` and `Calc` carry.
    pub fn parse(raw: &str) -> QueryIntent {
        let q = raw.trim();
        let Some(first) = q.chars().next() else {
            return QueryIntent::Empty;
        };
        if first.is_ascii_digit() || matches!(first, '(' | '-' | '.') {
            return QueryIntent::Calc(q.to_string());
        }
        let has_digit = q.chars().any(|c| c.is_ascii_digit());
        let has_operator = q.chars().any(|c| matches!(c, '+' | '*' | '%' | '^'));
        if has_digit && has_operator {
            return QueryIntent::Calc(q.to_string());
        }
        QueryIntent::Search(q.to_string())
    }
}

/// The kind of thing an [`Item`] launches or inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// An installed application.
    App,
    /// A built-in system command (sleep, lock, empty trash, ...).
    SystemCommand,
    /// A file or directory from the file index.
    File,
    /// An entry from clipboard history.
    ClipboardEntry,
    /// A keyword-expanded snippet.
    Snippet,
    /// A parameterized URL or app link.
    Quicklink,
    /// A user script from `~/.beckon/scripts/`.
    Script,
    /// An open window of a running application (the window switcher).
    Window,
}

/// One searchable, launchable entry in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Stable unique id; also the frecency key.
    pub id: String,
    /// What the user sees and what fuzzy matching runs against.
    pub title: String,
    /// Secondary line (path, URL, preview) shown under the title.
    pub subtitle: String,
    /// What kind of thing this is.
    pub kind: ItemKind,
}

impl Item {
    /// Convenience constructor.
    pub fn new(id: &str, title: &str, subtitle: &str, kind: ItemKind) -> Item {
        Item {
            id: id.to_string(),
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            kind,
        }
    }
}

/// One ranked result: the item plus every score component, so the UI can
/// show highlights and debug overlays without recomputing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// The matched item (cloned; providers keep ownership of their lists).
    pub item: Item,
    /// The combined score results are ordered by.
    pub score: i64,
    /// The raw fuzzy component.
    pub fuzzy_score: i64,
    /// The capped frecency component, in millipoints.
    pub frecency_boost: i64,
    /// Matched char indices in the title, for highlighting.
    pub positions: Vec<usize>,
}

/// Fuzzy-match `query` against every item title, blend in frecency, and
/// return results ordered best first. Non-matching items are dropped.
/// Ties break by title, then id, so the output is a pure function of the
/// inputs.
pub fn rank(query: &str, items: &[Item], frecency: &FrecencyStore, now_secs: u64) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = Vec::new();
    for item in items {
        let Some(m) = fuzzy::score(query, &item.title) else {
            continue;
        };
        let boost = frecency.score(&item.id, now_secs).min(FRECENCY_BOOST_CAP);
        out.push(Ranked {
            score: m.score * FUZZY_WEIGHT + boost,
            fuzzy_score: m.score,
            frecency_boost: boost,
            positions: m.positions,
            item: item.clone(),
        });
    }
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.item.title.cmp(&b.item.title))
            .then_with(|| a.item.id.cmp(&b.item.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> Vec<Item> {
        vec![
            Item::new(
                "app.chrome",
                "Google Chrome",
                "/Applications",
                ItemKind::App,
            ),
            Item::new(
                "app.charmap",
                "Character Map",
                "/Applications",
                ItemKind::App,
            ),
            Item::new("app.safari", "Safari", "/Applications", ItemKind::App),
        ]
    }

    fn titles(ranked: &[Ranked]) -> Vec<&str> {
        ranked.iter().map(|r| r.item.title.as_str()).collect()
    }

    #[test]
    fn parse_empty_and_whitespace() {
        assert_eq!(QueryIntent::parse(""), QueryIntent::Empty);
        assert_eq!(QueryIntent::parse("   "), QueryIntent::Empty);
    }

    #[test]
    fn parse_calc_leaning_inputs() {
        assert_eq!(
            QueryIntent::parse("2+2"),
            QueryIntent::Calc("2+2".to_string())
        );
        assert_eq!(
            QueryIntent::parse("(3*4)+1"),
            QueryIntent::Calc("(3*4)+1".to_string())
        );
        assert_eq!(
            QueryIntent::parse("-5 + 3"),
            QueryIntent::Calc("-5 + 3".to_string())
        );
        assert_eq!(
            QueryIntent::parse(".5 * 8"),
            QueryIntent::Calc(".5 * 8".to_string())
        );
        assert_eq!(
            QueryIntent::parse("  42  "),
            QueryIntent::Calc("42".to_string())
        );
        // Operator plus digit, even without a leading digit.
        assert_eq!(
            QueryIntent::parse("x * 2"),
            QueryIntent::Calc("x * 2".to_string())
        );
    }

    #[test]
    fn parse_search_leaning_inputs() {
        assert_eq!(
            QueryIntent::parse("chrome"),
            QueryIntent::Search("chrome".to_string())
        );
        // Hyphenated names are not arithmetic.
        assert_eq!(
            QueryIntent::parse("spider-man"),
            QueryIntent::Search("spider-man".to_string())
        );
        // A digit alone is not enough; needs an operator too.
        assert_eq!(
            QueryIntent::parse("base64"),
            QueryIntent::Search("base64".to_string())
        );
        // Slashes are paths, not division.
        assert_eq!(
            QueryIntent::parse("docs/2026"),
            QueryIntent::Search("docs/2026".to_string())
        );
    }

    #[test]
    fn rank_excludes_non_matching_items() {
        let store = FrecencyStore::new();
        let ranked = rank("zzz", &apps(), &store, 0);
        assert!(ranked.is_empty());
    }

    #[test]
    fn rank_orders_by_fuzzy_when_no_usage() {
        let store = FrecencyStore::new();
        let ranked = rank("chr", &apps(), &store, 0);
        assert_eq!(titles(&ranked), vec!["Google Chrome", "Character Map"]);
    }

    // Golden: a frequently used lower-fuzzy item beats a never-used
    // higher-fuzzy item, and the exact combined scores are locked.
    #[test]
    fn golden_habitual_item_wins_a_close_race() {
        let mut store = FrecencyStore::new();
        let now = 1_000_000;
        store.record_use("app.charmap", now);
        store.record_use("app.charmap", now);
        store.record_use("app.charmap", now);
        let ranked = rank("chr", &apps(), &store, now);
        assert_eq!(titles(&ranked), vec!["Character Map", "Google Chrome"]);
        // fuzzy 101 * 100 + 3000 boost.
        assert_eq!(ranked[0].score, 13100);
        assert_eq!(ranked[0].frecency_boost, 3000);
        // fuzzy 119 * 100 + no boost.
        assert_eq!(ranked[1].score, 11900);
        assert_eq!(ranked[1].frecency_boost, 0);
    }

    // Golden: one use is not enough to flip this race; fuzzy holds.
    #[test]
    fn golden_single_use_does_not_flip_a_clear_fuzzy_lead() {
        let mut store = FrecencyStore::new();
        let now = 1_000_000;
        store.record_use("app.charmap", now);
        let ranked = rank("chr", &apps(), &store, now);
        assert_eq!(titles(&ranked), vec!["Google Chrome", "Character Map"]);
        assert_eq!(ranked[0].score, 11900);
        assert_eq!(ranked[1].score, 11100);
    }

    // Golden: fuzzy dominates for strong matches. The boost cap means no
    // amount of usage bridges a 40-point fuzzy gap.
    #[test]
    fn golden_fuzzy_dominates_heavy_usage_on_strong_matches() {
        let mut items = apps();
        items.push(Item::new(
            "file.sfa",
            "Save File Archive",
            "~/Documents",
            ItemKind::File,
        ));
        let mut store = FrecencyStore::new();
        let now = 1_000_000;
        for _ in 0..100 {
            store.record_use("file.sfa", now);
        }
        let ranked = rank("safari", &items, &store, now);
        assert_eq!(titles(&ranked)[0], "Safari");
        // The boost saturated at the cap and still lost.
        assert_eq!(ranked[1].frecency_boost, FRECENCY_BOOST_CAP);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn empty_query_is_a_pure_frecency_list() {
        let mut store = FrecencyStore::new();
        let now = 500;
        store.record_use("app.safari", now);
        let ranked = rank("", &apps(), &store, now);
        // Used item first, the rest alphabetical by title.
        assert_eq!(
            titles(&ranked),
            vec!["Safari", "Character Map", "Google Chrome"]
        );
        assert_eq!(ranked[0].fuzzy_score, 0);
    }

    #[test]
    fn ties_break_by_title_then_id() {
        let items = vec![
            Item::new("id.b", "Notes", "", ItemKind::App),
            Item::new("id.a", "Notes", "", ItemKind::App),
            Item::new("id.c", "Anvil", "", ItemKind::App),
        ];
        let store = FrecencyStore::new();
        let ranked = rank("", &items, &store, 0);
        // All score 0: Anvil by title, then the two Notes by id.
        assert_eq!(titles(&ranked), vec!["Anvil", "Notes", "Notes"]);
        assert_eq!(ranked[1].item.id, "id.a");
        assert_eq!(ranked[2].item.id, "id.b");
    }

    #[test]
    fn frecency_boost_decays_with_time() {
        let mut store = FrecencyStore::new();
        store.record_use("app.charmap", 0);
        store.record_use("app.charmap", 0);
        store.record_use("app.charmap", 0);
        // Fresh usage wins the close race now (13100 vs 11900) but decays
        // below the gap after two half-lives (3000 -> 750).
        let fresh = rank("chr", &apps(), &store, 0);
        assert_eq!(titles(&fresh)[0], "Character Map");
        let stale = rank("chr", &apps(), &store, 2 * crate::frecency::HALF_LIFE_SECS);
        assert_eq!(titles(&stale)[0], "Google Chrome");
    }

    #[test]
    fn positions_flow_through_for_highlighting() {
        let store = FrecencyStore::new();
        let ranked = rank("chr", &apps(), &store, 0);
        assert_eq!(ranked[0].positions, vec![7, 8, 9]);
    }

    #[test]
    fn rank_is_deterministic() {
        let mut store = FrecencyStore::new();
        store.record_use("app.safari", 10);
        let a = rank("a", &apps(), &store, 10);
        let b = rank("a", &apps(), &store, 10);
        assert_eq!(a, b);
    }
}
