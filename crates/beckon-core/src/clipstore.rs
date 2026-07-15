//! Clipboard history model: dedupe, pin, search, evict, persist.
//!
//! Pure model, no clock reads and no pasteboard access. The macOS shell
//! watches NSPasteboard and calls [`ClipStore::add`] with an injected
//! timestamp; everything here is deterministic and testable on Linux.
//!
//! Behavior contract:
//!   - Dedupe by content: re-copying known text bumps `last_copied` and
//!     `copy_count` on the existing entry instead of storing a duplicate.
//!     Identity is fnv1a64(text) plus a full text compare, so a hash
//!     collision can never merge two different clips.
//!   - Capacity (default 1000, minimum 1) is a hard cap. Eviction removes
//!     the oldest unpinned entry first (smallest `last_copied`, then
//!     smallest id); only when everything else is pinned does the oldest
//!     pinned entry go. The entry just added is never the victim: the
//!     newest copy is the one the user wants.
//!   - Search is case-insensitive substring match. Results are pinned
//!     entries first, then unpinned, each group newest first
//!     (`last_copied` descending, id descending as the tiebreak).
//!     Deterministic ordering throughout: ids are monotonic and never
//!     reused, so no two entries ever compare equal.
//!   - Persistence goes through the canonical `persist` codec, so a saved
//!     store is byte-deterministic and survives a crash mid-write.

use crate::persist::{self, StoreError, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Default maximum number of entries a store retains.
pub const DEFAULT_CAPACITY: usize = 1000;

/// Schema version written into saved stores.
const SCHEMA_VERSION: i128 = 1;

/// FNV-1a 64-bit hash. Inline so the crate stays dependency-free; used
/// for cheap content identity, always backed by a full text compare.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// One remembered clipboard item. Timestamps are seconds, injected by the
/// caller; the model never reads a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry {
    /// Monotonic, never reused.
    pub id: u64,
    /// fnv1a64 of `text`.
    pub hash: u64,
    pub text: String,
    /// When this text was first seen.
    pub first_copied: u64,
    /// When this text was most recently copied.
    pub last_copied: u64,
    /// How many times this text has been copied.
    pub copy_count: u64,
    /// Pinned entries survive eviction and sort first in search.
    pub pinned: bool,
}

/// Failure while loading a persisted store.
#[derive(Debug)]
pub enum ClipLoadError {
    /// The underlying file could not be read or parsed.
    Store(StoreError),
    /// The document parsed but does not have the expected shape.
    Schema(&'static str),
}

impl fmt::Display for ClipLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipLoadError::Store(e) => write!(f, "clip store: {e}"),
            ClipLoadError::Schema(what) => write!(f, "clip store schema: {what}"),
        }
    }
}

impl std::error::Error for ClipLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClipLoadError::Store(e) => Some(e),
            ClipLoadError::Schema(_) => None,
        }
    }
}

impl From<StoreError> for ClipLoadError {
    fn from(e: StoreError) -> Self {
        ClipLoadError::Store(e)
    }
}

/// Clipboard history. Entries live in insertion order internally; all
/// exposed orderings are defined by [`ClipStore::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipStore {
    entries: Vec<ClipEntry>,
    next_id: u64,
    capacity: usize,
}

impl Default for ClipStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipStore {
    /// Empty store with [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Empty store holding at most `capacity` entries (clamped to 1).
    pub fn with_capacity(capacity: usize) -> Self {
        ClipStore {
            entries: Vec::new(),
            next_id: 1,
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// All entries in insertion order (oldest first). For display use
    /// [`ClipStore::search`] with an empty query instead.
    pub fn entries(&self) -> &[ClipEntry] {
        &self.entries
    }

    pub fn get(&self, id: u64) -> Option<&ClipEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Record a copy observed at `now` (seconds). If the exact text is
    /// already present its recency and count are bumped and the existing
    /// id returned; otherwise a new entry is created, evicting per the
    /// capacity policy. Returns the entry id either way.
    pub fn add(&mut self, text: &str, now: u64) -> u64 {
        let hash = fnv1a64(text.as_bytes());
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.hash == hash && e.text == text)
        {
            existing.last_copied = existing.last_copied.max(now);
            existing.copy_count = existing.copy_count.saturating_add(1);
            return existing.id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(ClipEntry {
            id,
            hash,
            text: text.to_string(),
            first_copied: now,
            last_copied: now,
            copy_count: 1,
            pinned: false,
        });
        self.evict_to_capacity(id);
        id
    }

    /// Oldest unpinned first (smallest `last_copied`, then smallest id);
    /// oldest pinned only when nothing else is unpinned. The entry with
    /// id `protected` (the one just added) is never evicted, so the cap
    /// falls on older material. The cap itself is hard.
    fn evict_to_capacity(&mut self, protected: u64) {
        while self.entries.len() > self.capacity {
            let victim = self
                .entries
                .iter()
                .filter(|e| !e.pinned && e.id != protected)
                .map(|e| (e.last_copied, e.id))
                .min()
                .or_else(|| {
                    self.entries
                        .iter()
                        .filter(|e| e.id != protected)
                        .map(|e| (e.last_copied, e.id))
                        .min()
                });
            match victim {
                Some((_, id)) => self.entries.retain(|e| e.id != id),
                None => return,
            }
        }
    }

    /// Pin an entry. Returns false if the id is unknown.
    pub fn pin(&mut self, id: u64) -> bool {
        self.set_pinned(id, true)
    }

    /// Unpin an entry. Returns false if the id is unknown.
    pub fn unpin(&mut self, id: u64) -> bool {
        self.set_pinned(id, false)
    }

    fn set_pinned(&mut self, id: u64, pinned: bool) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                entry.pinned = pinned;
                true
            }
            None => false,
        }
    }

    /// Remove an entry. Returns false if the id is unknown.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Case-insensitive substring search. An empty query matches every
    /// entry. Pinned entries come first, then unpinned; within each group
    /// newest first (`last_copied` descending, id descending).
    pub fn search(&self, query: &str) -> Vec<&ClipEntry> {
        let needle = query.to_lowercase();
        let mut hits: Vec<&ClipEntry> = self
            .entries
            .iter()
            .filter(|e| needle.is_empty() || e.text.to_lowercase().contains(&needle))
            .collect();
        hits.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.last_copied.cmp(&a.last_copied))
                .then(b.id.cmp(&a.id))
        });
        hits
    }

    /// Serialize to a canonical [`Value`] tree.
    fn to_value(&self) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|e| {
                let mut map = BTreeMap::new();
                map.insert("id".to_string(), Value::Int(i128::from(e.id)));
                map.insert("hash".to_string(), Value::Int(i128::from(e.hash)));
                map.insert("text".to_string(), Value::Str(e.text.clone()));
                map.insert(
                    "first_copied".to_string(),
                    Value::Int(i128::from(e.first_copied)),
                );
                map.insert(
                    "last_copied".to_string(),
                    Value::Int(i128::from(e.last_copied)),
                );
                map.insert(
                    "copy_count".to_string(),
                    Value::Int(i128::from(e.copy_count)),
                );
                map.insert("pinned".to_string(), Value::Bool(e.pinned));
                Value::Object(map)
            })
            .collect();
        let mut root = BTreeMap::new();
        root.insert("version".to_string(), Value::Int(SCHEMA_VERSION));
        root.insert("next_id".to_string(), Value::Int(i128::from(self.next_id)));
        root.insert("capacity".to_string(), Value::Int(self.capacity as i128));
        root.insert("entries".to_string(), Value::Array(entries));
        Value::Object(root)
    }

    /// Rebuild from a [`Value`] tree, validating shape and ranges.
    fn from_value(value: &Value) -> Result<Self, ClipLoadError> {
        let version = require_int(value, "version")?;
        if version != SCHEMA_VERSION {
            return Err(ClipLoadError::Schema("unsupported version"));
        }
        let next_id = require_u64(value, "next_id")?;
        let capacity_raw = require_int(value, "capacity")?;
        if capacity_raw < 1 || capacity_raw > (usize::MAX as i128) {
            return Err(ClipLoadError::Schema("capacity out of range"));
        }
        let entries_value = value
            .get("entries")
            .and_then(Value::as_array)
            .ok_or(ClipLoadError::Schema("missing entries array"))?;
        let mut entries = Vec::with_capacity(entries_value.len());
        let mut max_id = 0u64;
        for item in entries_value {
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .ok_or(ClipLoadError::Schema("entry missing text"))?
                .to_string();
            let entry = ClipEntry {
                id: require_u64(item, "id")?,
                hash: require_u64(item, "hash")?,
                first_copied: require_u64(item, "first_copied")?,
                last_copied: require_u64(item, "last_copied")?,
                copy_count: require_u64(item, "copy_count")?,
                pinned: item
                    .get("pinned")
                    .and_then(Value::as_bool)
                    .ok_or(ClipLoadError::Schema("entry missing pinned"))?,
                text,
            };
            max_id = max_id.max(entry.id);
            entries.push(entry);
        }
        if next_id <= max_id {
            return Err(ClipLoadError::Schema("next_id not past max entry id"));
        }
        Ok(ClipStore {
            entries,
            next_id,
            capacity: capacity_raw as usize,
        })
    }

    /// Save through the canonical codec with an atomic write.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        persist::save_value(path, &self.to_value())
    }

    /// Load a store saved by [`ClipStore::save`]. Missing file is
    /// `Ok(None)` so first launch needs no special casing.
    pub fn load(path: &Path) -> Result<Option<Self>, ClipLoadError> {
        match persist::load_value(path)? {
            Some(value) => Ok(Some(Self::from_value(&value)?)),
            None => Ok(None),
        }
    }
}

fn require_int(value: &Value, key: &'static str) -> Result<i128, ClipLoadError> {
    value
        .get(key)
        .and_then(Value::as_int)
        .ok_or(ClipLoadError::Schema(key))
}

fn require_u64(value: &Value, key: &'static str) -> Result<u64, ClipLoadError> {
    let n = require_int(value, key)?;
    u64::try_from(n).map_err(|_| ClipLoadError::Schema(key))
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
            "beckon-clipstore-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn fnv1a64_known_vectors() {
        // Published FNV-1a test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn add_assigns_monotonic_ids_and_timestamps() {
        let mut store = ClipStore::new();
        let a = store.add("alpha", 100);
        let b = store.add("beta", 200);
        assert_eq!((a, b), (1, 2));
        let entry = store.get(a).expect("entry");
        assert_eq!(entry.text, "alpha");
        assert_eq!(entry.first_copied, 100);
        assert_eq!(entry.last_copied, 100);
        assert_eq!(entry.copy_count, 1);
        assert_eq!(entry.hash, fnv1a64(b"alpha"));
        assert!(!entry.pinned);
    }

    #[test]
    fn add_dedupes_by_content() {
        let mut store = ClipStore::new();
        let first = store.add("same text", 100);
        let second = store.add("same text", 250);
        assert_eq!(first, second);
        assert_eq!(store.len(), 1);
        let entry = store.get(first).expect("entry");
        assert_eq!(entry.first_copied, 100);
        assert_eq!(entry.last_copied, 250);
        assert_eq!(entry.copy_count, 2);
        // A stale re-copy never moves recency backward.
        store.add("same text", 50);
        assert_eq!(store.get(first).expect("entry").last_copied, 250);
        assert_eq!(store.get(first).expect("entry").copy_count, 3);
    }

    #[test]
    fn pin_unpin_remove() {
        let mut store = ClipStore::new();
        let id = store.add("clip", 1);
        assert!(store.pin(id));
        assert!(store.get(id).expect("entry").pinned);
        assert!(store.unpin(id));
        assert!(!store.get(id).expect("entry").pinned);
        assert!(store.remove(id));
        assert!(store.get(id).is_none());
        // Unknown ids report false, not panic.
        assert!(!store.pin(999));
        assert!(!store.unpin(999));
        assert!(!store.remove(999));
    }

    #[test]
    fn eviction_removes_oldest_unpinned_first() {
        let mut store = ClipStore::with_capacity(3);
        let a = store.add("a", 10);
        let b = store.add("b", 20);
        let c = store.add("c", 30);
        // Pin the oldest: it must survive.
        assert!(store.pin(a));
        let d = store.add("d", 40);
        assert_eq!(store.len(), 3);
        assert!(store.get(a).is_some(), "pinned oldest survives");
        assert!(store.get(b).is_none(), "oldest unpinned evicted");
        assert!(store.get(c).is_some());
        assert!(store.get(d).is_some());
    }

    #[test]
    fn eviction_falls_back_to_oldest_pinned_when_all_pinned() {
        let mut store = ClipStore::with_capacity(2);
        let a = store.add("a", 10);
        let b = store.add("b", 20);
        assert!(store.pin(a));
        assert!(store.pin(b));
        let c = store.add("c", 30);
        // Cap is hard: with everything pinned the oldest pinned goes.
        assert_eq!(store.len(), 2);
        assert!(store.get(a).is_none());
        assert!(store.get(b).is_some());
        assert!(store.get(c).is_some());
    }

    #[test]
    fn eviction_ties_break_by_smallest_id() {
        let mut store = ClipStore::with_capacity(2);
        let a = store.add("a", 10);
        let b = store.add("b", 10);
        store.add("c", 10);
        assert!(
            store.get(a).is_none(),
            "same timestamp: smallest id evicted"
        );
        assert!(store.get(b).is_some());
    }

    #[test]
    fn dedupe_does_not_evict() {
        let mut store = ClipStore::with_capacity(2);
        store.add("a", 10);
        store.add("b", 20);
        // Re-copy of existing content must not trigger eviction.
        store.add("a", 30);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn capacity_is_clamped_to_one() {
        let mut store = ClipStore::with_capacity(0);
        assert_eq!(store.capacity(), 1);
        store.add("a", 1);
        store.add("b", 2);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        let mut store = ClipStore::new();
        store.add("Hello World", 10);
        store.add("goodbye world", 20);
        store.add("unrelated", 30);
        let hits = store.search("WORLD");
        let texts: Vec<&str> = hits.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["goodbye world", "Hello World"]);
        assert!(store.search("zzz").is_empty());
    }

    #[test]
    fn search_orders_pinned_then_recency_then_id() {
        let mut store = ClipStore::new();
        let old_pinned = store.add("apple pie", 10);
        store.add("apple juice", 20);
        let newest = store.add("apple cider", 30);
        store.pin(old_pinned);
        let hits = store.search("apple");
        let ids: Vec<u64> = hits.iter().map(|e| e.id).collect();
        // Pinned first despite being oldest, then unpinned newest first.
        assert_eq!(ids, vec![old_pinned, newest, 2]);
        // Same last_copied: larger id (later insertion) wins.
        let mut tie = ClipStore::new();
        let t1 = tie.add("tie one", 50);
        let t2 = tie.add("tie two", 50);
        let ids: Vec<u64> = tie.search("tie").iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![t2, t1]);
    }

    #[test]
    fn empty_query_returns_everything_ordered() {
        let mut store = ClipStore::new();
        store.add("one", 10);
        let two = store.add("two", 20);
        let three = store.add("three", 30);
        store.pin(two);
        let ids: Vec<u64> = store.search("").iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![two, three, 1]);
    }

    #[test]
    fn search_is_deterministic() {
        let mut store = ClipStore::new();
        for i in 0..20u64 {
            store.add(&format!("item {i}"), 100 + (i % 3));
        }
        let first: Vec<u64> = store.search("item").iter().map(|e| e.id).collect();
        let second: Vec<u64> = store.search("item").iter().map(|e| e.id).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let path = dir.join("clips.json");
        let mut store = ClipStore::with_capacity(5);
        store.add("first clip", 100);
        let id = store.add("second clip with unicode ü 🎉", 200);
        store.add("first clip", 300);
        store.pin(id);
        store.save(&path).expect("save");
        let loaded = ClipStore::load(&path).expect("load").expect("present");
        assert_eq!(loaded, store);
        // ids keep advancing correctly after a reload.
        let mut loaded = loaded;
        let next = loaded.add("third clip", 400);
        assert_eq!(next, 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_is_byte_deterministic() {
        let dir = temp_dir();
        let path_a = dir.join("a.json");
        let path_b = dir.join("b.json");
        let mut store = ClipStore::new();
        store.add("x", 1);
        store.add("y", 2);
        store.save(&path_a).expect("save a");
        store.save(&path_b).expect("save b");
        let a = fs::read(&path_a).expect("read a");
        let b = fs::read(&path_b).expect("read b");
        assert_eq!(a, b);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_none() {
        let dir = temp_dir();
        assert!(ClipStore::load(&dir.join("absent.json"))
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
            ClipStore::load(&path),
            Err(ClipLoadError::Schema(_))
        ));
        // Wrong version.
        persist::write_atomic(
            &path,
            b"{\"capacity\":10,\"entries\":[],\"next_id\":1,\"version\":99}",
        )
        .expect("write");
        assert!(matches!(
            ClipStore::load(&path),
            Err(ClipLoadError::Schema("unsupported version"))
        ));
        // next_id colliding with an existing entry id.
        persist::write_atomic(
            &path,
            b"{\"capacity\":10,\"entries\":[{\"copy_count\":1,\"first_copied\":1,\"hash\":0,\"id\":5,\"last_copied\":1,\"pinned\":false,\"text\":\"x\"}],\"next_id\":5,\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            ClipStore::load(&path),
            Err(ClipLoadError::Schema(_))
        ));
        // Negative timestamp.
        persist::write_atomic(
            &path,
            b"{\"capacity\":10,\"entries\":[{\"copy_count\":1,\"first_copied\":-1,\"hash\":0,\"id\":1,\"last_copied\":1,\"pinned\":false,\"text\":\"x\"}],\"next_id\":2,\"version\":1}",
        )
        .expect("write");
        assert!(matches!(
            ClipStore::load(&path),
            Err(ClipLoadError::Schema(_))
        ));
        // Unparseable JSON bubbles up as a store error.
        persist::write_atomic(&path, b"{nope").expect("write");
        assert!(matches!(
            ClipStore::load(&path),
            Err(ClipLoadError::Store(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_uses_full_u64_range_through_persistence() {
        let dir = temp_dir();
        let path = dir.join("hash.json");
        let mut store = ClipStore::new();
        // "a" hashes above i64::MAX, exercising the u64-to-i128 path.
        store.add("a", 1);
        assert!(store.entries()[0].hash > u64::from(u32::MAX));
        store.save(&path).expect("save");
        let loaded = ClipStore::load(&path).expect("load").expect("present");
        assert_eq!(loaded.entries()[0].hash, fnv1a64(b"a"));
        fs::remove_dir_all(&dir).ok();
    }
}
