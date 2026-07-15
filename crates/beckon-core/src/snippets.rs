//! Snippet store and placeholder expansion.
//!
//! Pure model, no clock reads and no pasteboard access. The macOS shell
//! detects a typed keyword (or an explicit invocation) and calls
//! [`expand`] with an injected timestamp; everything here is deterministic
//! and testable on Linux.
//!
//! Placeholder grammar (braces are the only metacharacters):
//!   - `{date}`      civil date of `now_secs` as `YYYY-MM-DD`, UTC
//!   - `{time}`      `HH:MM`, 24 hour, UTC
//!   - `{datetime}`  `YYYY-MM-DDTHH:MM:SSZ`, ISO 8601, UTC
//!   - `{clipboard}` the injected clipboard text; empty string when the
//!     context carries `None`
//!   - `{cursor}`    marks the final caret position and is removed from
//!     the output. Only the first `{cursor}` counts; later ones are
//!     stripped without effect
//!   - `{{` and `}}` are escapes for literal `{` and `}`
//!   - any other `{word}` (name matching `[A-Za-z0-9_]+`) passes through
//!     verbatim, braces included, so future placeholder names do not break
//!     old snippet bodies
//!
//! A `{` that cannot open a placeholder (followed by a space, a symbol, or
//! an immediate `}`) is a literal `{`. A `{` that starts a placeholder
//! name but hits end of input is a typed [`ExpandError`], never a panic.
//! A lone `}` is a literal `}`.
//!
//! All times are UTC: the deterministic core cannot know the local offset
//! without a platform call, so local-offset handling is the shell's job
//! later. Date math is proleptic Gregorian, integer only, derived from the
//! injected unix seconds (see `civil_from_unix`); the model never reads a
//! clock.
//!
//! Persistence goes through the canonical `persist` codec, so a saved
//! store is byte-deterministic and survives a crash mid-write.

use crate::persist::{self, StoreError, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Schema version written into saved stores.
const SCHEMA_VERSION: i128 = 1;

/// One keyword-expanded snippet. Timestamps are seconds, injected by the
/// caller; the model never reads a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Monotonic, never reused.
    pub id: u64,
    /// Short trigger the user types, like "addr".
    pub keyword: String,
    /// Human title shown in the launcher.
    pub name: String,
    /// The template text, expanded by [`expand`].
    pub body: String,
    /// When this snippet was created. Built-in defaults use 0.
    pub created: u64,
    /// When this snippet was last expanded; 0 until first use.
    pub last_used: u64,
    /// How many times this snippet has been expanded.
    pub use_count: u64,
}

/// Failure while loading a persisted store.
#[derive(Debug)]
pub enum SnippetLoadError {
    /// The underlying file could not be read or parsed.
    Store(StoreError),
    /// The document parsed but does not have the expected shape.
    Schema(&'static str),
}

impl fmt::Display for SnippetLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnippetLoadError::Store(e) => write!(f, "snippet store: {e}"),
            SnippetLoadError::Schema(what) => write!(f, "snippet store schema: {what}"),
        }
    }
}

impl std::error::Error for SnippetLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnippetLoadError::Store(e) => Some(e),
            SnippetLoadError::Schema(_) => None,
        }
    }
}

impl From<StoreError> for SnippetLoadError {
    fn from(e: StoreError) -> Self {
        SnippetLoadError::Store(e)
    }
}

/// Snippet collection. Entries live in insertion order internally; the
/// exposed ordering is defined by [`SnippetStore::search`]: `use_count`
/// descending, then keyword ascending, then id ascending.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnippetStore {
    snippets: Vec<Snippet>,
    next_id: u64,
}

impl SnippetStore {
    /// Empty store.
    pub fn new() -> Self {
        SnippetStore {
            snippets: Vec::new(),
            next_id: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.snippets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }

    /// All snippets in insertion order (oldest first). For display use
    /// [`SnippetStore::search`] with an empty query instead.
    pub fn snippets(&self) -> &[Snippet] {
        &self.snippets
    }

    pub fn get(&self, id: u64) -> Option<&Snippet> {
        self.snippets.iter().find(|s| s.id == id)
    }

    /// Create a snippet at `now` (seconds) and return its id.
    pub fn add(&mut self, keyword: &str, name: &str, body: &str, now: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.snippets.push(Snippet {
            id,
            keyword: keyword.to_string(),
            name: name.to_string(),
            body: body.to_string(),
            created: now,
            last_used: 0,
            use_count: 0,
        });
        id
    }

    /// Replace keyword, name, and body of an existing snippet, keeping its
    /// usage history. Returns false if the id is unknown.
    pub fn edit(&mut self, id: u64, keyword: &str, name: &str, body: &str) -> bool {
        match self.snippets.iter_mut().find(|s| s.id == id) {
            Some(snippet) => {
                snippet.keyword = keyword.to_string();
                snippet.name = name.to_string();
                snippet.body = body.to_string();
                true
            }
            None => false,
        }
    }

    /// Remove a snippet. Returns false if the id is unknown.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.snippets.len();
        self.snippets.retain(|s| s.id != id);
        self.snippets.len() != before
    }

    /// Record an expansion at `now` (seconds): bumps `use_count` and
    /// `last_used`. Returns false if the id is unknown. A stale call never
    /// moves `last_used` backward.
    pub fn record_use(&mut self, id: u64, now: u64) -> bool {
        match self.snippets.iter_mut().find(|s| s.id == id) {
            Some(snippet) => {
                snippet.use_count = snippet.use_count.saturating_add(1);
                snippet.last_used = snippet.last_used.max(now);
                true
            }
            None => false,
        }
    }

    /// Exact keyword match. If several snippets share a keyword the one
    /// that sorts first (most used, then smallest id) wins.
    pub fn lookup_keyword(&self, keyword: &str) -> Option<&Snippet> {
        self.snippets
            .iter()
            .filter(|s| s.keyword == keyword)
            .min_by(|a, b| b.use_count.cmp(&a.use_count).then(a.id.cmp(&b.id)))
    }

    /// Case-insensitive substring search over keyword and name. An empty
    /// query matches every snippet. Results are ordered most used first,
    /// then keyword ascending, then id ascending: deterministic throughout
    /// because ids are monotonic and never reused.
    pub fn search(&self, query: &str) -> Vec<&Snippet> {
        let needle = query.to_lowercase();
        let mut hits: Vec<&Snippet> = self
            .snippets
            .iter()
            .filter(|s| {
                needle.is_empty()
                    || s.keyword.to_lowercase().contains(&needle)
                    || s.name.to_lowercase().contains(&needle)
            })
            .collect();
        hits.sort_by(|a, b| {
            b.use_count
                .cmp(&a.use_count)
                .then(a.keyword.cmp(&b.keyword))
                .then(a.id.cmp(&b.id))
        });
        hits
    }

    /// Serialize to a canonical [`Value`] tree.
    fn to_value(&self) -> Value {
        let snippets: Vec<Value> = self
            .snippets
            .iter()
            .map(|s| {
                let mut map = BTreeMap::new();
                map.insert("id".to_string(), Value::Int(i128::from(s.id)));
                map.insert("keyword".to_string(), Value::Str(s.keyword.clone()));
                map.insert("name".to_string(), Value::Str(s.name.clone()));
                map.insert("body".to_string(), Value::Str(s.body.clone()));
                map.insert("created".to_string(), Value::Int(i128::from(s.created)));
                map.insert("last_used".to_string(), Value::Int(i128::from(s.last_used)));
                map.insert("use_count".to_string(), Value::Int(i128::from(s.use_count)));
                Value::Object(map)
            })
            .collect();
        let mut root = BTreeMap::new();
        root.insert("version".to_string(), Value::Int(SCHEMA_VERSION));
        root.insert("next_id".to_string(), Value::Int(i128::from(self.next_id)));
        root.insert("snippets".to_string(), Value::Array(snippets));
        Value::Object(root)
    }

    /// Rebuild from a [`Value`] tree, validating shape and ranges.
    fn from_value(value: &Value) -> Result<Self, SnippetLoadError> {
        let version = value
            .get("version")
            .and_then(Value::as_int)
            .ok_or(SnippetLoadError::Schema("version"))?;
        if version != SCHEMA_VERSION {
            return Err(SnippetLoadError::Schema("unsupported version"));
        }
        let next_id = require_u64(value, "next_id")?;
        let snippets_value = value
            .get("snippets")
            .and_then(Value::as_array)
            .ok_or(SnippetLoadError::Schema("missing snippets array"))?;
        let mut snippets = Vec::with_capacity(snippets_value.len());
        let mut max_id = 0u64;
        for item in snippets_value {
            let snippet = Snippet {
                id: require_u64(item, "id")?,
                keyword: require_str(item, "keyword")?,
                name: require_str(item, "name")?,
                body: require_str(item, "body")?,
                created: require_u64(item, "created")?,
                last_used: require_u64(item, "last_used")?,
                use_count: require_u64(item, "use_count")?,
            };
            max_id = max_id.max(snippet.id);
            snippets.push(snippet);
        }
        if next_id <= max_id {
            return Err(SnippetLoadError::Schema("next_id not past max snippet id"));
        }
        Ok(SnippetStore { snippets, next_id })
    }

    /// Save through the canonical codec with an atomic write.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        persist::save_value(path, &self.to_value())
    }

    /// Load a store saved by [`SnippetStore::save`]. Missing file is
    /// `Ok(None)` so first launch needs no special casing.
    pub fn load(path: &Path) -> Result<Option<Self>, SnippetLoadError> {
        match persist::load_value(path)? {
            Some(value) => Ok(Some(Self::from_value(&value)?)),
            None => Ok(None),
        }
    }
}

fn require_u64(value: &Value, key: &'static str) -> Result<u64, SnippetLoadError> {
    let n = value
        .get(key)
        .and_then(Value::as_int)
        .ok_or(SnippetLoadError::Schema(key))?;
    u64::try_from(n).map_err(|_| SnippetLoadError::Schema(key))
}

fn require_str(value: &Value, key: &'static str) -> Result<String, SnippetLoadError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(SnippetLoadError::Schema(key))
}

/// Built-in starter snippets so the feature is discoverable before the
/// user writes any. Deterministic: `created` is 0 (meaning "built-in"),
/// no clock is read.
pub fn defaults() -> SnippetStore {
    let mut store = SnippetStore::new();
    store.add("date", "Today's date", "{date}", 0);
    store.add("time", "Current time (UTC)", "{time}", 0);
    store.add("iso", "ISO datetime (UTC)", "{datetime}", 0);
    store
}

/// Everything [`expand`] needs from the outside world, injected so the
/// core stays clock-free and clipboard-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandContext {
    /// Unix seconds, the sole input to the date and time placeholders.
    pub now_secs: u64,
    /// Current clipboard text; `None` expands `{clipboard}` to "".
    pub clipboard: Option<String>,
}

/// The result of expanding a snippet body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The expanded text with all markers resolved or removed.
    pub text: String,
    /// Byte offset into `text` where the caret should land, from the first
    /// `{cursor}` marker; `None` when the body had no cursor marker.
    pub cursor_offset: Option<usize>,
}

/// Typed expansion failure. Positions are byte offsets into the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    /// A `{` opened a placeholder name that never closed before end of
    /// input, like `"{date"` or a trailing lone `"{"`.
    UnterminatedPlaceholder { pos: usize },
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandError::UnterminatedPlaceholder { pos } => {
                write!(f, "unterminated placeholder at byte offset {pos}")
            }
        }
    }
}

impl std::error::Error for ExpandError {}

/// Is this byte a placeholder-name byte? Names are ASCII words, so byte
/// scanning is UTF-8 safe: multi-byte sequences never contain `{` or `}`.
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Expand `body` per the module-level grammar. Never panics: malformed
/// input either passes through literally or returns a typed error.
pub fn expand(body: &str, ctx: &ExpandContext) -> Result<Expanded, ExpandError> {
    let bytes = body.as_bytes();
    let mut out = String::new();
    let mut cursor_offset: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && is_name_byte(bytes[j]) {
                    j += 1;
                }
                if j > start && bytes.get(j) == Some(&b'}') {
                    match &body[start..j] {
                        "date" => out.push_str(&format_date(ctx.now_secs)),
                        "time" => out.push_str(&format_time(ctx.now_secs)),
                        "datetime" => out.push_str(&format_datetime(ctx.now_secs)),
                        "clipboard" => out.push_str(ctx.clipboard.as_deref().unwrap_or("")),
                        "cursor" => {
                            // First marker wins; later ones are stripped.
                            if cursor_offset.is_none() {
                                cursor_offset = Some(out.len());
                            }
                        }
                        // Unknown names pass through verbatim so future
                        // placeholders do not break old bodies.
                        _ => out.push_str(&body[i..=j]),
                    }
                    i = j + 1;
                } else if j == bytes.len() {
                    // `{` plus zero or more name bytes, then end of input:
                    // this could still have been a placeholder, so it is
                    // unterminated rather than literal.
                    return Err(ExpandError::UnterminatedPlaceholder { pos: i });
                } else {
                    // Definitively not a placeholder (empty name or a
                    // non-name byte before `}`): literal `{`, resume after.
                    out.push('{');
                    i += 1;
                }
            }
            b'}' => {
                // `}}` collapses to one `}`; a lone `}` is literal anyway.
                out.push('}');
                i += if bytes.get(i + 1) == Some(&b'}') {
                    2
                } else {
                    1
                };
            }
            _ => {
                // Copy the run up to the next metacharacter in one slice.
                let next = bytes[i..]
                    .iter()
                    .position(|&b| b == b'{' || b == b'}')
                    .map_or(bytes.len(), |p| i + p);
                out.push_str(&body[i..next]);
                i = next;
            }
        }
    }
    Ok(Expanded {
        text: out,
        cursor_offset,
    })
}

/// Days per era and civil-epoch shift for the proleptic Gregorian
/// calendar. An era is 400 years: 146097 days.
const DAYS_PER_ERA: u64 = 146_097;
/// Days from 0000-03-01 (the civil algorithm's epoch) to 1970-01-01.
const UNIX_EPOCH_SHIFT: u64 = 719_468;

/// Convert unix seconds to a UTC civil timestamp: (year, month, day,
/// hour, minute, second). Proleptic Gregorian, integer only, following
/// the classic days-from-civil derivation. `u64` seconds means nothing
/// before 1970 is representable, which is fine for a launcher.
fn civil_from_unix(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days + UNIX_EPOCH_SHIFT;
    let era = z / DAYS_PER_ERA;
    let doe = z % DAYS_PER_ERA; // day of era, 0..146096
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // 0..399
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, March-based
    let mp = (5 * doy + 2) / 153; // March-based month, 0..11
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day, hour, minute, second)
}

/// `YYYY-MM-DD`, UTC.
fn format_date(secs: u64) -> String {
    let (y, m, d, _, _, _) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM`, 24 hour, UTC.
fn format_time(secs: u64) -> String {
    let (_, _, _, h, min, _) = civil_from_unix(secs);
    format!("{h:02}:{min:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ`, ISO 8601, UTC.
fn format_datetime(secs: u64) -> String {
    let (y, m, d, h, min, s) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh per-test directory under the system temp dir; the real home
    /// is never touched.
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "beckon-snippets-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn ctx(now_secs: u64) -> ExpandContext {
        ExpandContext {
            now_secs,
            clipboard: None,
        }
    }

    fn ctx_clip(now_secs: u64, clip: &str) -> ExpandContext {
        ExpandContext {
            now_secs,
            clipboard: Some(clip.to_string()),
        }
    }

    // ---- civil date math goldens ----

    #[test]
    fn civil_date_golden_timestamps() {
        // (unix seconds, expected date, expected time, expected datetime)
        let cases: &[(u64, &str, &str, &str)] = &[
            (0, "1970-01-01", "00:00", "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01", "23:59", "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02", "00:00", "1970-01-02T00:00:00Z"),
            // Well-known round timestamps.
            (1_000_000_000, "2001-09-09", "01:46", "2001-09-09T01:46:40Z"),
            (1_234_567_890, "2009-02-13", "23:31", "2009-02-13T23:31:30Z"),
            // 2000 is divisible by 400: a leap year despite the century.
            (951_782_400, "2000-02-29", "00:00", "2000-02-29T00:00:00Z"),
            (951_868_800, "2000-03-01", "00:00", "2000-03-01T00:00:00Z"),
            // Ordinary leap year.
            (1_709_164_800, "2024-02-29", "00:00", "2024-02-29T00:00:00Z"),
            // 2100 is divisible by 100 but not 400: no Feb 29.
            (4_107_456_000, "2100-02-28", "00:00", "2100-02-28T00:00:00Z"),
            (4_107_542_400, "2100-03-01", "00:00", "2100-03-01T00:00:00Z"),
            // Last second of a year.
            (1_735_689_599, "2024-12-31", "23:59", "2024-12-31T23:59:59Z"),
            (1_735_689_600, "2025-01-01", "00:00", "2025-01-01T00:00:00Z"),
        ];
        for &(secs, date, time, datetime) in cases {
            assert_eq!(format_date(secs), date, "date of {secs}");
            assert_eq!(format_time(secs), time, "time of {secs}");
            assert_eq!(format_datetime(secs), datetime, "datetime of {secs}");
        }
    }

    #[test]
    fn civil_date_is_consistent_across_a_leap_february() {
        // Walk every day of Feb..Mar 2024 and check month lengths.
        let feb1 = 1_706_745_600u64; // 2024-02-01T00:00:00Z
        for day in 0..29 {
            let (y, m, d, _, _, _) = civil_from_unix(feb1 + day * 86_400);
            assert_eq!((y, m), (2024, 2));
            assert_eq!(d, day + 1);
        }
        let (y, m, d, _, _, _) = civil_from_unix(feb1 + 29 * 86_400);
        assert_eq!((y, m, d), (2024, 3, 1));
    }

    // ---- expansion ----

    #[test]
    fn expand_plain_text_passes_through() {
        let out = expand("no placeholders here", &ctx(0)).expect("expand");
        assert_eq!(out.text, "no placeholders here");
        assert_eq!(out.cursor_offset, None);
    }

    #[test]
    fn expand_empty_body() {
        let out = expand("", &ctx(0)).expect("expand");
        assert_eq!(out.text, "");
        assert_eq!(out.cursor_offset, None);
    }

    #[test]
    fn expand_date_time_datetime() {
        let c = ctx(1_234_567_890);
        assert_eq!(
            expand("today is {date}", &c).expect("expand").text,
            "today is 2009-02-13"
        );
        assert_eq!(expand("{time}", &c).expect("expand").text, "23:31");
        assert_eq!(
            expand("[{datetime}]", &c).expect("expand").text,
            "[2009-02-13T23:31:30Z]"
        );
    }

    #[test]
    fn expand_clipboard_present_and_absent() {
        let with = ctx_clip(0, "pasted ü 🎉");
        assert_eq!(
            expand("<{clipboard}>", &with).expect("expand").text,
            "<pasted ü 🎉>"
        );
        let without = ctx(0);
        assert_eq!(
            expand("<{clipboard}>", &without).expect("expand").text,
            "<>"
        );
    }

    #[test]
    fn expand_cursor_marks_offset_and_is_removed() {
        let out = expand("a{cursor}b", &ctx(0)).expect("expand");
        assert_eq!(out.text, "ab");
        assert_eq!(out.cursor_offset, Some(1));
        // At the very start and very end.
        let out = expand("{cursor}tail", &ctx(0)).expect("expand");
        assert_eq!((out.text.as_str(), out.cursor_offset), ("tail", Some(0)));
        let out = expand("head{cursor}", &ctx(0)).expect("expand");
        assert_eq!((out.text.as_str(), out.cursor_offset), ("head", Some(4)));
    }

    #[test]
    fn expand_only_first_cursor_counts() {
        let out = expand("{cursor}x{cursor}y{cursor}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "xy");
        assert_eq!(out.cursor_offset, Some(0));
    }

    #[test]
    fn expand_cursor_offset_is_bytes_after_multibyte_text() {
        // "ü" is two bytes: the offset is a byte offset, not a char count.
        let out = expand("ü{cursor}z", &ctx(0)).expect("expand");
        assert_eq!(out.text, "üz");
        assert_eq!(out.cursor_offset, Some(2));
    }

    #[test]
    fn expand_brace_escapes() {
        let out = expand("{{date}}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{date}");
        let out = expand("{{{{}}}}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{{}}");
        // Escaped open brace next to a real placeholder.
        let out = expand("{{{date}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{1970-01-01");
    }

    #[test]
    fn expand_unknown_placeholder_passes_through_verbatim() {
        let out = expand("{name} and {UPPER_case_9}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{name} and {UPPER_case_9}");
    }

    #[test]
    fn expand_literal_braces_that_are_not_placeholders() {
        // Empty name, space in name, symbol in name: all literal.
        assert_eq!(expand("{}", &ctx(0)).expect("expand").text, "{}");
        assert_eq!(expand("{ date}", &ctx(0)).expect("expand").text, "{ date}");
        assert_eq!(expand("{a-b}", &ctx(0)).expect("expand").text, "{a-b}");
        // Lone close brace is literal.
        assert_eq!(expand("a}b", &ctx(0)).expect("expand").text, "a}b");
    }

    #[test]
    fn expand_nasty_nesting() {
        // The outer `{a` cannot close, so it is literal; the inner
        // placeholder still expands; the trailing `}` is literal.
        let out = expand("{a{date}}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{a1970-01-01}");
        // Nested cursor inside a broken wrapper still lands.
        let out = expand("{a{cursor}}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{a}");
        assert_eq!(out.cursor_offset, Some(2));
        // Unknown inside unknown.
        let out = expand("{x{y}z}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{x{y}z}");
    }

    #[test]
    fn expand_unterminated_placeholder_is_typed_error() {
        assert_eq!(
            expand("{date", &ctx(0)),
            Err(ExpandError::UnterminatedPlaceholder { pos: 0 })
        );
        assert_eq!(
            expand("abc{", &ctx(0)),
            Err(ExpandError::UnterminatedPlaceholder { pos: 3 })
        );
        assert_eq!(
            expand("x{word_", &ctx(0)),
            Err(ExpandError::UnterminatedPlaceholder { pos: 1 })
        );
        // But a `{` that already failed to be a placeholder is literal
        // even at end of input.
        assert_eq!(expand("{ ", &ctx(0)).expect("expand").text, "{ ");
    }

    #[test]
    fn expand_never_panics_on_hostile_bodies() {
        let hostile = [
            "{",
            "}",
            "{}",
            "}}}}",
            "{{{{",
            "{{}",
            "{}}",
            "{a",
            "a}",
            "{ü}",
            "🎉{date}🎉",
            "{cursor",
            "{date}{",
            "{{cursor}}",
            "{_}",
            "{9}",
        ];
        for body in hostile {
            // Ok or Err both fine; the point is no panic.
            let _ = expand(body, &ctx_clip(1_234_567_890, "clip"));
        }
    }

    #[test]
    fn expand_multibyte_placeholder_name_is_literal() {
        // Non-ASCII bytes are not name bytes, so the brace is literal.
        let out = expand("{ü}", &ctx(0)).expect("expand");
        assert_eq!(out.text, "{ü}");
    }

    // ---- store ----

    #[test]
    fn add_get_edit_remove() {
        let mut store = SnippetStore::new();
        let id = store.add("addr", "Home address", "123 Main St", 100);
        assert_eq!(id, 1);
        let s = store.get(id).expect("snippet");
        assert_eq!(
            (s.keyword.as_str(), s.name.as_str(), s.body.as_str()),
            ("addr", "Home address", "123 Main St")
        );
        assert_eq!((s.created, s.last_used, s.use_count), (100, 0, 0));
        assert!(store.edit(id, "home", "Home", "456 Oak Ave"));
        let s = store.get(id).expect("snippet");
        assert_eq!(s.keyword, "home");
        assert_eq!(s.body, "456 Oak Ave");
        assert_eq!(s.created, 100, "edit keeps history");
        assert!(store.remove(id));
        assert!(store.get(id).is_none());
        // Unknown ids report false, not panic.
        assert!(!store.edit(999, "x", "x", "x"));
        assert!(!store.remove(999));
        assert!(!store.record_use(999, 1));
    }

    #[test]
    fn record_use_bumps_count_and_recency() {
        let mut store = SnippetStore::new();
        let id = store.add("sig", "Signature", "-- J", 10);
        assert!(store.record_use(id, 50));
        assert!(store.record_use(id, 40)); // stale: never moves backward
        let s = store.get(id).expect("snippet");
        assert_eq!(s.use_count, 2);
        assert_eq!(s.last_used, 50);
    }

    #[test]
    fn lookup_keyword_is_exact() {
        let mut store = SnippetStore::new();
        store.add("addr", "Address", "x", 1);
        store.add("address", "Longer", "y", 2);
        assert_eq!(store.lookup_keyword("addr").expect("hit").name, "Address");
        assert!(store.lookup_keyword("add").is_none());
        assert!(store.lookup_keyword("ADDR").is_none(), "keyword is exact");
    }

    #[test]
    fn lookup_keyword_prefers_most_used_then_smallest_id() {
        let mut store = SnippetStore::new();
        let a = store.add("dup", "First", "x", 1);
        let b = store.add("dup", "Second", "y", 2);
        assert_eq!(store.lookup_keyword("dup").expect("hit").id, a);
        store.record_use(b, 10);
        assert_eq!(store.lookup_keyword("dup").expect("hit").id, b);
    }

    #[test]
    fn search_matches_keyword_and_name_case_insensitive() {
        let mut store = SnippetStore::new();
        store.add("addr", "Home Address", "x", 1);
        store.add("sig", "Email signature", "y", 2);
        store.add("misc", "Other", "z", 3);
        let hits: Vec<&str> = store
            .search("ADDR")
            .iter()
            .map(|s| s.keyword.as_str())
            .collect();
        assert_eq!(hits, vec!["addr"]);
        let hits: Vec<&str> = store
            .search("signature")
            .iter()
            .map(|s| s.keyword.as_str())
            .collect();
        assert_eq!(hits, vec!["sig"]);
        assert!(store.search("zzz").is_empty());
    }

    #[test]
    fn search_orders_by_use_count_then_keyword_then_id() {
        let mut store = SnippetStore::new();
        let b = store.add("beta", "B", "x", 1);
        store.add("alpha", "A", "y", 2);
        store.add("gamma", "G", "z", 3);
        // No usage yet: pure keyword order.
        let kws: Vec<&str> = store
            .search("")
            .iter()
            .map(|s| s.keyword.as_str())
            .collect();
        assert_eq!(kws, vec!["alpha", "beta", "gamma"]);
        // Usage dominates keyword order.
        store.record_use(b, 10);
        let kws: Vec<&str> = store
            .search("")
            .iter()
            .map(|s| s.keyword.as_str())
            .collect();
        assert_eq!(kws, vec!["beta", "alpha", "gamma"]);
        // Same use_count and keyword: smaller id first.
        let mut tie = SnippetStore::new();
        let t1 = tie.add("same", "One", "x", 1);
        let t2 = tie.add("same", "Two", "y", 2);
        let ids: Vec<u64> = tie.search("same").iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![t1, t2]);
    }

    #[test]
    fn search_is_deterministic() {
        let mut store = SnippetStore::new();
        for i in 0..20u64 {
            store.add(&format!("kw{}", i % 5), &format!("name {i}"), "b", i);
        }
        let first: Vec<u64> = store.search("kw").iter().map(|s| s.id).collect();
        let second: Vec<u64> = store.search("kw").iter().map(|s| s.id).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn defaults_are_present_and_expandable() {
        let store = defaults();
        assert!(store.len() >= 2);
        let date = store.lookup_keyword("date").expect("date snippet");
        let out = expand(&date.body, &ctx(1_234_567_890)).expect("expand");
        assert_eq!(out.text, "2009-02-13");
        let iso = store.lookup_keyword("iso").expect("iso snippet");
        let out = expand(&iso.body, &ctx(1_234_567_890)).expect("expand");
        assert_eq!(out.text, "2009-02-13T23:31:30Z");
        // Every default body expands cleanly.
        for s in store.snippets() {
            expand(&s.body, &ctx(0)).expect("default body expands");
        }
    }

    // ---- persistence ----

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let path = dir.join("snippets.json");
        let mut store = SnippetStore::new();
        store.add("addr", "Address ü 🎉", "line one\nline two", 100);
        let id = store.add("sig", "Signature", "-- J{cursor}", 200);
        store.record_use(id, 300);
        store.save(&path).expect("save");
        let loaded = SnippetStore::load(&path).expect("load").expect("present");
        assert_eq!(loaded, store);
        // ids keep advancing correctly after a reload.
        let mut loaded = loaded;
        let next = loaded.add("new", "New", "n", 400);
        assert_eq!(next, 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_is_byte_deterministic() {
        let dir = temp_dir();
        let path_a = dir.join("a.json");
        let path_b = dir.join("b.json");
        let mut store = SnippetStore::new();
        store.add("x", "X", "body x", 1);
        store.add("y", "Y", "body y", 2);
        store.save(&path_a).expect("save a");
        store.save(&path_b).expect("save b");
        assert_eq!(
            fs::read(&path_a).expect("read a"),
            fs::read(&path_b).expect("read b")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_none() {
        let dir = temp_dir();
        assert!(SnippetStore::load(&dir.join("absent.json"))
            .expect("load")
            .is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_malformed_documents() {
        let dir = temp_dir();
        let path = dir.join("bad.json");
        // Not an object.
        persist::write_atomic(&path, b"[1,2,3]").expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Schema(_))
        ));
        // Wrong version.
        persist::write_atomic(&path, b"{\"next_id\":1,\"snippets\":[],\"version\":99}")
            .expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Schema("unsupported version"))
        ));
        // next_id colliding with an existing snippet id.
        persist::write_atomic(
            &path,
            b"{\"next_id\":5,\"snippets\":[{\"body\":\"b\",\"created\":1,\"id\":5,\"keyword\":\"k\",\"last_used\":0,\"name\":\"n\",\"use_count\":0}],\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Schema(_))
        ));
        // Negative timestamp.
        persist::write_atomic(
            &path,
            b"{\"next_id\":2,\"snippets\":[{\"body\":\"b\",\"created\":-1,\"id\":1,\"keyword\":\"k\",\"last_used\":0,\"name\":\"n\",\"use_count\":0}],\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Schema(_))
        ));
        // Missing field.
        persist::write_atomic(
            &path,
            b"{\"next_id\":2,\"snippets\":[{\"body\":\"b\",\"id\":1}],\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Schema(_))
        ));
        // Unparseable JSON bubbles up as a store error.
        persist::write_atomic(&path, b"{nope").expect("write");
        assert!(matches!(
            SnippetStore::load(&path),
            Err(SnippetLoadError::Store(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }
}
