//! One-shot developer transforms behind the launcher bar.
//!
//! Every transform is a pure function of its arguments: no clock reads
//! (callers inject `now_secs`), no randomness (callers inject `entropy`),
//! no filesystem, no network. Same inputs, same output string, on every
//! platform, forever.
//!
//! The SHA-256 here is implemented from the FIPS 180-4 spec and verified
//! against the standard test vectors, but it is a convenience CHECKSUM
//! utility, not a security claim: no constant-time guarantees, no side
//! channel hardening. Use it for etags and file fingerprints, not keys.
//!
//! Command grammar recognized by [`parse_command`]. The first word picks
//! the transform (case-insensitive); the rest of the line, trimmed, is
//! the argument. Unknown first words return `None` so the launcher falls
//! through to ordinary app search.
//!
//! ```text
//! uuid                random uuid v4 from injected entropy (no argument)
//! b64 <text>          base64 encode (alias: base64 <text>)
//! unb64 <text>        base64 decode
//! sha256 <text>       sha-256 checksum, lowercase hex
//! fnv <text>          fnv-1a 64-bit hash, lowercase hex
//! json <text>         pretty-print json, 2-space indent, sorted keys
//! epoch <n>           unix seconds to "YYYY-MM-DD HH:MM:SS UTC";
//!                     bare "epoch" formats the injected now
//! date <text>         "YYYY-MM-DD[ HH:MM[:SS]]" to unix seconds
//! upper <text>        ascii uppercase
//! lower <text>        ascii lowercase
//! title <text>        ascii title case (words split on whitespace)
//! count <text>        chars, bytes, words, lines summary
//! ```
//!
//! JSON pretty-printing parses with [`crate::persist`], which rejects
//! floats by design (this codebase stores scaled integers only); a float
//! in the input is a typed error, not a lossy parse. Object keys come
//! back sorted because the persist tree is a `BTreeMap`.

use crate::clipstore::fnv1a64;
use crate::persist;
use std::fmt;

/// The transforms [`run`] can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevUtil {
    /// Random UUID version 4 built from the injected entropy bytes.
    UuidV4,
    /// RFC 4648 standard-alphabet base64 encode, with padding.
    Base64Encode,
    /// RFC 4648 standard-alphabet base64 decode.
    Base64Decode,
    /// SHA-256 checksum (FIPS 180-4), lowercase hex.
    Sha256,
    /// FNV-1a 64-bit hash, lowercase hex (mirrors clipstore identity).
    Fnv1a64,
    /// Pretty-print JSON: 2-space indent, sorted keys, floats rejected.
    JsonPretty,
    /// Unix seconds to a human UTC timestamp.
    EpochToDate,
    /// A human UTC timestamp to unix seconds.
    DateToEpoch,
    /// ASCII lowercase.
    Lower,
    /// ASCII uppercase.
    Upper,
    /// ASCII title case, word boundaries at whitespace.
    TitleCase,
    /// Character, byte, word, and line counts.
    CountChars,
}

/// Typed transform failure. Never a panic: garbage in, error out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevUtilError {
    /// Base64 input length is not a multiple of four.
    Base64Length,
    /// A byte outside the standard alphabet (byte offset given).
    Base64Char { pos: usize },
    /// Padding somewhere other than the last one or two positions.
    Base64Padding,
    /// Decoded base64 bytes are not valid UTF-8, so there is no text to show.
    DecodedNotUtf8,
    /// JSON did not parse (includes the float-rejected case).
    Json(persist::ParseError),
    /// The epoch argument is not a whole number of seconds.
    InvalidEpoch,
    /// The epoch falls outside years 0000 to 9999.
    EpochOutOfRange,
    /// The date string does not match "YYYY-MM-DD[ HH:MM[:SS]]" or names
    /// a day that does not exist (like february 29 off a leap year).
    InvalidDate,
}

impl fmt::Display for DevUtilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DevUtilError::Base64Length => {
                write!(f, "base64 length must be a multiple of four")
            }
            DevUtilError::Base64Char { pos } => {
                write!(f, "invalid base64 character at offset {pos}")
            }
            DevUtilError::Base64Padding => write!(f, "misplaced base64 padding"),
            DevUtilError::DecodedNotUtf8 => {
                write!(f, "decoded bytes are not valid utf-8 text")
            }
            DevUtilError::Json(e) => write!(f, "json: {e}"),
            DevUtilError::InvalidEpoch => write!(f, "epoch must be whole seconds"),
            DevUtilError::EpochOutOfRange => {
                write!(f, "epoch outside years 0000 to 9999")
            }
            DevUtilError::InvalidDate => {
                write!(f, "expected YYYY-MM-DD[ HH:MM[:SS]] naming a real day")
            }
        }
    }
}

impl std::error::Error for DevUtilError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DevUtilError::Json(e) => Some(e),
            _ => None,
        }
    }
}

/// Execute one transform. `now_secs` is only read by [`DevUtil::EpochToDate`]
/// with an empty argument; `entropy` is only read by [`DevUtil::UuidV4`].
/// Injecting both keeps every call a pure function, so tests get
/// determinism for free.
pub fn run(
    util: DevUtil,
    input: &str,
    now_secs: u64,
    entropy: &[u8; 16],
) -> Result<String, DevUtilError> {
    match util {
        DevUtil::UuidV4 => Ok(uuid_v4(entropy)),
        DevUtil::Base64Encode => Ok(b64_encode(input.as_bytes())),
        DevUtil::Base64Decode => {
            let bytes = b64_decode(input)?;
            String::from_utf8(bytes).map_err(|_| DevUtilError::DecodedNotUtf8)
        }
        DevUtil::Sha256 => Ok(sha256_hex(input.as_bytes())),
        DevUtil::Fnv1a64 => Ok(format!("{:016x}", fnv1a64(input.as_bytes()))),
        DevUtil::JsonPretty => json_pretty(input),
        DevUtil::EpochToDate => epoch_to_date(input, now_secs),
        DevUtil::DateToEpoch => date_to_epoch(input).map(|secs| secs.to_string()),
        DevUtil::Lower => Ok(input.to_ascii_lowercase()),
        DevUtil::Upper => Ok(input.to_ascii_uppercase()),
        DevUtil::TitleCase => Ok(title_case(input)),
        DevUtil::CountChars => Ok(count_chars(input)),
    }
}

/// Recognize a launcher phrasing. The first word (case-insensitive)
/// selects the transform; the rest, trimmed, is the argument. `uuid`
/// takes no argument, `epoch` may omit its argument (meaning now), and
/// every other transform requires one. Anything unrecognized is `None`
/// so the launcher falls through to app search.
pub fn parse_command(input: &str) -> Option<(DevUtil, String)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (trimmed, ""),
    };
    let util = match head.to_ascii_lowercase().as_str() {
        "uuid" => DevUtil::UuidV4,
        "b64" | "base64" => DevUtil::Base64Encode,
        "unb64" => DevUtil::Base64Decode,
        "sha256" => DevUtil::Sha256,
        "fnv" => DevUtil::Fnv1a64,
        "json" => DevUtil::JsonPretty,
        "epoch" => DevUtil::EpochToDate,
        "date" => DevUtil::DateToEpoch,
        "upper" => DevUtil::Upper,
        "lower" => DevUtil::Lower,
        "title" => DevUtil::TitleCase,
        "count" => DevUtil::CountChars,
        _ => return None,
    };
    match util {
        // "uuid something" is probably a search, not a command.
        DevUtil::UuidV4 if !rest.is_empty() => None,
        // Bare "epoch" means "format the injected now".
        DevUtil::UuidV4 | DevUtil::EpochToDate => Some((util, rest.to_string())),
        // Everything else needs an argument to act on.
        _ if rest.is_empty() => None,
        _ => Some((util, rest.to_string())),
    }
}

// ---------------------------------------------------------------------------
// UUID v4
// ---------------------------------------------------------------------------

/// Format the injected entropy as an RFC 4122 version 4 UUID: version
/// bits in byte 6, variant bits in byte 8, lowercase hex, 8-4-4-4-12.
fn uuid_v4(entropy: &[u8; 16]) -> String {
    let mut b = *entropy;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Base64 (RFC 4648, standard alphabet, with padding)
// ---------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(char::from(B64_ALPHABET[usize::from(b0 >> 2)]));
        out.push(char::from(
            B64_ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))],
        ));
        if chunk.len() > 1 {
            out.push(char::from(
                B64_ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))],
            ));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(B64_ALPHABET[usize::from(b2 & 0x3f)]));
        } else {
            out.push('=');
        }
    }
    out
}

/// Map one alphabet byte back to its 6-bit value.
fn b64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn b64_decode(input: &str) -> Result<Vec<u8>, DevUtilError> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(DevUtilError::Base64Length);
    }
    // Padding may only be the last byte or the last two bytes.
    let pad = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 {
        return Err(DevUtilError::Base64Padding);
    }
    let data = &bytes[..bytes.len() - pad];
    if data.contains(&b'=') {
        return Err(DevUtilError::Base64Padding);
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut acc_bits = 0u32;
    for (pos, &byte) in data.iter().enumerate() {
        let value = b64_value(byte).ok_or(DevUtilError::Base64Char { pos })?;
        acc = (acc << 6) | u32::from(value);
        acc_bits += 6;
        if acc_bits >= 8 {
            acc_bits -= 8;
            out.push((acc >> acc_bits) as u8);
        }
    }
    // "x===" style inputs leave a lone 6-bit tail that encodes nothing.
    if acc_bits >= 6 {
        return Err(DevUtilError::Base64Padding);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------

/// The 64 round constants: fractional parts of the cube roots of the
/// first 64 primes, as specified.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 over `bytes`, rendered as 64 lowercase hex digits. Checksum
/// utility only; see the module docs for the non-claim on security.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Pad: append 0x80, zeros to 56 mod 64, then the bit length big-endian.
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (&k, &wi) in SHA256_K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// JSON pretty-printing
// ---------------------------------------------------------------------------

fn json_pretty(input: &str) -> Result<String, DevUtilError> {
    let value = persist::parse(input).map_err(DevUtilError::Json)?;
    let mut out = String::new();
    write_pretty(&value, 0, &mut out);
    Ok(out)
}

/// Walk a persist tree and emit it with 2-space indentation. Keys come
/// out sorted because the object representation is a `BTreeMap`; strings
/// use the same minimal escaping as the canonical codec.
fn write_pretty(value: &persist::Value, indent: usize, out: &mut String) {
    match value {
        persist::Value::Null => out.push_str("null"),
        persist::Value::Bool(true) => out.push_str("true"),
        persist::Value::Bool(false) => out.push_str("false"),
        persist::Value::Int(n) => out.push_str(&n.to_string()),
        persist::Value::Str(s) => write_json_string(s, out),
        persist::Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                push_indent(indent + 1, out);
                write_pretty(item, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push(']');
        }
        persist::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                push_indent(indent + 1, out);
                write_json_string(key, out);
                out.push_str(": ");
                write_pretty(item, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push('}');
        }
    }
}

fn push_indent(levels: usize, out: &mut String) {
    for _ in 0..levels {
        out.push_str("  ");
    }
}

/// Minimal JSON string escaping, mirroring the canonical codec: only the
/// quote, backslash, and control characters are escaped.
fn write_json_string(s: &str, out: &mut String) {
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

// ---------------------------------------------------------------------------
// Epoch and civil-date math (all integer, proleptic gregorian, UTC)
// ---------------------------------------------------------------------------

/// Days-from-epoch to (year, month, day), Howard Hinnant's civil_from_days
/// in pure integer arithmetic. Valid over the whole i64 day range we ever
/// feed it; output is clamped to a sane era by the callers.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days from the unix epoch; the inverse of
/// [`civil_from_days`].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap_year(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Unix seconds (or, for an empty argument, the injected now) rendered as
/// "YYYY-MM-DD HH:MM:SS UTC". Negative epochs work; years outside 0000
/// to 9999 are a typed error.
fn epoch_to_date(input: &str, now_secs: u64) -> Result<String, DevUtilError> {
    let trimmed = input.trim();
    let secs: i64 = if trimmed.is_empty() {
        i64::try_from(now_secs).map_err(|_| DevUtilError::EpochOutOfRange)?
    } else {
        trimmed
            .parse::<i64>()
            .map_err(|_| DevUtilError::InvalidEpoch)?
    };
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    if !(0..=9999).contains(&y) {
        return Err(DevUtilError::EpochOutOfRange);
    }
    Ok(format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    ))
}

/// Read exactly two ASCII digits at `at`.
fn two_digits(bytes: &[u8], at: usize) -> Result<i64, DevUtilError> {
    let hi = bytes[at];
    let lo = bytes[at + 1];
    if !hi.is_ascii_digit() || !lo.is_ascii_digit() {
        return Err(DevUtilError::InvalidDate);
    }
    Ok(i64::from(hi - b'0') * 10 + i64::from(lo - b'0'))
}

/// Strict "YYYY-MM-DD[ HH:MM[:SS]]" to unix seconds. Fixed widths, real
/// calendar validation (leap years included), UTC only.
fn date_to_epoch(input: &str) -> Result<i64, DevUtilError> {
    let bytes = input.trim().as_bytes();
    // The three legal shapes by length: date, date+hm, date+hms.
    if !matches!(bytes.len(), 10 | 16 | 19) {
        return Err(DevUtilError::InvalidDate);
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit) {
        return Err(DevUtilError::InvalidDate);
    }
    let y = bytes[..4]
        .iter()
        .fold(0i64, |acc, b| acc * 10 + i64::from(b - b'0'));
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(DevUtilError::InvalidDate);
    }
    let m = two_digits(bytes, 5)?;
    let d = two_digits(bytes, 8)?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(DevUtilError::InvalidDate);
    }
    let (mut hour, mut minute, mut second) = (0i64, 0i64, 0i64);
    if bytes.len() >= 16 {
        if bytes[10] != b' ' || bytes[13] != b':' {
            return Err(DevUtilError::InvalidDate);
        }
        hour = two_digits(bytes, 11)?;
        minute = two_digits(bytes, 14)?;
        if bytes.len() == 19 {
            if bytes[16] != b':' {
                return Err(DevUtilError::InvalidDate);
            }
            second = two_digits(bytes, 17)?;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(DevUtilError::InvalidDate);
        }
    }
    Ok(days_from_civil(y, m, d) * 86_400 + hour * 3600 + minute * 60 + second)
}

// ---------------------------------------------------------------------------
// Text transforms
// ---------------------------------------------------------------------------

/// ASCII title case: the first character after start-of-string or
/// whitespace is uppercased, everything else lowercased. Non-ASCII
/// passes through untouched.
fn title_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut at_word_start = true;
    for ch in input.chars() {
        if ch.is_whitespace() {
            at_word_start = true;
            out.push(ch);
        } else if at_word_start {
            out.push(ch.to_ascii_uppercase());
            at_word_start = false;
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

/// Summary counts: unicode scalar chars, utf-8 bytes, whitespace-split
/// words, and lines (a trailing newline does not add an empty line).
fn count_chars(input: &str) -> String {
    let chars = input.chars().count();
    let bytes = input.len();
    let words = input.split_whitespace().count();
    let lines = if input.is_empty() {
        0
    } else {
        input.lines().count()
    };
    format!("{chars} chars, {bytes} bytes, {words} words, {lines} lines")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTROPY: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];

    fn ok(util: DevUtil, input: &str) -> String {
        run(util, input, 0, &ENTROPY).unwrap_or_else(|e| panic!("{util:?} {input:?} failed: {e}"))
    }

    fn err(util: DevUtil, input: &str) -> DevUtilError {
        match run(util, input, 0, &ENTROPY) {
            Ok(s) => panic!("{util:?} {input:?} should fail, got {s:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn golden_uuid_v4_from_entropy() {
        // Byte 6 gets the version nibble, byte 8 the variant bits.
        assert_eq!(
            ok(DevUtil::UuidV4, ""),
            "00010203-0405-4607-8809-0a0b0c0d0e0f"
        );
        // All-ones entropy still yields valid version and variant fields.
        let uuid = run(DevUtil::UuidV4, "", 0, &[0xff; 16]).expect("uuid");
        assert_eq!(uuid, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let chars: Vec<char> = uuid.chars().collect();
        assert_eq!(chars[14], '4');
        assert!(matches!(chars[19], '8' | '9' | 'a' | 'b'));
    }

    #[test]
    fn golden_base64_rfc4648_vectors() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(ok(DevUtil::Base64Encode, plain), encoded);
            assert_eq!(ok(DevUtil::Base64Decode, encoded), plain);
        }
        // Non-ASCII round-trips through utf-8 bytes.
        assert_eq!(ok(DevUtil::Base64Encode, "schlüssel"), "c2NobMO8c3NlbA==");
        assert_eq!(ok(DevUtil::Base64Decode, "c2NobMO8c3NlbA=="), "schlüssel");
    }

    #[test]
    fn base64_decode_errors_are_typed() {
        assert_eq!(
            err(DevUtil::Base64Decode, "Zg="),
            DevUtilError::Base64Length
        );
        assert_eq!(
            err(DevUtil::Base64Decode, "Zm9?"),
            DevUtilError::Base64Char { pos: 3 }
        );
        assert_eq!(
            err(DevUtil::Base64Decode, "Z==="),
            DevUtilError::Base64Padding
        );
        assert_eq!(
            err(DevUtil::Base64Decode, "Z=g="),
            DevUtilError::Base64Padding
        );
        assert_eq!(
            err(DevUtil::Base64Decode, "===="),
            DevUtilError::Base64Padding
        );
        // 0xff alone is not utf-8 text.
        assert_eq!(
            err(DevUtil::Base64Decode, "/w=="),
            DevUtilError::DecodedNotUtf8
        );
    }

    #[test]
    fn golden_sha256_fips_vectors() {
        assert_eq!(
            ok(DevUtil::Sha256, ""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            ok(DevUtil::Sha256, "abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The two-block FIPS vector.
        assert_eq!(
            ok(
                DevUtil::Sha256,
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            ),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Exactly 55 bytes forces the tight one-block padding case; 56
        // forces the padding to spill into a second block.
        assert_eq!(
            ok(DevUtil::Sha256, &"a".repeat(55)),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            ok(DevUtil::Sha256, &"a".repeat(56)),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
    }

    #[test]
    fn golden_fnv1a64_vectors() {
        // Same published vectors clipstore locks; output is 16 hex digits.
        assert_eq!(ok(DevUtil::Fnv1a64, ""), "cbf29ce484222325");
        assert_eq!(ok(DevUtil::Fnv1a64, "a"), "af63dc4c8601ec8c");
        assert_eq!(ok(DevUtil::Fnv1a64, "foobar"), "85944171f73967e8");
    }

    #[test]
    fn golden_json_pretty() {
        let input =
            "{\"b\": 1, \"a\": [1, 2, {\"x\": null}], \"c\": {}, \"d\": [], \"e\": \"hi\\n\"}";
        let expected = concat!(
            "{\n",
            "  \"a\": [\n",
            "    1,\n",
            "    2,\n",
            "    {\n",
            "      \"x\": null\n",
            "    }\n",
            "  ],\n",
            "  \"b\": 1,\n",
            "  \"c\": {},\n",
            "  \"d\": [],\n",
            "  \"e\": \"hi\\n\"\n",
            "}"
        );
        assert_eq!(ok(DevUtil::JsonPretty, input), expected);
        // Scalars stand alone.
        assert_eq!(ok(DevUtil::JsonPretty, "  true "), "true");
        assert_eq!(ok(DevUtil::JsonPretty, "[]"), "[]");
    }

    #[test]
    fn json_pretty_rejects_floats_and_garbage() {
        assert!(matches!(
            err(DevUtil::JsonPretty, "{\"a\": 1.5}"),
            DevUtilError::Json(persist::ParseError::FloatRejected { .. })
        ));
        assert!(matches!(
            err(DevUtil::JsonPretty, "not json"),
            DevUtilError::Json(_)
        ));
        assert!(matches!(
            err(DevUtil::JsonPretty, ""),
            DevUtilError::Json(persist::ParseError::UnexpectedEof)
        ));
    }

    #[test]
    fn golden_epoch_to_date() {
        assert_eq!(ok(DevUtil::EpochToDate, "0"), "1970-01-01 00:00:00 UTC");
        assert_eq!(ok(DevUtil::EpochToDate, "-1"), "1969-12-31 23:59:59 UTC");
        // The 2038 boundary: i32 rolls over, this does not.
        assert_eq!(
            ok(DevUtil::EpochToDate, "2147483647"),
            "2038-01-19 03:14:07 UTC"
        );
        assert_eq!(
            ok(DevUtil::EpochToDate, "2147483648"),
            "2038-01-19 03:14:08 UTC"
        );
        // Leap day handling, both a leap century and an ordinary leap year.
        assert_eq!(
            ok(DevUtil::EpochToDate, "951782400"),
            "2000-02-29 00:00:00 UTC"
        );
        assert_eq!(
            ok(DevUtil::EpochToDate, "1709164800"),
            "2024-02-29 00:00:00 UTC"
        );
        // The day after a leap day.
        assert_eq!(
            ok(DevUtil::EpochToDate, "1709251200"),
            "2024-03-01 00:00:00 UTC"
        );
    }

    #[test]
    fn epoch_empty_input_formats_the_injected_now() {
        let out = run(DevUtil::EpochToDate, "", 86_400, &ENTROPY).expect("epoch now");
        assert_eq!(out, "1970-01-02 00:00:00 UTC");
        let out = run(DevUtil::EpochToDate, "   ", 3_661, &ENTROPY).expect("epoch now");
        assert_eq!(out, "1970-01-01 01:01:01 UTC");
    }

    #[test]
    fn epoch_errors_are_typed() {
        assert_eq!(err(DevUtil::EpochToDate, "abc"), DevUtilError::InvalidEpoch);
        assert_eq!(err(DevUtil::EpochToDate, "1.5"), DevUtilError::InvalidEpoch);
        assert_eq!(
            err(DevUtil::EpochToDate, "99999999999999999999"),
            DevUtilError::InvalidEpoch
        );
        // Fits in i64 but lands outside years 0000 to 9999.
        assert_eq!(
            err(DevUtil::EpochToDate, "9223372036854775807"),
            DevUtilError::EpochOutOfRange
        );
        assert_eq!(
            err(DevUtil::EpochToDate, "-62167219201"),
            DevUtilError::EpochOutOfRange
        );
    }

    #[test]
    fn golden_date_to_epoch() {
        assert_eq!(ok(DevUtil::DateToEpoch, "1970-01-01"), "0");
        assert_eq!(ok(DevUtil::DateToEpoch, "1970-01-01 00:00:00"), "0");
        assert_eq!(
            ok(DevUtil::DateToEpoch, "2038-01-19 03:14:07"),
            "2147483647"
        );
        assert_eq!(ok(DevUtil::DateToEpoch, "2024-02-29"), "1709164800");
        assert_eq!(ok(DevUtil::DateToEpoch, "2000-02-29"), "951782400");
        // Minutes without seconds.
        assert_eq!(ok(DevUtil::DateToEpoch, "1970-01-01 01:01"), "3660");
        // Pre-epoch dates go negative.
        assert_eq!(ok(DevUtil::DateToEpoch, "1969-12-31 23:59:59"), "-1");
        assert_eq!(ok(DevUtil::DateToEpoch, "1969-12-31"), "-86400");
    }

    #[test]
    fn date_round_trips_through_epoch() {
        for secs in [
            0i64,
            1,
            59,
            3_661,
            86_399,
            86_400,
            951_782_400,
            1_709_164_800,
            2_147_483_647,
            -1,
            -86_400,
        ] {
            let rendered = ok(DevUtil::EpochToDate, &secs.to_string());
            let date = rendered.strip_suffix(" UTC").expect("utc suffix");
            assert_eq!(
                ok(DevUtil::DateToEpoch, date),
                secs.to_string(),
                "round trip through {rendered:?}"
            );
        }
    }

    #[test]
    fn date_errors_are_typed() {
        let bad = [
            "",
            "garbage",
            "2024-1-1",
            "2024/01/01",
            "2024-13-01",
            "2024-00-10",
            "2024-01-32",
            "2024-01-00",
            "2023-02-29",
            "1900-02-29",
            "2024-01-01T00:00",
            "2024-01-01 24:00",
            "2024-01-01 00:60",
            "2024-01-01 00:00:60",
            "2024-01-01 00:00:00 UTC",
            "20240101",
        ];
        for input in bad {
            assert_eq!(
                err(DevUtil::DateToEpoch, input),
                DevUtilError::InvalidDate,
                "input {input:?}"
            );
        }
        // 2000 was a leap year (divisible by 400), 1900 was not.
        assert!(run(DevUtil::DateToEpoch, "2000-02-29", 0, &ENTROPY).is_ok());
    }

    #[test]
    fn golden_case_transforms() {
        assert_eq!(ok(DevUtil::Upper, "hello World"), "HELLO WORLD");
        assert_eq!(ok(DevUtil::Lower, "Hello WORLD"), "hello world");
        assert_eq!(ok(DevUtil::TitleCase, "hello world"), "Hello World");
        assert_eq!(ok(DevUtil::TitleCase, "MANY WORDS here"), "Many Words Here");
        assert_eq!(ok(DevUtil::TitleCase, "it's fine"), "It's Fine");
        assert_eq!(ok(DevUtil::TitleCase, "  spaced   out "), "  Spaced   Out ");
        // Non-ASCII passes through untouched.
        assert_eq!(ok(DevUtil::Upper, "über"), "üBER");
        assert_eq!(ok(DevUtil::TitleCase, ""), "");
    }

    #[test]
    fn golden_count_chars() {
        assert_eq!(
            ok(DevUtil::CountChars, ""),
            "0 chars, 0 bytes, 0 words, 0 lines"
        );
        assert_eq!(
            ok(DevUtil::CountChars, "hello world"),
            "11 chars, 11 bytes, 2 words, 1 lines"
        );
        assert_eq!(
            ok(DevUtil::CountChars, "a\nb\nc"),
            "5 chars, 5 bytes, 3 words, 3 lines"
        );
        // A trailing newline does not add an empty line.
        assert_eq!(
            ok(DevUtil::CountChars, "a\nb\n"),
            "4 chars, 4 bytes, 2 words, 2 lines"
        );
        // Multibyte: 4 chars, more bytes.
        assert_eq!(
            ok(DevUtil::CountChars, "über"),
            "4 chars, 5 bytes, 1 words, 1 lines"
        );
    }

    #[test]
    fn golden_parse_command() {
        assert_eq!(
            parse_command("uuid"),
            Some((DevUtil::UuidV4, String::new()))
        );
        assert_eq!(
            parse_command("b64 hello"),
            Some((DevUtil::Base64Encode, "hello".to_string()))
        );
        assert_eq!(
            parse_command("base64 hello"),
            Some((DevUtil::Base64Encode, "hello".to_string()))
        );
        assert_eq!(
            parse_command("unb64 aGk="),
            Some((DevUtil::Base64Decode, "aGk=".to_string()))
        );
        assert_eq!(
            parse_command("sha256 abc"),
            Some((DevUtil::Sha256, "abc".to_string()))
        );
        assert_eq!(
            parse_command("fnv abc"),
            Some((DevUtil::Fnv1a64, "abc".to_string()))
        );
        assert_eq!(
            parse_command("json {\"a\":1}"),
            Some((DevUtil::JsonPretty, "{\"a\":1}".to_string()))
        );
        assert_eq!(
            parse_command("epoch 1700000000"),
            Some((DevUtil::EpochToDate, "1700000000".to_string()))
        );
        assert_eq!(
            parse_command("epoch"),
            Some((DevUtil::EpochToDate, String::new()))
        );
        assert_eq!(
            parse_command("date 2024-02-29"),
            Some((DevUtil::DateToEpoch, "2024-02-29".to_string()))
        );
        assert_eq!(
            parse_command("upper hi"),
            Some((DevUtil::Upper, "hi".to_string()))
        );
        assert_eq!(
            parse_command("lower HI"),
            Some((DevUtil::Lower, "HI".to_string()))
        );
        assert_eq!(
            parse_command("title hi there"),
            Some((DevUtil::TitleCase, "hi there".to_string()))
        );
        assert_eq!(
            parse_command("count one two"),
            Some((DevUtil::CountChars, "one two".to_string()))
        );
    }

    #[test]
    fn parse_command_is_forgiving_about_case_and_whitespace() {
        assert_eq!(
            parse_command("  SHA256   abc  "),
            Some((DevUtil::Sha256, "abc".to_string()))
        );
        assert_eq!(
            parse_command("UUID"),
            Some((DevUtil::UuidV4, String::new()))
        );
    }

    #[test]
    fn parse_command_falls_through_to_search() {
        // Unknown prefixes are not commands.
        assert_eq!(parse_command("safari"), None);
        assert_eq!(parse_command("uuidgen"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        // Commands that need an argument fall through without one.
        assert_eq!(parse_command("b64"), None);
        assert_eq!(parse_command("sha256"), None);
        assert_eq!(parse_command("upper"), None);
        // uuid with an argument is a search phrase, not a command.
        assert_eq!(parse_command("uuid please"), None);
    }

    #[test]
    fn nothing_panics_on_garbage() {
        let utils = [
            DevUtil::UuidV4,
            DevUtil::Base64Encode,
            DevUtil::Base64Decode,
            DevUtil::Sha256,
            DevUtil::Fnv1a64,
            DevUtil::JsonPretty,
            DevUtil::EpochToDate,
            DevUtil::DateToEpoch,
            DevUtil::Lower,
            DevUtil::Upper,
            DevUtil::TitleCase,
            DevUtil::CountChars,
        ];
        let nasties = [
            "",
            " ",
            "\u{0}",
            "====",
            "{\"a\":",
            "999999999999999999999999",
            "-",
            "日本語",
            "\\u0000",
            "0000-00-00 00:00:00",
            "\t\n\r",
        ];
        for util in utils {
            for input in nasties {
                // Ok or Err both fine; the assertion is "no panic".
                let _ = run(util, input, u64::MAX, &ENTROPY);
                let _ = parse_command(input);
            }
        }
    }

    #[test]
    fn run_is_deterministic() {
        for util in [DevUtil::UuidV4, DevUtil::Sha256, DevUtil::JsonPretty] {
            let a = run(util, "{\"k\": [1, 2]}", 42, &ENTROPY);
            let b = run(util, "{\"k\": [1, 2]}", 42, &ENTROPY);
            assert_eq!(a, b);
        }
    }
}
