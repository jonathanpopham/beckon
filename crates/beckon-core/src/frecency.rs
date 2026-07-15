//! Usage-based ranking with integer half-life decay.
//!
//! Each recorded use of an item deposits `SCALE` (1000) millipoints. The
//! deposit decays with a half-life of `HALF_LIFE_SECS` (14 days), so items
//! used often and recently score high, items untouched for months fade to
//! zero and get pruned.
//!
//! The decay model, in pure integer arithmetic (no floats anywhere):
//! whole elapsed half-lives are exact halvings (right shifts); the
//! fractional remainder `r` (0 <= r < H) interpolates linearly between
//! 1.0 and 0.5, i.e. `value * (2H - r) / (2H)`. This is a piecewise linear
//! approximation of exponential decay: exact at every half-life multiple,
//! at most a few percent high in between, monotonically non-increasing,
//! and byte deterministic.
//!
//! `record_use` rebases the stored value to `now_secs` (decays it, adds the
//! deposit, moves the timestamp), so the score is a function of the exact
//! call sequence, which is what golden tests lock. The crate never reads a
//! clock: `now_secs` is always injected by the caller.
//!
//! Serialization is a self-contained line format, canonical by
//! construction (entries sorted by id via `BTreeMap`, one entry per line,
//! version header first):
//!
//! ```text
//! beckon-frecency v1
//! <id>\t<millipoints>\t<last_used_secs>
//! ```
//!
//! Ids are escaped so tabs, newlines, and percent signs round-trip
//! (`%` -> `%25`, tab -> `%09`, newline -> `%0A`).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Seconds in the decay half-life: 14 days.
pub const HALF_LIFE_SECS: u64 = 14 * 24 * 60 * 60;

/// Millipoints deposited per recorded use.
const SCALE: i64 = 1000;

const HEADER: &str = "beckon-frecency v1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Fixed-point value in millipoints, valid as of `last_used`.
    millis: i64,
    /// Injected timestamp (seconds) of the last rebase.
    last_used: u64,
}

/// Frecency scores for item ids. Deterministic: `BTreeMap` keeps
/// iteration and serialization order stable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrecencyStore {
    entries: BTreeMap<String, Entry>,
}

/// Decay `millis` from `last` to `now`. Elapsed time saturates at zero so
/// a clock that appears to run backwards never inflates a score.
fn decayed(millis: i64, last: u64, now: u64) -> i64 {
    let elapsed = now.saturating_sub(last);
    let halvings = elapsed / HALF_LIFE_SECS;
    if halvings >= 63 {
        return 0;
    }
    let v = millis >> (halvings as u32);
    let rem = (elapsed % HALF_LIFE_SECS) as i128;
    let h2 = 2 * HALF_LIFE_SECS as i128;
    ((v as i128) * (h2 - rem) / h2) as i64
}

fn escape(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\n' => out.push_str("%0A"),
            _ => out.push(c),
        }
    }
    out
}

fn unescape(id: &str) -> Option<String> {
    let mut out = String::with_capacity(id.len());
    let mut chars = id.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let a = chars.next()?;
        let b = chars.next()?;
        match (a, b) {
            ('2', '5') => out.push('%'),
            ('0', '9') => out.push('\t'),
            ('0', 'A') => out.push('\n'),
            _ => return None,
        }
    }
    Some(out)
}

/// Errors from [`FrecencyStore::from_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The first line is not the expected version header.
    BadHeader,
    /// A data line does not parse; the payload is the 1-based line number.
    Malformed(usize),
    /// The same id appears twice; canonical files list each id once.
    DuplicateId(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::BadHeader => write!(f, "missing or unknown frecency header"),
            ParseError::Malformed(line) => write!(f, "malformed frecency entry at line {line}"),
            ParseError::DuplicateId(id) => write!(f, "duplicate frecency id {id:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl FrecencyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one use of `id` at the injected time `now_secs`: the stored
    /// value is decayed to now, one deposit is added, and the entry is
    /// rebased.
    pub fn record_use(&mut self, id: &str, now_secs: u64) {
        let e = self.entries.entry(id.to_string()).or_insert(Entry {
            millis: 0,
            last_used: now_secs,
        });
        e.millis = decayed(e.millis, e.last_used, now_secs) + SCALE;
        e.last_used = e.last_used.max(now_secs);
    }

    /// The decayed score of `id` at `now_secs`, in millipoints. Unknown
    /// ids score 0.
    pub fn score(&self, id: &str, now_secs: u64) -> i64 {
        self.entries
            .get(id)
            .map(|e| decayed(e.millis, e.last_used, now_secs))
            .unwrap_or(0)
    }

    /// Drop every entry whose score has decayed to zero at `now_secs`.
    pub fn prune(&mut self, now_secs: u64) {
        self.entries
            .retain(|_, e| decayed(e.millis, e.last_used, now_secs) > 0);
    }

    /// Number of tracked ids.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no ids are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl fmt::Display for FrecencyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{HEADER}")?;
        for (id, e) in &self.entries {
            writeln!(f, "{}\t{}\t{}", escape(id), e.millis, e.last_used)?;
        }
        Ok(())
    }
}

impl FromStr for FrecencyStore {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut lines = s.lines();
        if lines.next() != Some(HEADER) {
            return Err(ParseError::BadHeader);
        }
        let mut entries = BTreeMap::new();
        for (idx, line) in lines.enumerate() {
            // 1-based, counting the header as line 1.
            let line_no = idx + 2;
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [id_raw, millis_raw, last_raw] = fields[..] else {
                return Err(ParseError::Malformed(line_no));
            };
            let id = unescape(id_raw).ok_or(ParseError::Malformed(line_no))?;
            let millis: i64 = millis_raw
                .parse()
                .map_err(|_| ParseError::Malformed(line_no))?;
            let last_used: u64 = last_raw
                .parse()
                .map_err(|_| ParseError::Malformed(line_no))?;
            if entries
                .insert(id.clone(), Entry { millis, last_used })
                .is_some()
            {
                return Err(ParseError::DuplicateId(id));
            }
        }
        Ok(FrecencyStore { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = HALF_LIFE_SECS;

    #[test]
    fn unknown_id_scores_zero() {
        let store = FrecencyStore::new();
        assert_eq!(store.score("nope", 0), 0);
    }

    #[test]
    fn one_use_scores_one_deposit_immediately() {
        let mut store = FrecencyStore::new();
        store.record_use("app.safari", 100);
        assert_eq!(store.score("app.safari", 100), 1000);
    }

    #[test]
    fn one_half_life_halves_the_score() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        assert_eq!(store.score("a", H), 500);
    }

    #[test]
    fn two_half_lives_quarter_the_score() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        assert_eq!(store.score("a", 2 * H), 250);
    }

    // Golden: the linear-interpolation remainder. value * (2H - r) / (2H).
    #[test]
    fn golden_fractional_decay_is_linear_between_halvings() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        assert_eq!(store.score("a", H / 4), 875);
        assert_eq!(store.score("a", H / 2), 750);
        assert_eq!(store.score("a", H + H / 2), 375);
    }

    #[test]
    fn decay_is_monotonically_non_increasing() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        let mut prev = store.score("a", 0);
        for step in 1..40 {
            let now = step * (H / 3);
            let cur = store.score("a", now);
            assert!(cur <= prev, "score rose from {prev} to {cur} at {now}");
            prev = cur;
        }
    }

    #[test]
    fn uses_accumulate() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        store.record_use("a", 0);
        assert_eq!(store.score("a", 0), 2000);
    }

    #[test]
    fn record_rebases_decayed_value_then_deposits() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 0);
        store.record_use("a", H);
        // 1000 decayed to 500, plus a fresh 1000.
        assert_eq!(store.score("a", H), 1500);
    }

    #[test]
    fn clock_running_backwards_never_inflates() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 100);
        assert_eq!(store.score("a", 50), 1000);
    }

    #[test]
    fn prune_drops_fully_decayed_entries_and_keeps_live_ones() {
        let mut store = FrecencyStore::new();
        store.record_use("old", 0);
        store.record_use("fresh", 10 * H);
        // 1000 >> 10 == 0: ten half-lives kill a single deposit.
        assert_eq!(store.score("old", 10 * H), 0);
        store.prune(10 * H);
        assert_eq!(store.len(), 1);
        assert_eq!(store.score("old", 10 * H), 0);
        assert_eq!(store.score("fresh", 10 * H), 1000);
    }

    // Golden: the canonical serialized form, byte for byte.
    #[test]
    fn golden_serialization_is_canonical() {
        let mut store = FrecencyStore::new();
        store.record_use("zed", 50);
        store.record_use("app.safari", 100);
        store.record_use("app.safari", 100);
        let text = store.to_string();
        assert_eq!(
            text,
            "beckon-frecency v1\napp.safari\t2000\t100\nzed\t1000\t50\n"
        );
    }

    #[test]
    fn roundtrip_preserves_the_store() {
        let mut store = FrecencyStore::new();
        store.record_use("a", 1);
        store.record_use("b", 2);
        store.record_use("a", 3);
        let parsed: FrecencyStore = store.to_string().parse().unwrap();
        assert_eq!(parsed, store);
    }

    #[test]
    fn ids_with_tabs_newlines_and_percents_roundtrip() {
        let mut store = FrecencyStore::new();
        store.record_use("weird\tid\nwith %25", 7);
        let parsed: FrecencyStore = store.to_string().parse().unwrap();
        assert_eq!(parsed, store);
        assert_eq!(parsed.score("weird\tid\nwith %25", 7), 1000);
    }

    #[test]
    fn bad_header_is_rejected() {
        assert_eq!(
            "not a header\n".parse::<FrecencyStore>(),
            Err(ParseError::BadHeader)
        );
        assert_eq!("".parse::<FrecencyStore>(), Err(ParseError::BadHeader));
    }

    #[test]
    fn malformed_lines_are_rejected_with_line_numbers() {
        let text = "beckon-frecency v1\ngood\t1000\t1\nbad line\n";
        assert_eq!(text.parse::<FrecencyStore>(), Err(ParseError::Malformed(3)));
        let text = "beckon-frecency v1\nid\tnot_a_number\t1\n";
        assert_eq!(text.parse::<FrecencyStore>(), Err(ParseError::Malformed(2)));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let text = "beckon-frecency v1\na\t1\t1\na\t2\t2\n";
        assert_eq!(
            text.parse::<FrecencyStore>(),
            Err(ParseError::DuplicateId("a".to_string()))
        );
    }
}
