//! The launcher config file model. The file is the truth: everything a
//! preferences window will ever show is a view over this document, stored
//! at `store_root()/config.json` (see [`CONFIG_FILE`]) through the
//! canonical `persist` codec.
//!
//! Document shape (standard JSON, no floats; every section and every key
//! is optional, with the defaults noted):
//!
//! ```json
//! {
//!   "aliases": { "lh": "window.left-half" },
//!   "hotkey": { "key": "space", "modifiers": ["opt"] },
//!   "max_results": 9,
//!   "theme": {
//!     "background": "#1C1C21",
//!     "foreground": "#FFFFFF",
//!     "accent": "#5AC8FA",
//!     "font_size": 22
//!   },
//!   "triggers": { "v": "clip" }
//! }
//! ```
//!
//! Semantics:
//!   - `aliases`: alias word -> command/item id. The word is a single
//!     token (non-empty, no whitespace); the target id must be non-empty.
//!     Default: empty.
//!   - `hotkey`: the summon chord. `key` is a single letter or digit, or
//!     a named key (`space`, `tab`, `return`, `f1`..`f12`), resolved via
//!     [`keycode_for`]; `modifiers` is a non-empty list drawn from `cmd`,
//!     `opt`, `ctrl`, `shift` (case-insensitive, deduplicated). Default:
//!     Option+Space.
//!   - `theme`: `#RRGGBB` hex colors plus a font size clamped to 9..=32.
//!     Defaults match the built-in panel styling.
//!   - `max_results`: rows shown per query, clamped to 1..=100. Default 9.
//!   - `triggers`: keyword renames for the built-in sources. Entries are
//!     merged over the identity defaults (`clip`, `win`, `file`, `menu`,
//!     `emoji`, `snip`, `go`) plus the built-in synonyms ([`TRIGGER_SYNONYMS`]:
//!     `clipboard`, `windows`, `find`, `snippet`), so `"v": "clip"` adds a
//!     synonym without losing `clip` itself. Targets must name one of the
//!     seven sources.
//!
//! There is no schema version field: the document is user-authored, every
//! key is optional, and unknown keys are ignored for forward compatibility.
//! Parsing never panics; every failure is a typed [`ConfigError`].

use crate::persist::{self, StoreError, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// The config file name under the beckon store root. The shell loads
/// `persist::store_root().join(CONFIG_FILE)`.
pub const CONFIG_FILE: &str = "config.json";

/// The seven built-in keyword-triggered sources a `triggers` entry may
/// point at.
pub const TRIGGER_SOURCES: [&str; 7] = ["clip", "emoji", "file", "go", "menu", "snip", "win"];

/// Built-in synonym keywords merged into the default trigger table next
/// to the identity entries, so the long spellings keep working without
/// any engine-side special cases. Users may repoint them like any other
/// trigger keyword.
pub const TRIGGER_SYNONYMS: [(&str, &str); 4] = [
    ("clipboard", "clip"),
    ("find", "file"),
    ("snippet", "snip"),
    ("windows", "win"),
];

/// Modifier names the `hotkey.modifiers` list accepts.
pub const MODIFIER_NAMES: [&str; 4] = ["cmd", "opt", "ctrl", "shift"];

/// Font size bounds (points). Values outside are clamped, not rejected.
pub const FONT_SIZE_MIN: u32 = 9;
pub const FONT_SIZE_MAX: u32 = 32;

/// `max_results` bounds. Values outside are clamped, not rejected.
pub const MAX_RESULTS_MIN: usize = 1;
pub const MAX_RESULTS_MAX: usize = 100;

/// The summon hotkey section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyConfig {
    /// A single letter or digit, or `space` / `tab` / `return` /
    /// `f1`..`f12`. Stored lowercase. Default `"space"`.
    pub key: String,
    /// Non-empty, deduplicated, lowercase names from [`MODIFIER_NAMES`].
    /// Default `["opt"]`.
    pub modifiers: Vec<String>,
}

/// The theme section. Colors are `#RRGGBB` strings, validated at parse
/// time; [`parse_hex_color`] turns them into channel tuples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeConfig {
    /// Panel background. Default `"#1C1C21"` (the built-in dark panel).
    pub background: String,
    /// Query and row text. Default `"#FFFFFF"`.
    pub foreground: String,
    /// Selection and highlight color. Default `"#5AC8FA"`.
    pub accent: String,
    /// Query field font size in points, clamped to
    /// [`FONT_SIZE_MIN`]..=[`FONT_SIZE_MAX`]. Default 22.
    pub font_size: u32,
}

/// The whole config document. See the module docs for field semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Alias word -> command/item id.
    pub aliases: BTreeMap<String, String>,
    /// The summon hotkey.
    pub hotkey: HotkeyConfig,
    /// Panel colors and font size.
    pub theme: ThemeConfig,
    /// Rows shown per query.
    pub max_results: usize,
    /// Trigger keyword -> built-in source name. Always contains at least
    /// the identity defaults for the seven sources plus the
    /// [`TRIGGER_SYNONYMS`].
    pub triggers: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            aliases: BTreeMap::new(),
            hotkey: HotkeyConfig {
                key: "space".to_string(),
                modifiers: vec!["opt".to_string()],
            },
            theme: ThemeConfig {
                background: "#1C1C21".to_string(),
                foreground: "#FFFFFF".to_string(),
                accent: "#5AC8FA".to_string(),
                font_size: 22,
            },
            max_results: 9,
            triggers: TRIGGER_SOURCES
                .iter()
                .map(|s| (s.to_string(), s.to_string()))
                .chain(
                    TRIGGER_SYNONYMS
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string())),
                )
                .collect(),
        }
    }
}

/// Typed config failure. Parsing and loading never panic.
#[derive(Debug)]
pub enum ConfigError {
    /// The underlying file could not be read or parsed as JSON.
    Store(StoreError),
    /// The document parsed but a section has the wrong shape.
    Schema(&'static str),
    /// A theme color is not a `#RRGGBB` string.
    BadColor { field: &'static str, value: String },
    /// A `hotkey.modifiers` entry is not one of [`MODIFIER_NAMES`].
    UnknownModifier(String),
    /// `hotkey.key` does not resolve through [`keycode_for`].
    UnknownKey(String),
    /// An alias word is empty or contains whitespace.
    BadAlias { alias: String },
    /// An alias maps to an empty target id.
    EmptyAliasTarget { alias: String },
    /// A trigger keyword is empty or contains whitespace.
    BadTrigger { keyword: String },
    /// A trigger points at something not in [`TRIGGER_SOURCES`].
    UnknownTriggerTarget { keyword: String, target: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Store(e) => write!(f, "config store: {e}"),
            ConfigError::Schema(what) => write!(f, "config schema: {what}"),
            ConfigError::BadColor { field, value } => {
                write!(f, "config theme.{field}: {value:?} is not \"#RRGGBB\"")
            }
            ConfigError::UnknownModifier(name) => {
                write!(
                    f,
                    "config hotkey: unknown modifier {name:?} (use cmd, opt, ctrl, shift)"
                )
            }
            ConfigError::UnknownKey(name) => {
                write!(f, "config hotkey: unknown key {name:?}")
            }
            ConfigError::BadAlias { alias } => {
                write!(
                    f,
                    "config aliases: {alias:?} must be one non-empty word without whitespace"
                )
            }
            ConfigError::EmptyAliasTarget { alias } => {
                write!(f, "config aliases: {alias:?} maps to an empty target")
            }
            ConfigError::BadTrigger { keyword } => {
                write!(
                    f,
                    "config triggers: {keyword:?} must be one non-empty word without whitespace"
                )
            }
            ConfigError::UnknownTriggerTarget { keyword, target } => {
                write!(
                    f,
                    "config triggers: {keyword:?} points at unknown source {target:?}"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for ConfigError {
    fn from(e: StoreError) -> Self {
        ConfigError::Store(e)
    }
}

/// Parse a `#RRGGBB` string into channel bytes. Case-insensitive hex;
/// anything else (wrong length, missing `#`, non-hex bytes) is `None`.
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some((
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

/// Virtual keycodes for the configurable hotkey keys. The numbers are the
/// kVK_ constants from Carbon's Events.h (HIToolbox framework), hardcoded
/// as data because beckon links no C headers: kVK_ANSI_A..kVK_ANSI_Z,
/// kVK_ANSI_0..kVK_ANSI_9, kVK_Space, kVK_Tab, kVK_Return, kVK_F1..kVK_F12.
/// These are ANSI layout positions, the same ones RegisterEventHotKey
/// takes. The core keeps the table; the macOS shell applies it.
const KEYCODES: [(&str, u16); 51] = [
    ("a", 0x00),
    ("b", 0x0B),
    ("c", 0x08),
    ("d", 0x02),
    ("e", 0x0E),
    ("f", 0x03),
    ("g", 0x05),
    ("h", 0x04),
    ("i", 0x22),
    ("j", 0x26),
    ("k", 0x28),
    ("l", 0x25),
    ("m", 0x2E),
    ("n", 0x2D),
    ("o", 0x1F),
    ("p", 0x23),
    ("q", 0x0C),
    ("r", 0x0F),
    ("s", 0x01),
    ("t", 0x11),
    ("u", 0x20),
    ("v", 0x09),
    ("w", 0x0D),
    ("x", 0x07),
    ("y", 0x10),
    ("z", 0x06),
    ("0", 0x1D),
    ("1", 0x12),
    ("2", 0x13),
    ("3", 0x14),
    ("4", 0x15),
    ("5", 0x17),
    ("6", 0x16),
    ("7", 0x1A),
    ("8", 0x1C),
    ("9", 0x19),
    ("space", 0x31),
    ("tab", 0x30),
    ("return", 0x24),
    ("f1", 0x7A),
    ("f2", 0x78),
    ("f3", 0x63),
    ("f4", 0x76),
    ("f5", 0x60),
    ("f6", 0x61),
    ("f7", 0x62),
    ("f8", 0x64),
    ("f9", 0x65),
    ("f10", 0x6D),
    ("f11", 0x67),
    ("f12", 0x6F),
];

/// Resolve a config key name to its ANSI virtual keycode (see [`KEYCODES`]
/// for the provenance of the numbers). Case-insensitive; `None` for
/// anything outside the supported set.
pub fn keycode_for(key: &str) -> Option<u16> {
    let k = key.to_ascii_lowercase();
    KEYCODES
        .iter()
        .find(|(name, _)| *name == k)
        .map(|(_, code)| *code)
}

/// Parse a config document from a [`Value`] tree. Missing sections take
/// their defaults; present sections are validated (see the module docs).
pub fn parse(value: &Value) -> Result<Config, ConfigError> {
    let root = value
        .as_object()
        .ok_or(ConfigError::Schema("config root must be an object"))?;
    let mut config = Config::default();

    if let Some(v) = root.get("aliases") {
        let map = v
            .as_object()
            .ok_or(ConfigError::Schema("aliases must be an object"))?;
        for (word, target) in map {
            let target = target
                .as_str()
                .ok_or(ConfigError::Schema("alias targets must be strings"))?;
            if word.is_empty() || word.chars().any(char::is_whitespace) {
                return Err(ConfigError::BadAlias {
                    alias: word.clone(),
                });
            }
            if target.is_empty() {
                return Err(ConfigError::EmptyAliasTarget {
                    alias: word.clone(),
                });
            }
            config.aliases.insert(word.clone(), target.to_string());
        }
    }

    if let Some(v) = root.get("hotkey") {
        let obj = v
            .as_object()
            .ok_or(ConfigError::Schema("hotkey must be an object"))?;
        if let Some(k) = obj.get("key") {
            let k = k
                .as_str()
                .ok_or(ConfigError::Schema("hotkey.key must be a string"))?
                .to_ascii_lowercase();
            if keycode_for(&k).is_none() {
                return Err(ConfigError::UnknownKey(k));
            }
            config.hotkey.key = k;
        }
        if let Some(m) = obj.get("modifiers") {
            let arr = m
                .as_array()
                .ok_or(ConfigError::Schema("hotkey.modifiers must be an array"))?;
            let mut mods: Vec<String> = Vec::new();
            for item in arr {
                let name = item
                    .as_str()
                    .ok_or(ConfigError::Schema("hotkey modifiers must be strings"))?
                    .to_ascii_lowercase();
                if !MODIFIER_NAMES.contains(&name.as_str()) {
                    return Err(ConfigError::UnknownModifier(name));
                }
                if !mods.contains(&name) {
                    mods.push(name);
                }
            }
            // A global chord with zero modifiers would swallow a plain
            // keypress system-wide; refuse to model that.
            if mods.is_empty() {
                return Err(ConfigError::Schema(
                    "hotkey.modifiers must name at least one modifier",
                ));
            }
            config.hotkey.modifiers = mods;
        }
    }

    if let Some(v) = root.get("theme") {
        let obj = v
            .as_object()
            .ok_or(ConfigError::Schema("theme must be an object"))?;
        for (field, slot) in [
            ("background", &mut config.theme.background),
            ("foreground", &mut config.theme.foreground),
            ("accent", &mut config.theme.accent),
        ] {
            if let Some(c) = obj.get(field) {
                let c = c
                    .as_str()
                    .ok_or(ConfigError::Schema("theme colors must be strings"))?;
                if parse_hex_color(c).is_none() {
                    return Err(ConfigError::BadColor {
                        field,
                        value: c.to_string(),
                    });
                }
                *slot = c.to_string();
            }
        }
        if let Some(n) = obj.get("font_size") {
            let n = n
                .as_int()
                .ok_or(ConfigError::Schema("theme.font_size must be an integer"))?;
            config.theme.font_size =
                n.clamp(i128::from(FONT_SIZE_MIN), i128::from(FONT_SIZE_MAX)) as u32;
        }
    }

    if let Some(n) = root.get("max_results") {
        let n = n
            .as_int()
            .ok_or(ConfigError::Schema("max_results must be an integer"))?;
        config.max_results = n.clamp(MAX_RESULTS_MIN as i128, MAX_RESULTS_MAX as i128) as usize;
    }

    if let Some(v) = root.get("triggers") {
        let map = v
            .as_object()
            .ok_or(ConfigError::Schema("triggers must be an object"))?;
        for (keyword, target) in map {
            let target = target
                .as_str()
                .ok_or(ConfigError::Schema("trigger targets must be strings"))?;
            let keyword_norm = keyword.to_lowercase();
            if keyword_norm.is_empty() || keyword_norm.chars().any(char::is_whitespace) {
                return Err(ConfigError::BadTrigger {
                    keyword: keyword.clone(),
                });
            }
            if !TRIGGER_SOURCES.contains(&target) {
                return Err(ConfigError::UnknownTriggerTarget {
                    keyword: keyword.clone(),
                    target: target.to_string(),
                });
            }
            // Merged over the identity defaults: renames add or repoint,
            // they never silently drop the built-in keywords.
            config.triggers.insert(keyword_norm, target.to_string());
        }
    }

    Ok(config)
}

/// Serialize to a canonical [`Value`] tree. `parse(to_value(c)) == c` for
/// every `Config` this module produces (golden-tested), because `Config`
/// only ever holds normalized, validated data.
pub fn to_value(config: &Config) -> Value {
    let str_map = |m: &BTreeMap<String, String>| {
        Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
                .collect(),
        )
    };
    let mut hotkey = BTreeMap::new();
    hotkey.insert("key".to_string(), Value::Str(config.hotkey.key.clone()));
    hotkey.insert(
        "modifiers".to_string(),
        Value::Array(
            config
                .hotkey
                .modifiers
                .iter()
                .map(|m| Value::Str(m.clone()))
                .collect(),
        ),
    );
    let mut theme = BTreeMap::new();
    theme.insert(
        "background".to_string(),
        Value::Str(config.theme.background.clone()),
    );
    theme.insert(
        "foreground".to_string(),
        Value::Str(config.theme.foreground.clone()),
    );
    theme.insert(
        "accent".to_string(),
        Value::Str(config.theme.accent.clone()),
    );
    theme.insert(
        "font_size".to_string(),
        Value::Int(i128::from(config.theme.font_size)),
    );
    let mut root = BTreeMap::new();
    root.insert("aliases".to_string(), str_map(&config.aliases));
    root.insert("hotkey".to_string(), Value::Object(hotkey));
    root.insert("theme".to_string(), Value::Object(theme));
    root.insert(
        "max_results".to_string(),
        Value::Int(config.max_results as i128),
    );
    root.insert("triggers".to_string(), str_map(&config.triggers));
    Value::Object(root)
}

/// Load the config file. Mirrors the other stores: a missing file is
/// `Ok(None)` (the caller uses `Config::default()`), a present but
/// corrupt or invalid file is a typed error.
pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
    match persist::load_value(path)? {
        Some(value) => Ok(Some(parse(&value)?)),
        None => Ok(None),
    }
}

/// Save a config through the canonical codec with an atomic write. The
/// counterpart of [`load`]; a future preferences window writes here.
pub fn save(config: &Config, path: &Path) -> Result<(), StoreError> {
    persist::save_value(path, &to_value(config))
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
            "beckon-config-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn parse_json(json: &str) -> Result<Config, ConfigError> {
        parse(&persist::parse(json).expect("test document must be valid JSON"))
    }

    // ---- defaults ----

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = Config::default();
        assert!(c.aliases.is_empty());
        assert_eq!(c.hotkey.key, "space");
        assert_eq!(c.hotkey.modifiers, vec!["opt"]);
        assert_eq!(c.theme.background, "#1C1C21");
        assert_eq!(c.theme.foreground, "#FFFFFF");
        assert_eq!(c.theme.accent, "#5AC8FA");
        assert_eq!(c.theme.font_size, 22);
        assert_eq!(c.max_results, 9);
        assert_eq!(
            c.triggers.len(),
            TRIGGER_SOURCES.len() + TRIGGER_SYNONYMS.len()
        );
        for s in TRIGGER_SOURCES {
            assert_eq!(c.triggers.get(s).map(String::as_str), Some(s));
        }
        for (k, v) in TRIGGER_SYNONYMS {
            assert_eq!(c.triggers.get(k).map(String::as_str), Some(v));
        }
    }

    #[test]
    fn empty_document_parses_to_defaults() {
        assert_eq!(parse_json("{}").expect("parse"), Config::default());
    }

    // ---- round trip ----

    #[test]
    fn golden_default_canonical_form() {
        // The exact bytes a fresh config serializes to, locked.
        assert_eq!(
            to_value(&Config::default()).to_canonical_string(),
            "{\"aliases\":{},\
             \"hotkey\":{\"key\":\"space\",\"modifiers\":[\"opt\"]},\
             \"max_results\":9,\
             \"theme\":{\"accent\":\"#5AC8FA\",\"background\":\"#1C1C21\",\
             \"font_size\":22,\"foreground\":\"#FFFFFF\"},\
             \"triggers\":{\"clip\":\"clip\",\"clipboard\":\"clip\",\"emoji\":\"emoji\",\
             \"file\":\"file\",\"find\":\"file\",\"go\":\"go\",\"menu\":\"menu\",\
             \"snip\":\"snip\",\"snippet\":\"snip\",\"win\":\"win\",\
             \"windows\":\"win\"}}"
        );
    }

    #[test]
    fn parse_to_value_round_trip() {
        let mut c = Config::default();
        c.aliases
            .insert("lh".to_string(), "window.left-half".to_string());
        c.hotkey.key = "k".to_string();
        c.hotkey.modifiers = vec!["cmd".to_string(), "shift".to_string()];
        c.theme.background = "#000000".to_string();
        c.theme.font_size = 18;
        c.max_results = 12;
        c.triggers.insert("v".to_string(), "clip".to_string());
        let v = to_value(&c);
        let back = parse(&v).expect("round trip parses");
        assert_eq!(back, c);
        // Canonical form is a fixed point.
        assert_eq!(
            to_value(&back).to_canonical_string(),
            v.to_canonical_string()
        );
    }

    // ---- section parsing ----

    #[test]
    fn parse_full_document() {
        let c = parse_json(
            r##"{
                "aliases": {"lh": "window.left-half", "g": "app.chrome"},
                "hotkey": {"key": "Space", "modifiers": ["CMD", "shift"]},
                "max_results": 5,
                "theme": {"background": "#101010", "accent": "#ff8800"},
                "triggers": {"v": "clip", "w": "win"}
            }"##,
        )
        .expect("parse");
        assert_eq!(
            c.aliases.get("lh").map(String::as_str),
            Some("window.left-half")
        );
        // Key and modifier names normalize to lowercase.
        assert_eq!(c.hotkey.key, "space");
        assert_eq!(c.hotkey.modifiers, vec!["cmd", "shift"]);
        assert_eq!(c.max_results, 5);
        assert_eq!(c.theme.background, "#101010");
        assert_eq!(c.theme.accent, "#ff8800");
        // Unset theme keys keep their defaults.
        assert_eq!(c.theme.foreground, "#FFFFFF");
        assert_eq!(c.theme.font_size, 22);
        // Renames merge over the identity defaults.
        assert_eq!(c.triggers.get("v").map(String::as_str), Some("clip"));
        assert_eq!(c.triggers.get("clip").map(String::as_str), Some("clip"));
        assert_eq!(
            c.triggers.len(),
            TRIGGER_SOURCES.len() + TRIGGER_SYNONYMS.len() + 2
        );
    }

    #[test]
    fn duplicate_modifiers_deduplicate() {
        let c = parse_json(r##"{"hotkey":{"modifiers":["opt","OPT","cmd"]}}"##).expect("parse");
        assert_eq!(c.hotkey.modifiers, vec!["opt", "cmd"]);
    }

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let c = parse_json(r##"{"future_section": {"x": 1}}"##).expect("parse");
        assert_eq!(c, Config::default());
    }

    // ---- validation errors ----

    #[test]
    fn rejects_non_object_root() {
        assert!(matches!(
            parse(&Value::Array(vec![])),
            Err(ConfigError::Schema("config root must be an object"))
        ));
    }

    #[test]
    fn rejects_bad_colors() {
        for (json, field) in [
            (r##"{"theme":{"background":"red"}}"##, "background"),
            (r##"{"theme":{"foreground":"#FFF"}}"##, "foreground"),
            (r##"{"theme":{"accent":"#GGGGGG"}}"##, "accent"),
            (r##"{"theme":{"background":"1C1C21"}}"##, "background"),
            (r##"{"theme":{"background":"#1C1C211"}}"##, "background"),
        ] {
            match parse_json(json) {
                Err(ConfigError::BadColor { field: f, .. }) => assert_eq!(f, field),
                other => panic!("{json} should be BadColor, got {other:?}"),
            }
        }
        // Non-string colors are a shape error.
        assert!(matches!(
            parse_json(r##"{"theme":{"background":7}}"##),
            Err(ConfigError::Schema(_))
        ));
    }

    #[test]
    fn rejects_unknown_modifiers() {
        match parse_json(r##"{"hotkey":{"modifiers":["opt","hyper"]}}"##) {
            Err(ConfigError::UnknownModifier(name)) => assert_eq!(name, "hyper"),
            other => panic!("expected UnknownModifier, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_modifier_list() {
        assert!(matches!(
            parse_json(r##"{"hotkey":{"modifiers":[]}}"##),
            Err(ConfigError::Schema(_))
        ));
    }

    #[test]
    fn rejects_unknown_keys() {
        for json in [
            r##"{"hotkey":{"key":"escape"}}"##,
            r##"{"hotkey":{"key":"f13"}}"##,
            r##"{"hotkey":{"key":""}}"##,
        ] {
            assert!(
                matches!(parse_json(json), Err(ConfigError::UnknownKey(_))),
                "{json}"
            );
        }
    }

    #[test]
    fn rejects_bad_aliases() {
        assert!(matches!(
            parse_json(r##"{"aliases":{"":"x"}}"##),
            Err(ConfigError::BadAlias { .. })
        ));
        assert!(matches!(
            parse_json(r##"{"aliases":{"two words":"x"}}"##),
            Err(ConfigError::BadAlias { .. })
        ));
        match parse_json(r##"{"aliases":{"lh":""}}"##) {
            Err(ConfigError::EmptyAliasTarget { alias }) => assert_eq!(alias, "lh"),
            other => panic!("expected EmptyAliasTarget, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_triggers() {
        assert!(matches!(
            parse_json(r##"{"triggers":{"":"clip"}}"##),
            Err(ConfigError::BadTrigger { .. })
        ));
        assert!(matches!(
            parse_json(r##"{"triggers":{"a b":"clip"}}"##),
            Err(ConfigError::BadTrigger { .. })
        ));
        match parse_json(r##"{"triggers":{"v":"clipboardz"}}"##) {
            Err(ConfigError::UnknownTriggerTarget { keyword, target }) => {
                assert_eq!(keyword, "v");
                assert_eq!(target, "clipboardz");
            }
            other => panic!("expected UnknownTriggerTarget, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_section_shapes() {
        for json in [
            r##"{"aliases":[1]}"##,
            r##"{"aliases":{"a":1}}"##,
            r##"{"hotkey":"opt+space"}"##,
            r##"{"hotkey":{"key":49}}"##,
            r##"{"hotkey":{"modifiers":"opt"}}"##,
            r##"{"hotkey":{"modifiers":[1]}}"##,
            r##"{"theme":[]}"##,
            r##"{"theme":{"font_size":"big"}}"##,
            r##"{"max_results":"nine"}"##,
            r##"{"triggers":[]}"##,
            r##"{"triggers":{"v":9}}"##,
        ] {
            assert!(
                matches!(parse_json(json), Err(ConfigError::Schema(_))),
                "{json}"
            );
        }
    }

    #[test]
    fn font_size_and_max_results_clamp() {
        let c = parse_json(r##"{"theme":{"font_size":2},"max_results":0}"##).expect("parse");
        assert_eq!(c.theme.font_size, FONT_SIZE_MIN);
        assert_eq!(c.max_results, MAX_RESULTS_MIN);
        let c = parse_json(r##"{"theme":{"font_size":500},"max_results":9999}"##).expect("parse");
        assert_eq!(c.theme.font_size, FONT_SIZE_MAX);
        assert_eq!(c.max_results, MAX_RESULTS_MAX);
        let c = parse_json(r##"{"theme":{"font_size":-3},"max_results":-1}"##).expect("parse");
        assert_eq!(c.theme.font_size, FONT_SIZE_MIN);
        assert_eq!(c.max_results, MAX_RESULTS_MIN);
    }

    // ---- hex colors ----

    #[test]
    fn hex_color_goldens() {
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#1C1C21"), Some((0x1C, 0x1C, 0x21)));
        assert_eq!(parse_hex_color("#5AC8FA"), Some((0x5A, 0xC8, 0xFA)));
        assert_eq!(parse_hex_color("#ff8800"), Some((0xFF, 0x88, 0x00)));
        for bad in ["", "#", "#FFF", "#FFFFFFF", "FFFFFF", "#GGGGGG", "#12345"] {
            assert_eq!(parse_hex_color(bad), None, "{bad:?}");
        }
        // Every default color parses (the defaults must validate).
        let t = Config::default().theme;
        for c in [&t.background, &t.foreground, &t.accent] {
            assert!(parse_hex_color(c).is_some(), "{c}");
        }
    }

    // ---- keycodes ----

    #[test]
    fn keycode_table_sanity() {
        // Anchors straight out of Carbon's Events.h.
        assert_eq!(keycode_for("a"), Some(0x00));
        assert_eq!(keycode_for("s"), Some(0x01));
        assert_eq!(keycode_for("z"), Some(0x06));
        assert_eq!(keycode_for("0"), Some(0x1D));
        assert_eq!(keycode_for("9"), Some(0x19));
        // kVK_Space = 49, the value the shell's default hotkey hardcodes.
        assert_eq!(keycode_for("space"), Some(49));
        assert_eq!(keycode_for("tab"), Some(0x30));
        assert_eq!(keycode_for("return"), Some(0x24));
        assert_eq!(keycode_for("f1"), Some(0x7A));
        assert_eq!(keycode_for("f12"), Some(0x6F));
        // Case-insensitive.
        assert_eq!(keycode_for("A"), keycode_for("a"));
        assert_eq!(keycode_for("SPACE"), keycode_for("space"));
        // Unknown names miss.
        for bad in ["", "escape", "f13", "aa", "ü", "spacebar"] {
            assert_eq!(keycode_for(bad), None, "{bad:?}");
        }
        // Full coverage: 26 letters + 10 digits + 3 named + 12 F keys,
        // no duplicate names, no duplicate codes.
        assert_eq!(KEYCODES.len(), 51);
        for (i, (name_a, code_a)) in KEYCODES.iter().enumerate() {
            for (name_b, code_b) in &KEYCODES[i + 1..] {
                assert_ne!(name_a, name_b);
                assert_ne!(code_a, code_b, "{name_a} and {name_b} share a code");
            }
        }
    }

    // ---- load and save ----

    #[test]
    fn load_missing_file_is_none() {
        let dir = temp_dir();
        assert!(load(&dir.join("absent.json")).expect("load").is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let path = dir.join(CONFIG_FILE);
        let mut c = Config::default();
        c.aliases
            .insert("lh".to_string(), "window.left-half".to_string());
        c.triggers.insert("v".to_string(), "clip".to_string());
        save(&c, &path).expect("save");
        let loaded = load(&path).expect("load").expect("present");
        assert_eq!(loaded, c);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_reports_corruption_and_invalid_documents() {
        let dir = temp_dir();
        let path = dir.join(CONFIG_FILE);
        // Unparseable JSON bubbles up as a store error.
        persist::write_atomic(&path, b"{nope").expect("write");
        assert!(matches!(load(&path), Err(ConfigError::Store(_))));
        // Valid JSON with an invalid section is a typed config error.
        persist::write_atomic(&path, br##"{"hotkey":{"modifiers":["hyper"]}}"##).expect("write");
        assert!(matches!(load(&path), Err(ConfigError::UnknownModifier(_))));
        fs::remove_dir_all(&dir).ok();
    }
}
