//! Canonical JSON codec and atomic file store.
//!
//! Everything beckon persists (clipboard history, snippets, frecency,
//! settings) goes through this module. The codec is deliberately narrower
//! than JSON: there is NO float variant. This codebase stores scaled
//! integers only, so any float in an input document is rejected with a
//! typed error rather than parsed lossily.
//!
//! Canonical form (what [`Value::to_canonical_string`] emits):
//!   - object keys sorted (the `BTreeMap` representation guarantees this)
//!   - no whitespace anywhere
//!   - minimal escapes: only `"` `\` and control characters are escaped;
//!     `\b \f \n \r \t` use their short forms, other controls use
//!     lowercase `\u00xx`
//!   - integers in plain base-10 with no leading zeros
//!
//! Same tree in, same bytes out, on every platform, forever. The parser is
//! more liberal: it accepts standard JSON (minus floats), including
//! arbitrary whitespace, `\uXXXX` escapes, and surrogate pairs, so hand
//! edited files round-trip. Duplicate object keys are rejected because a
//! canonical store must have exactly one meaning per document.
//!
//! Writes are atomic: content goes to a temp file in the same directory,
//! is fsynced, then renamed over the target, and the directory is synced.
//! A crash leaves either the old file or the new one, never a torn write.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum nesting depth the parser accepts. Guards against stack
/// exhaustion on adversarial input; honest documents are nowhere close.
const MAX_DEPTH: usize = 128;

/// A JSON-shaped value with no float variant. Numbers are `i128`; anything
/// that needs fractions stores scaled integers (see `calc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i128),
    Str(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// Borrow the integer payload, if this is an `Int`.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Borrow the boolean payload, if this is a `Bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Borrow the string payload, if this is a `Str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow the element list, if this is an `Array`.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Borrow the key map, if this is an `Object`.
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Look up a key on an `Object`; `None` for other variants too.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|map| map.get(key))
    }

    /// Serialize to the canonical byte-deterministic form.
    pub fn to_canonical_string(&self) -> String {
        let mut out = String::new();
        write_value(self, &mut out);
        out
    }
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Str(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Typed parse failure. Positions are byte offsets into the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Input ended in the middle of a value.
    UnexpectedEof,
    /// A byte that cannot start or continue the expected construct.
    UnexpectedByte { pos: usize, byte: u8 },
    /// A number contained `.`, `e`, or `E`. This codec stores integers only.
    FloatRejected { pos: usize },
    /// An integer literal does not fit in `i128`.
    IntOverflow { pos: usize },
    /// A number had a leading zero (standard JSON forbids `0123`).
    LeadingZero { pos: usize },
    /// A backslash escape the codec does not recognize.
    InvalidEscape { pos: usize },
    /// A malformed `\uXXXX` escape or an unpaired surrogate.
    InvalidUnicodeEscape { pos: usize },
    /// A raw control character inside a string literal.
    ControlCharInString { pos: usize },
    /// The same key appeared twice in one object.
    DuplicateKey { pos: usize, key: String },
    /// Nesting exceeded [`MAX_DEPTH`].
    TooDeep { pos: usize },
    /// Bytes remained after the first complete value.
    TrailingData { pos: usize },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedEof => write!(f, "unexpected end of input"),
            ParseError::UnexpectedByte { pos, byte } => {
                write!(f, "unexpected byte 0x{byte:02x} at offset {pos}")
            }
            ParseError::FloatRejected { pos } => {
                write!(
                    f,
                    "float literal at offset {pos}: this store holds integers only"
                )
            }
            ParseError::IntOverflow { pos } => {
                write!(f, "integer at offset {pos} does not fit in i128")
            }
            ParseError::LeadingZero { pos } => {
                write!(f, "number with leading zero at offset {pos}")
            }
            ParseError::InvalidEscape { pos } => {
                write!(f, "invalid escape sequence at offset {pos}")
            }
            ParseError::InvalidUnicodeEscape { pos } => {
                write!(f, "invalid unicode escape at offset {pos}")
            }
            ParseError::ControlCharInString { pos } => {
                write!(f, "raw control character in string at offset {pos}")
            }
            ParseError::DuplicateKey { pos, key } => {
                write!(f, "duplicate object key {key:?} at offset {pos}")
            }
            ParseError::TooDeep { pos } => {
                write!(f, "nesting deeper than {MAX_DEPTH} at offset {pos}")
            }
            ParseError::TrailingData { pos } => {
                write!(f, "trailing data after value at offset {pos}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse one JSON document (floats rejected). Surrounding whitespace is
/// allowed; anything else after the value is an error.
pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser { src: input, pos: 0 };
    parser.skip_ws();
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.src.len() {
        return Err(ParseError::TrailingData { pos: parser.pos });
    }
    Ok(value)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn expect_literal(&mut self, lit: &str) -> Result<(), ParseError> {
        if self.src[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            match self.peek() {
                Some(byte) => Err(ParseError::UnexpectedByte {
                    pos: self.pos,
                    byte,
                }),
                None => Err(ParseError::UnexpectedEof),
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep { pos: self.pos });
        }
        match self.peek() {
            None => Err(ParseError::UnexpectedEof),
            Some(b'n') => {
                self.expect_literal("null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.expect_literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect_literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(byte) => Err(ParseError::UnexpectedByte {
                pos: self.pos,
                byte,
            }),
        }
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        let negative = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let digits_start = self.pos;
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return match self.peek() {
                Some(byte) => Err(ParseError::UnexpectedByte {
                    pos: self.pos,
                    byte,
                }),
                None => Err(ParseError::UnexpectedEof),
            };
        }
        let digits = &self.src[digits_start..self.pos];
        if digits.len() > 1 && digits.starts_with('0') {
            return Err(ParseError::LeadingZero { pos: start });
        }
        // The whole point of this codec: floats are not values here.
        if let Some(b'.') | Some(b'e') | Some(b'E') = self.peek() {
            return Err(ParseError::FloatRejected { pos: start });
        }
        // Accumulate toward the final sign so the full i128 range parses,
        // including i128::MIN (whose magnitude has no positive counterpart).
        let mut acc: i128 = 0;
        for b in digits.bytes() {
            let digit = i128::from(b - b'0');
            acc = acc
                .checked_mul(10)
                .and_then(|m| {
                    if negative {
                        m.checked_sub(digit)
                    } else {
                        m.checked_add(digit)
                    }
                })
                .ok_or(ParseError::IntOverflow { pos: start })?;
        }
        Ok(Value::Int(acc))
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        // Caller guarantees the opening quote.
        self.pos += 1;
        let mut out = String::new();
        let mut run_start = self.pos;
        loop {
            let Some(b) = self.peek() else {
                return Err(ParseError::UnexpectedEof);
            };
            match b {
                b'"' => {
                    out.push_str(&self.src[run_start..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(&self.src[run_start..self.pos]);
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                    run_start = self.pos;
                }
                0x00..=0x1f => {
                    return Err(ParseError::ControlCharInString { pos: self.pos });
                }
                _ => self.pos += 1,
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let esc_pos = self.pos - 1;
        let Some(b) = self.peek() else {
            return Err(ParseError::UnexpectedEof);
        };
        self.pos += 1;
        match b {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.parse_hex4(esc_pos)?;
                let code = if (0xd800..=0xdbff).contains(&first) {
                    // High surrogate: a low surrogate escape must follow.
                    if self.peek() != Some(b'\\') {
                        return Err(ParseError::InvalidUnicodeEscape { pos: esc_pos });
                    }
                    self.pos += 1;
                    if self.peek() != Some(b'u') {
                        return Err(ParseError::InvalidUnicodeEscape { pos: esc_pos });
                    }
                    self.pos += 1;
                    let second = self.parse_hex4(esc_pos)?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(ParseError::InvalidUnicodeEscape { pos: esc_pos });
                    }
                    0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    // Lone low surrogate.
                    return Err(ParseError::InvalidUnicodeEscape { pos: esc_pos });
                } else {
                    first
                };
                let ch = char::from_u32(code)
                    .ok_or(ParseError::InvalidUnicodeEscape { pos: esc_pos })?;
                out.push(ch);
            }
            _ => return Err(ParseError::InvalidEscape { pos: esc_pos }),
        }
        Ok(())
    }

    fn parse_hex4(&mut self, esc_pos: usize) -> Result<u32, ParseError> {
        let mut code: u32 = 0;
        for _ in 0..4 {
            let Some(b) = self.peek() else {
                return Err(ParseError::UnexpectedEof);
            };
            let digit = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a') + 10,
                b'A'..=b'F' => u32::from(b - b'A') + 10,
                _ => return Err(ParseError::InvalidUnicodeEscape { pos: esc_pos }),
            };
            code = code * 16 + digit;
            self.pos += 1;
        }
        Ok(code)
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, ParseError> {
        // Caller guarantees the opening bracket.
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                Some(byte) => {
                    return Err(ParseError::UnexpectedByte {
                        pos: self.pos,
                        byte,
                    })
                }
                None => return Err(ParseError::UnexpectedEof),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Value, ParseError> {
        // Caller guarantees the opening brace.
        self.pos += 1;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            let key_pos = self.pos;
            if self.peek() != Some(b'"') {
                return match self.peek() {
                    Some(byte) => Err(ParseError::UnexpectedByte {
                        pos: self.pos,
                        byte,
                    }),
                    None => Err(ParseError::UnexpectedEof),
                };
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return match self.peek() {
                    Some(byte) => Err(ParseError::UnexpectedByte {
                        pos: self.pos,
                        byte,
                    }),
                    None => Err(ParseError::UnexpectedEof),
                };
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.parse_value(depth + 1)?;
            if map.insert(key.clone(), value).is_some() {
                return Err(ParseError::DuplicateKey { pos: key_pos, key });
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                Some(byte) => {
                    return Err(ParseError::UnexpectedByte {
                        pos: self.pos,
                        byte,
                    })
                }
                None => return Err(ParseError::UnexpectedEof),
            }
        }
    }
}

/// Failure of a load or save through the file store.
#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Parse(ParseError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "io error: {e}"),
            StoreError::Parse(e) => write!(f, "parse error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Parse(e) => Some(e),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<ParseError> for StoreError {
    fn from(e: ParseError) -> Self {
        StoreError::Parse(e)
    }
}

/// Distinguishes temp files from concurrent writers in the same process.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, rename over the target, fsync the directory. On unix the temp
/// file is created with mode 0600 (stores can hold clipboard text).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let dir: PathBuf = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = write_atomic_inner(&tmp, path, &dir, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_atomic_inner(tmp: &Path, path: &Path, dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open.mode(0o600);
    }
    let mut file = open.open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    // Persist the rename itself. Directory fsync is a unix notion; on
    // other platforms the rename is the best we can do.
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Read a whole file, mapping "does not exist" to `None`.
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Serialize `value` canonically (plus a trailing newline for grep and
/// cat friendliness) and write it atomically to `path`.
pub fn save_value(path: &Path, value: &Value) -> Result<(), StoreError> {
    let mut text = value.to_canonical_string();
    text.push('\n');
    write_atomic(path, text.as_bytes())?;
    Ok(())
}

/// Load and parse a value written by [`save_value`]. Missing file is
/// `Ok(None)`; a present but malformed file is an error.
pub fn load_value(path: &Path) -> Result<Option<Value>, StoreError> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|e| StoreError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
    Ok(Some(parse(&text)?))
}

/// Resolve the beckon store root: `$BECKON_HOME` if set and non-empty,
/// else `$HOME/.beckon`, else a relative `.beckon` as a last resort.
pub fn store_root() -> PathBuf {
    store_root_from(std::env::var_os("BECKON_HOME"), std::env::var_os("HOME"))
}

fn store_root_from(beckon_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(dir) = beckon_home {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    match home {
        Some(h) if !h.is_empty() => Path::new(&h).join(".beckon"),
        _ => PathBuf::from(".beckon"),
    }
}

/// Resolve the store root and make sure it exists with owner-only (0700)
/// permissions. Idempotent: an existing directory is tightened to 0700.
pub fn ensure_store_root() -> io::Result<PathBuf> {
    let root = store_root();
    fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh per-test directory under the system temp dir. Nothing here
    /// ever touches the real home directory.
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "beckon-persist-test-{}-{}",
            std::process::id(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn canonical_scalars() {
        assert_eq!(Value::Null.to_canonical_string(), "null");
        assert_eq!(Value::Bool(true).to_canonical_string(), "true");
        assert_eq!(Value::Bool(false).to_canonical_string(), "false");
        assert_eq!(Value::Int(0).to_canonical_string(), "0");
        assert_eq!(Value::Int(-42).to_canonical_string(), "-42");
        assert_eq!(
            Value::Int(i128::MAX).to_canonical_string(),
            "170141183460469231731687303715884105727"
        );
        assert_eq!(
            Value::Int(i128::MIN).to_canonical_string(),
            "-170141183460469231731687303715884105728"
        );
        assert_eq!(
            parse("-170141183460469231731687303715884105728").expect("parse"),
            Value::Int(i128::MIN)
        );
        assert_eq!(Value::Str(String::new()).to_canonical_string(), "\"\"");
    }

    #[test]
    fn canonical_sorts_keys_and_strips_whitespace() {
        let v = parse("{ \"b\" : 1 ,\n\t\"a\" : [ 2 , 3 ] }").expect("parse");
        assert_eq!(v.to_canonical_string(), "{\"a\":[2,3],\"b\":1}");
    }

    #[test]
    fn canonical_escapes_are_minimal() {
        let v = Value::Str("a\"b\\c\nd\te\u{8}\u{c}\r\u{1}z".to_string());
        assert_eq!(
            v.to_canonical_string(),
            "\"a\\\"b\\\\c\\nd\\te\\b\\f\\r\\u0001z\""
        );
        // Non-ASCII passes through unescaped.
        let v = Value::Str("schlüssel 日本 🎉".to_string());
        assert_eq!(v.to_canonical_string(), "\"schlüssel 日本 🎉\"");
    }

    #[test]
    fn parse_accepts_standard_json_forms() {
        assert_eq!(parse("null").expect("parse"), Value::Null);
        assert_eq!(parse("  true  ").expect("parse"), Value::Bool(true));
        assert_eq!(parse("-7").expect("parse"), Value::Int(-7));
        assert_eq!(parse("[]").expect("parse"), Value::Array(vec![]));
        assert_eq!(parse("{}").expect("parse"), Value::Object(BTreeMap::new()));
        assert_eq!(
            parse("[1, [2, [3]]]").expect("parse"),
            Value::Array(vec![
                Value::Int(1),
                Value::Array(vec![Value::Int(2), Value::Array(vec![Value::Int(3)])]),
            ])
        );
    }

    #[test]
    fn parse_handles_escapes_and_surrogate_pairs() {
        assert_eq!(
            parse("\"a\\u0041\\n\\t\\\\\\\"\\/\"").expect("parse"),
            Value::Str("aA\n\t\\\"/".to_string())
        );
        // U+1F389 as a surrogate pair.
        assert_eq!(
            parse("\"\\ud83c\\udf89\"").expect("parse"),
            Value::Str("🎉".to_string())
        );
    }

    #[test]
    fn parse_rejects_floats_with_typed_error() {
        for input in ["1.5", "0.0", "-3.14", "1e3", "2E-4", "1."] {
            match parse(input) {
                Err(ParseError::FloatRejected { .. }) => {}
                other => panic!("{input:?} should reject float, got {other:?}"),
            }
        }
        // Floats nested inside structures are rejected too.
        match parse("{\"a\": 1.5}") {
            Err(ParseError::FloatRejected { .. }) => {}
            other => panic!("nested float should reject, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert!(matches!(parse(""), Err(ParseError::UnexpectedEof)));
        assert!(matches!(parse("["), Err(ParseError::UnexpectedEof)));
        assert!(matches!(parse("\"abc"), Err(ParseError::UnexpectedEof)));
        assert!(matches!(
            parse("nul"),
            Err(ParseError::UnexpectedByte { .. }) | Err(ParseError::UnexpectedEof)
        ));
        assert!(matches!(parse("1 2"), Err(ParseError::TrailingData { .. })));
        assert!(matches!(parse("0123"), Err(ParseError::LeadingZero { .. })));
        assert!(matches!(
            parse("\"\\q\""),
            Err(ParseError::InvalidEscape { .. })
        ));
        assert!(matches!(
            parse("\"\\ud800\""),
            Err(ParseError::InvalidUnicodeEscape { .. })
        ));
        assert!(matches!(
            parse("\"\\udc00\""),
            Err(ParseError::InvalidUnicodeEscape { .. })
        ));
        assert!(matches!(
            parse("{\"a\":1,\"a\":2}"),
            Err(ParseError::DuplicateKey { .. })
        ));
        assert!(matches!(
            parse("\"a\nb\""),
            Err(ParseError::ControlCharInString { .. })
        ));
        // 340282366920938463463374607431768211456 = 2^128, way past i128.
        assert!(matches!(
            parse("340282366920938463463374607431768211456"),
            Err(ParseError::IntOverflow { .. })
        ));
    }

    #[test]
    fn parse_rejects_pathological_nesting() {
        let deep = "[".repeat(MAX_DEPTH + 2);
        assert!(matches!(parse(&deep), Err(ParseError::TooDeep { .. })));
    }

    /// Tiny deterministic xorshift generator for property tests. No rand
    /// crate: std only, seeded, reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_string(rng: &mut Rng) -> String {
        let len = rng.below(12);
        let mut s = String::new();
        for _ in 0..len {
            match rng.below(8) {
                0 => s.push('"'),
                1 => s.push('\\'),
                2 => s.push('\n'),
                3 => s.push('\u{1}'),
                4 => s.push('ü'),
                5 => s.push('🎉'),
                _ => s.push(char::from(b'a' + (rng.below(26) as u8))),
            }
        }
        s
    }

    fn random_value(rng: &mut Rng, depth: usize) -> Value {
        let scalar_only = depth >= 4;
        let choice = if scalar_only {
            rng.below(4)
        } else {
            rng.below(6)
        };
        match choice {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 0),
            2 => {
                // Full i128 range, both signs, including the extremes.
                let hi = u128::from(rng.next());
                let lo = u128::from(rng.next());
                let full = ((hi << 64) | lo) as i128;
                match rng.below(8) {
                    0 => Value::Int(i128::MIN),
                    1 => Value::Int(i128::MAX),
                    2 => Value::Int(full % 1000),
                    _ => Value::Int(full),
                }
            }
            3 => Value::Str(random_string(rng)),
            4 => {
                let n = rng.below(4) as usize;
                Value::Array((0..n).map(|_| random_value(rng, depth + 1)).collect())
            }
            _ => {
                let n = rng.below(4) as usize;
                let mut map = BTreeMap::new();
                for _ in 0..n {
                    map.insert(random_string(rng), random_value(rng, depth + 1));
                }
                Value::Object(map)
            }
        }
    }

    #[test]
    fn round_trip_property() {
        let mut rng = Rng(0x5eed_beef_cafe_f00d);
        for _ in 0..300 {
            let v = random_value(&mut rng, 0);
            let encoded = v.to_canonical_string();
            let decoded = parse(&encoded).expect("canonical output must parse");
            assert_eq!(decoded, v, "value round-trip through {encoded:?}");
            // Canonical form is a fixed point: re-encoding changes nothing.
            assert_eq!(decoded.to_canonical_string(), encoded);
        }
    }

    #[test]
    fn atomic_write_and_read_back() {
        let dir = temp_dir();
        let path = dir.join("store.json");
        assert_eq!(read_optional(&path).expect("read"), None);
        write_atomic(&path, b"hello").expect("write");
        assert_eq!(read_optional(&path).expect("read"), Some(b"hello".to_vec()));
        // Overwrite is atomic too: the old content is fully replaced.
        write_atomic(&path, b"goodbye").expect("rewrite");
        assert_eq!(
            read_optional(&path).expect("read"),
            Some(b"goodbye".to_vec())
        );
        // No temp file droppings remain.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir();
        let path = dir.join("secret.json");
        write_atomic(&path, b"{}").expect("write");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_load_value_round_trip() {
        let dir = temp_dir();
        let path = dir.join("value.json");
        assert!(load_value(&path).expect("load missing").is_none());
        let v = obj(&[
            ("count", Value::Int(3)),
            ("name", Value::Str("beckon".to_string())),
            ("tags", Value::Array(vec![Value::Str("a".to_string())])),
        ]);
        save_value(&path, &v).expect("save");
        let loaded = load_value(&path).expect("load").expect("present");
        assert_eq!(loaded, v);
        // The on-disk form is canonical plus one newline.
        let bytes = read_optional(&path).expect("read").expect("present");
        let mut expected = v.to_canonical_string().into_bytes();
        expected.push(b'\n');
        assert_eq!(bytes, expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_value_reports_corruption() {
        let dir = temp_dir();
        let path = dir.join("corrupt.json");
        write_atomic(&path, b"{\"a\": 1.5}").expect("write");
        assert!(matches!(
            load_value(&path),
            Err(StoreError::Parse(ParseError::FloatRejected { .. }))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_root_resolution_order() {
        let root = store_root_from(
            Some(OsString::from("/x/custom")),
            Some(OsString::from("/home/u")),
        );
        assert_eq!(root, PathBuf::from("/x/custom"));
        let root = store_root_from(None, Some(OsString::from("/home/u")));
        assert_eq!(root, PathBuf::from("/home/u/.beckon"));
        // Empty BECKON_HOME falls through to HOME.
        let root = store_root_from(Some(OsString::new()), Some(OsString::from("/home/u")));
        assert_eq!(root, PathBuf::from("/home/u/.beckon"));
        let root = store_root_from(None, None);
        assert_eq!(root, PathBuf::from(".beckon"));
    }

    #[test]
    fn ensure_store_root_creates_0700_under_beckon_home() {
        // The single test that touches the environment: it points
        // BECKON_HOME at a temp dir so nothing touches the real home.
        let dir = temp_dir().join("root");
        std::env::set_var("BECKON_HOME", &dir);
        let created = ensure_store_root().expect("ensure");
        std::env::remove_var("BECKON_HOME");
        assert_eq!(created, dir);
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&dir).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        fs::remove_dir_all(dir.parent().expect("parent")).ok();
    }
}
