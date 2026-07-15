//! Parameterized links: URL templates with a `{query}` placeholder.
//!
//! Pure model. [`fill`] only builds strings; the macOS shell opens the
//! resulting URL later. Nothing here touches the network.
//!
//! Template grammar (braces are the only metacharacters):
//!   - `{query}` is replaced by the percent-encoded user query, at every
//!     occurrence
//!   - `{{` and `}}` are escapes for literal `{` and `}` (outside a
//!     placeholder; inside one, a single `}` closes it first)
//!   - any other `{word}` passes through verbatim (forward compatibility)
//!   - a template without `{query}` fills to itself unchanged
//!
//! Encoding is RFC 3986 percent-encoding over UTF-8 bytes: the unreserved
//! set (ALPHA / DIGIT / `-` / `.` / `_` / `~`) passes through, every other
//! byte becomes `%XX` with uppercase hex. Spaces are `%20`, never `+`.
//!
//! Persistence goes through the canonical `persist` codec, so a saved
//! store is byte-deterministic and survives a crash mid-write.

use crate::persist::{self, StoreError, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Schema version written into saved stores.
const SCHEMA_VERSION: i128 = 1;

/// One parameterized link. Timestamps are seconds, injected by the
/// caller; the model never reads a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quicklink {
    /// Monotonic, never reused.
    pub id: u64,
    /// Human name shown in the launcher, like "Google Search".
    pub name: String,
    /// URL template, like `https://www.google.com/search?q={query}`.
    pub template: String,
    /// When this link was created. Built-in defaults use 0.
    pub created: u64,
    /// How many times this link has been opened.
    pub use_count: u64,
}

/// Failure while loading a persisted store.
#[derive(Debug)]
pub enum QuicklinkLoadError {
    /// The underlying file could not be read or parsed.
    Store(StoreError),
    /// The document parsed but does not have the expected shape.
    Schema(&'static str),
}

impl fmt::Display for QuicklinkLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuicklinkLoadError::Store(e) => write!(f, "quicklink store: {e}"),
            QuicklinkLoadError::Schema(what) => write!(f, "quicklink store schema: {what}"),
        }
    }
}

impl std::error::Error for QuicklinkLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QuicklinkLoadError::Store(e) => Some(e),
            QuicklinkLoadError::Schema(_) => None,
        }
    }
}

impl From<StoreError> for QuicklinkLoadError {
    fn from(e: StoreError) -> Self {
        QuicklinkLoadError::Store(e)
    }
}

/// Quicklink collection. Entries live in insertion order internally; the
/// exposed ordering is defined by [`QuicklinkStore::search`]: `use_count`
/// descending, then name ascending, then id ascending.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuicklinkStore {
    links: Vec<Quicklink>,
    next_id: u64,
}

impl QuicklinkStore {
    /// Empty store.
    pub fn new() -> Self {
        QuicklinkStore {
            links: Vec::new(),
            next_id: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// All links in insertion order (oldest first). For display use
    /// [`QuicklinkStore::search`] with an empty query instead.
    pub fn links(&self) -> &[Quicklink] {
        &self.links
    }

    pub fn get(&self, id: u64) -> Option<&Quicklink> {
        self.links.iter().find(|l| l.id == id)
    }

    /// Create a link at `now` (seconds) and return its id. The template
    /// is stored as given; call [`validate`] first if it came from user
    /// input.
    pub fn add(&mut self, name: &str, template: &str, now: u64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.links.push(Quicklink {
            id,
            name: name.to_string(),
            template: template.to_string(),
            created: now,
            use_count: 0,
        });
        id
    }

    /// Replace name and template of an existing link, keeping its usage
    /// history. Returns false if the id is unknown.
    pub fn edit(&mut self, id: u64, name: &str, template: &str) -> bool {
        match self.links.iter_mut().find(|l| l.id == id) {
            Some(link) => {
                link.name = name.to_string();
                link.template = template.to_string();
                true
            }
            None => false,
        }
    }

    /// Remove a link. Returns false if the id is unknown.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.links.len();
        self.links.retain(|l| l.id != id);
        self.links.len() != before
    }

    /// Record an open: bumps `use_count`. Returns false if the id is
    /// unknown.
    pub fn record_use(&mut self, id: u64) -> bool {
        match self.links.iter_mut().find(|l| l.id == id) {
            Some(link) => {
                link.use_count = link.use_count.saturating_add(1);
                true
            }
            None => false,
        }
    }

    /// Case-insensitive substring search over the name. An empty query
    /// matches every link. Results are ordered most used first, then name
    /// ascending, then id ascending: deterministic throughout because ids
    /// are monotonic and never reused.
    pub fn search(&self, query: &str) -> Vec<&Quicklink> {
        let needle = query.to_lowercase();
        let mut hits: Vec<&Quicklink> = self
            .links
            .iter()
            .filter(|l| needle.is_empty() || l.name.to_lowercase().contains(&needle))
            .collect();
        hits.sort_by(|a, b| {
            b.use_count
                .cmp(&a.use_count)
                .then(a.name.cmp(&b.name))
                .then(a.id.cmp(&b.id))
        });
        hits
    }

    /// Serialize to a canonical [`Value`] tree.
    fn to_value(&self) -> Value {
        let links: Vec<Value> = self
            .links
            .iter()
            .map(|l| {
                let mut map = BTreeMap::new();
                map.insert("id".to_string(), Value::Int(i128::from(l.id)));
                map.insert("name".to_string(), Value::Str(l.name.clone()));
                map.insert("template".to_string(), Value::Str(l.template.clone()));
                map.insert("created".to_string(), Value::Int(i128::from(l.created)));
                map.insert("use_count".to_string(), Value::Int(i128::from(l.use_count)));
                Value::Object(map)
            })
            .collect();
        let mut root = BTreeMap::new();
        root.insert("version".to_string(), Value::Int(SCHEMA_VERSION));
        root.insert("next_id".to_string(), Value::Int(i128::from(self.next_id)));
        root.insert("links".to_string(), Value::Array(links));
        Value::Object(root)
    }

    /// Rebuild from a [`Value`] tree, validating shape and ranges.
    fn from_value(value: &Value) -> Result<Self, QuicklinkLoadError> {
        let version = value
            .get("version")
            .and_then(Value::as_int)
            .ok_or(QuicklinkLoadError::Schema("version"))?;
        if version != SCHEMA_VERSION {
            return Err(QuicklinkLoadError::Schema("unsupported version"));
        }
        let next_id = require_u64(value, "next_id")?;
        let links_value = value
            .get("links")
            .and_then(Value::as_array)
            .ok_or(QuicklinkLoadError::Schema("missing links array"))?;
        let mut links = Vec::with_capacity(links_value.len());
        let mut max_id = 0u64;
        for item in links_value {
            let link = Quicklink {
                id: require_u64(item, "id")?,
                name: require_str(item, "name")?,
                template: require_str(item, "template")?,
                created: require_u64(item, "created")?,
                use_count: require_u64(item, "use_count")?,
            };
            max_id = max_id.max(link.id);
            links.push(link);
        }
        if next_id <= max_id {
            return Err(QuicklinkLoadError::Schema("next_id not past max link id"));
        }
        Ok(QuicklinkStore { links, next_id })
    }

    /// Save through the canonical codec with an atomic write.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        persist::save_value(path, &self.to_value())
    }

    /// Load a store saved by [`QuicklinkStore::save`]. Missing file is
    /// `Ok(None)` so first launch needs no special casing.
    pub fn load(path: &Path) -> Result<Option<Self>, QuicklinkLoadError> {
        match persist::load_value(path)? {
            Some(value) => Ok(Some(Self::from_value(&value)?)),
            None => Ok(None),
        }
    }
}

fn require_u64(value: &Value, key: &'static str) -> Result<u64, QuicklinkLoadError> {
    let n = value
        .get(key)
        .and_then(Value::as_int)
        .ok_or(QuicklinkLoadError::Schema(key))?;
    u64::try_from(n).map_err(|_| QuicklinkLoadError::Schema(key))
}

fn require_str(value: &Value, key: &'static str) -> Result<String, QuicklinkLoadError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(QuicklinkLoadError::Schema(key))
}

/// Built-in starter links users expect. Deterministic: `created` is 0
/// (meaning "built-in"), no clock is read. Every template here passes
/// [`validate`], enforced by test.
pub fn defaults() -> QuicklinkStore {
    let mut store = QuicklinkStore::new();
    store.add(
        "Google Search",
        "https://www.google.com/search?q={query}",
        0,
    );
    store.add(
        "Wikipedia",
        "https://en.wikipedia.org/wiki/Special:Search?search={query}",
        0,
    );
    store.add(
        "GitHub Repositories",
        "https://github.com/search?q={query}&type=repositories",
        0,
    );
    store.add(
        "YouTube",
        "https://www.youtube.com/results?search_query={query}",
        0,
    );
    store.add("Apple Maps", "https://maps.apple.com/?q={query}", 0);
    store
}

/// RFC 3986 percent-encoding, byte-wise over UTF-8. The unreserved set
/// (ALPHA / DIGIT / `-` / `.` / `_` / `~`) passes through; every other
/// byte becomes `%XX` with uppercase hex. Spaces are `%20`, never `+`.
pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0x0f)] as char);
            }
        }
    }
    out
}

/// Substitute every `{query}` in `template` with the percent-encoded
/// `query`, honoring `{{` and `}}` escapes. Anything else, including
/// unknown `{word}` placeholders and stray braces, passes through
/// verbatim; a template without `{query}` returns unchanged. Never fails:
/// [`validate`] is the place for rejecting malformed templates.
pub fn fill(template: &str, query: &str) -> String {
    let encoded = percent_encode(query);
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len() + encoded.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    out.push('{');
                    i += 2;
                } else if template[i..].starts_with("{query}") {
                    out.push_str(&encoded);
                    i += "{query}".len();
                } else {
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
                // Safe on UTF-8: multi-byte sequences never contain braces.
                let next = bytes[i..]
                    .iter()
                    .position(|&b| b == b'{' || b == b'}')
                    .map_or(bytes.len(), |p| i + p);
                out.push_str(&template[i..next]);
                i = next;
            }
        }
    }
    out
}

/// Typed template validation failure. Positions are byte offsets into the
/// template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuicklinkError {
    /// The template does not start with `http://` or `https://`.
    MissingScheme,
    /// Nothing follows the scheme prefix.
    EmptyAfterScheme,
    /// A whitespace or control byte, which a URL may never contain raw.
    Whitespace { pos: usize },
    /// A brace without its partner: an unclosed `{`, a stray `}`, or a
    /// `{` nested inside an open placeholder.
    UnbalancedBraces { pos: usize },
}

impl fmt::Display for QuicklinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuicklinkError::MissingScheme => {
                write!(f, "template must start with http:// or https://")
            }
            QuicklinkError::EmptyAfterScheme => {
                write!(f, "template has nothing after the scheme")
            }
            QuicklinkError::Whitespace { pos } => {
                write!(f, "whitespace or control byte at offset {pos}")
            }
            QuicklinkError::UnbalancedBraces { pos } => {
                write!(f, "unbalanced brace at offset {pos}")
            }
        }
    }
}

impl std::error::Error for QuicklinkError {}

/// Check that a template is http/https-shaped: a lowercase `http://` or
/// `https://` prefix with a non-empty remainder, no raw whitespace or
/// control bytes anywhere, and balanced braces. Braces are checked with
/// the same escape rules [`fill`] uses: `{{` and `}}` are literals outside
/// a placeholder, a single `}` closes an open `{` first, and placeholders
/// do not nest.
pub fn validate(template: &str) -> Result<(), QuicklinkError> {
    let rest = template
        .strip_prefix("https://")
        .or_else(|| template.strip_prefix("http://"))
        .ok_or(QuicklinkError::MissingScheme)?;
    if rest.is_empty() {
        return Err(QuicklinkError::EmptyAfterScheme);
    }
    let bytes = template.as_bytes();
    for (pos, &b) in bytes.iter().enumerate() {
        if b <= 0x20 || b == 0x7f {
            return Err(QuicklinkError::Whitespace { pos });
        }
    }
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => match open {
                // A `{` inside an open placeholder never balances.
                Some(_) => return Err(QuicklinkError::UnbalancedBraces { pos: i }),
                None => {
                    if bytes.get(i + 1) == Some(&b'{') {
                        i += 2; // literal escape
                    } else {
                        open = Some(i);
                        i += 1;
                    }
                }
            },
            b'}' => match open {
                // Inside a placeholder a single `}` closes it first.
                Some(_) => {
                    open = None;
                    i += 1;
                }
                None => {
                    if bytes.get(i + 1) == Some(&b'}') {
                        i += 2; // literal escape
                    } else {
                        return Err(QuicklinkError::UnbalancedBraces { pos: i });
                    }
                }
            },
            _ => i += 1,
        }
    }
    match open {
        Some(pos) => Err(QuicklinkError::UnbalancedBraces { pos }),
        None => Ok(()),
    }
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
            "beckon-quicklinks-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    // ---- percent encoding ----

    #[test]
    fn percent_encode_goldens() {
        // Unreserved set passes through untouched.
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(percent_encode(""), "");
        // The classics.
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("#fragment"), "%23fragment");
        assert_eq!(percent_encode("a+b"), "a%2Bb");
        assert_eq!(percent_encode("50%"), "50%25");
        assert_eq!(percent_encode("path/to?x"), "path%2Fto%3Fx");
        // UTF-8 goes byte-wise with uppercase hex.
        assert_eq!(percent_encode("ü"), "%C3%BC");
        assert_eq!(percent_encode("日本"), "%E6%97%A5%E6%9C%AC");
        assert_eq!(percent_encode("🎉"), "%F0%9F%8E%89");
        // Space is %20, never +.
        assert!(!percent_encode("a b").contains('+'));
    }

    // ---- fill ----

    #[test]
    fn fill_substitutes_and_encodes() {
        assert_eq!(
            fill("https://www.google.com/search?q={query}", "hello world"),
            "https://www.google.com/search?q=hello%20world"
        );
        assert_eq!(
            fill("https://x.test/?q={query}", "a&b=c#d"),
            "https://x.test/?q=a%26b%3Dc%23d"
        );
        assert_eq!(
            fill("https://x.test/?q={query}", "日本 🎉"),
            "https://x.test/?q=%E6%97%A5%E6%9C%AC%20%F0%9F%8E%89"
        );
    }

    #[test]
    fn fill_substitutes_every_occurrence() {
        assert_eq!(
            fill("https://x.test/{query}/compare/{query}", "a b"),
            "https://x.test/a%20b/compare/a%20b"
        );
    }

    #[test]
    fn fill_without_placeholder_returns_unchanged() {
        let t = "https://example.com/fixed/path";
        assert_eq!(fill(t, "ignored query"), t);
    }

    #[test]
    fn fill_honors_brace_escapes() {
        // `{{query}}` is a literal "{query}", not a substitution.
        assert_eq!(
            fill("https://x.test/{{query}}", "z"),
            "https://x.test/{query}"
        );
        assert_eq!(fill("{{}}", "z"), "{}");
    }

    #[test]
    fn fill_passes_unknown_placeholders_and_stray_braces() {
        assert_eq!(
            fill("https://x.test/{other}", "z"),
            "https://x.test/{other}"
        );
        assert_eq!(fill("a{b", "z"), "a{b");
        assert_eq!(fill("a}b", "z"), "a}b");
        assert_eq!(fill("{", "z"), "{");
        assert_eq!(fill("{quer", "z"), "{quer");
        // `{query` without the close brace is not the placeholder.
        assert_eq!(fill("x{query", "z"), "x{query");
    }

    #[test]
    fn fill_empty_query_substitutes_empty() {
        assert_eq!(fill("https://x.test/?q={query}", ""), "https://x.test/?q=");
    }

    // ---- validate ----

    #[test]
    fn validate_accepts_http_and_https_shapes() {
        assert_eq!(validate("https://www.google.com/search?q={query}"), Ok(()));
        assert_eq!(validate("http://localhost:8080/{query}"), Ok(()));
        assert_eq!(validate("https://x.test/no/placeholder"), Ok(()));
        // Escaped braces are fine.
        assert_eq!(validate("https://x.test/{{literal}}"), Ok(()));
        for link in defaults().links() {
            assert_eq!(validate(&link.template), Ok(()), "default {}", link.name);
        }
    }

    #[test]
    fn validate_rejects_missing_scheme() {
        for t in ["www.google.com", "ftp://x.test", "HTTPS://x.test", ""] {
            assert_eq!(validate(t), Err(QuicklinkError::MissingScheme), "{t:?}");
        }
    }

    #[test]
    fn validate_rejects_empty_remainder() {
        assert_eq!(validate("https://"), Err(QuicklinkError::EmptyAfterScheme));
        assert_eq!(validate("http://"), Err(QuicklinkError::EmptyAfterScheme));
    }

    #[test]
    fn validate_rejects_whitespace_and_control_bytes() {
        assert_eq!(
            validate("https://x.test/a b"),
            Err(QuicklinkError::Whitespace { pos: 16 })
        );
        assert!(matches!(
            validate("https://x.test/\ta"),
            Err(QuicklinkError::Whitespace { .. })
        ));
        assert!(matches!(
            validate("https://x.test/a\n"),
            Err(QuicklinkError::Whitespace { .. })
        ));
        assert!(matches!(
            validate("https://x.test/a\u{7f}"),
            Err(QuicklinkError::Whitespace { .. })
        ));
    }

    #[test]
    fn validate_rejects_unbalanced_braces() {
        assert_eq!(
            validate("https://x.test/{query"),
            Err(QuicklinkError::UnbalancedBraces { pos: 15 })
        );
        assert_eq!(
            validate("https://x.test/query}"),
            Err(QuicklinkError::UnbalancedBraces { pos: 20 })
        );
        // A `{` inside an open placeholder never balances.
        assert_eq!(
            validate("https://x.test/{a{b}}"),
            Err(QuicklinkError::UnbalancedBraces { pos: 17 })
        );
        // A close-then-stray-close: the first `}` closes, the second has
        // no partner and no pair to escape with.
        assert_eq!(
            validate("https://x.test/{q}}"),
            Err(QuicklinkError::UnbalancedBraces { pos: 18 })
        );
    }

    #[test]
    fn validate_never_panics_on_hostile_templates() {
        let hostile = [
            "https://x.test/{",
            "https://x.test/}",
            "https://x.test/{{{",
            "https://x.test/}}}",
            "https://x.test/{}{}{}",
            "https://x.test/ü{query}",
        ];
        for t in hostile {
            let _ = validate(t); // Ok or Err both fine; no panic.
        }
    }

    // ---- defaults ----

    #[test]
    fn defaults_cover_the_expected_services() {
        let store = defaults();
        let names: Vec<&str> = store.links().iter().map(|l| l.name.as_str()).collect();
        for expected in [
            "Google Search",
            "Wikipedia",
            "GitHub Repositories",
            "YouTube",
            "Apple Maps",
        ] {
            assert!(names.contains(&expected), "missing default {expected}");
        }
        // And they fill sensibly.
        let google = &store.links()[0];
        assert_eq!(
            fill(&google.template, "rust launcher"),
            "https://www.google.com/search?q=rust%20launcher"
        );
    }

    // ---- store ----

    #[test]
    fn add_get_edit_remove() {
        let mut store = QuicklinkStore::new();
        let id = store.add("Docs", "https://docs.test/{query}", 100);
        assert_eq!(id, 1);
        let l = store.get(id).expect("link");
        assert_eq!(l.name, "Docs");
        assert_eq!((l.created, l.use_count), (100, 0));
        assert!(store.edit(id, "Docs v2", "https://docs.test/v2/{query}"));
        let l = store.get(id).expect("link");
        assert_eq!(l.name, "Docs v2");
        assert_eq!(l.created, 100, "edit keeps history");
        assert!(store.remove(id));
        assert!(store.get(id).is_none());
        // Unknown ids report false, not panic.
        assert!(!store.edit(999, "x", "x"));
        assert!(!store.remove(999));
        assert!(!store.record_use(999));
    }

    #[test]
    fn search_matches_name_case_insensitive_and_orders() {
        let mut store = QuicklinkStore::new();
        store.add("Zebra Search", "https://z.test/{query}", 1);
        let g = store.add("Google Search", "https://g.test/{query}", 2);
        store.add("Maps", "https://m.test/{query}", 3);
        let names: Vec<&str> = store
            .search("SEARCH")
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        // No usage yet: name order.
        assert_eq!(names, vec!["Google Search", "Zebra Search"]);
        // Usage dominates name order: Zebra pulls ahead of Google.
        let z = 1;
        store.record_use(z);
        assert!(store.get(g).is_some());
        let names: Vec<&str> = store
            .search("search")
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["Zebra Search", "Google Search"]);
        // Empty query returns everything.
        assert_eq!(store.search("").len(), 3);
        assert!(store.search("zzz").is_empty());
    }

    #[test]
    fn search_ties_break_by_smallest_id() {
        let mut store = QuicklinkStore::new();
        let a = store.add("Same", "https://a.test/", 1);
        let b = store.add("Same", "https://b.test/", 2);
        let ids: Vec<u64> = store.search("same").iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    // ---- persistence ----

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let path = dir.join("quicklinks.json");
        let mut store = QuicklinkStore::new();
        store.add("Google", "https://www.google.com/search?q={query}", 100);
        let id = store.add("Unicode ü 🎉", "https://x.test/{query}", 200);
        store.record_use(id);
        store.save(&path).expect("save");
        let loaded = QuicklinkStore::load(&path).expect("load").expect("present");
        assert_eq!(loaded, store);
        // ids keep advancing correctly after a reload.
        let mut loaded = loaded;
        let next = loaded.add("New", "https://n.test/{query}", 300);
        assert_eq!(next, 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_is_byte_deterministic() {
        let dir = temp_dir();
        let path_a = dir.join("a.json");
        let path_b = dir.join("b.json");
        let mut store = QuicklinkStore::new();
        store.add("X", "https://x.test/{query}", 1);
        store.add("Y", "https://y.test/{query}", 2);
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
        assert!(QuicklinkStore::load(&dir.join("absent.json"))
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
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Schema(_))
        ));
        // Wrong version.
        persist::write_atomic(&path, b"{\"links\":[],\"next_id\":1,\"version\":99}")
            .expect("write");
        assert!(matches!(
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Schema("unsupported version"))
        ));
        // next_id colliding with an existing link id.
        persist::write_atomic(
            &path,
            b"{\"links\":[{\"created\":1,\"id\":5,\"name\":\"n\",\"template\":\"https://x.test/\",\"use_count\":0}],\"next_id\":5,\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Schema(_))
        ));
        // Negative counter.
        persist::write_atomic(
            &path,
            b"{\"links\":[{\"created\":1,\"id\":1,\"name\":\"n\",\"template\":\"https://x.test/\",\"use_count\":-1}],\"next_id\":2,\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Schema(_))
        ));
        // Missing field.
        persist::write_atomic(
            &path,
            b"{\"links\":[{\"id\":1,\"name\":\"n\"}],\"next_id\":2,\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Schema(_))
        ));
        // Unparseable JSON bubbles up as a store error.
        persist::write_atomic(&path, b"{nope").expect("write");
        assert!(matches!(
            QuicklinkStore::load(&path),
            Err(QuicklinkLoadError::Store(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }
}
