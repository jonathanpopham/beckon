//! Path intent: recognizing that a query IS a filesystem path and
//! normalizing it, so the launcher can act on pasted locations like
//! `~/Documents/report.pdf`, `/Applications/Safari.app`, a shell-escaped
//! `~/My\ Folder`, a quoted path, or a `file://` URL copied from a
//! browser or Finder.
//!
//! This module is pure: no filesystem access, no env reads. The caller
//! injects the home directory; the shell decides what exists. Everything
//! here is deterministic and tested on Linux.

/// A query recognized as a path, normalized for filesystem use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathQuery {
    /// The absolute (or `.`-relative) path with tilde expanded, quotes
    /// stripped, shell escapes unescaped, and file:// percent-decoding
    /// applied.
    pub expanded: String,
    /// Whether the user ended with `/`, asking for the directory's
    /// contents rather than the directory itself.
    pub trailing_slash: bool,
}

/// Recognize and normalize a path-shaped query. Returns None for
/// everything that is not path-shaped; the caller falls through to the
/// normal pipeline. `home` is the user's home directory without a
/// trailing slash.
pub fn parse(query: &str, home: &str) -> Option<PathQuery> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unquoted = strip_quotes(trimmed);
    let decoded = if let Some(rest) = unquoted.strip_prefix("file://") {
        // file URLs may carry a localhost authority and are
        // percent-encoded.
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        if !path.starts_with('/') {
            return None;
        }
        percent_decode(path)
    } else {
        unescape_shell(&unquoted)
    };

    let is_pathlike = decoded.starts_with('/')
        || decoded == "~"
        || decoded.starts_with("~/")
        || decoded.starts_with("./")
        || decoded.starts_with("../");
    if !is_pathlike {
        return None;
    }

    let trailing_slash = decoded.len() > 1 && decoded.ends_with('/');
    let mut expanded = if decoded == "~" {
        home.to_string()
    } else if let Some(rest) = decoded.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        decoded
    };
    while expanded.len() > 1 && expanded.ends_with('/') {
        expanded.pop();
    }
    Some(PathQuery {
        expanded,
        trailing_slash,
    })
}

/// Strip one matching pair of surrounding single or double quotes, the
/// way a path pasted from a shell command line often arrives.
fn strip_quotes(s: &str) -> String {
    let b = s.as_bytes();
    if s.len() >= 2
        && (b[0] == b'"' && b[s.len() - 1] == b'"' || b[0] == b'\'' && b[s.len() - 1] == b'\'')
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Undo shell escaping: `\<char>` becomes `<char>` (covers `\ `, `\(`,
/// and friends from drag-and-drop or tab completion). A trailing lone
/// backslash is kept literally.
fn unescape_shell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Decode %XX sequences byte-wise; invalid sequences pass through
/// literally so a garbled URL still becomes a best-effort path.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Human size with one decimal digit, integer math only (tenths), for
/// the metadata subtitle: 0 B, 512 B, 1.5 KB, 3.2 MB, 1.0 GB.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1024 * 1024 * 1024),
        ("MB", 1024 * 1024),
        ("KB", 1024),
        ("B", 1),
    ];
    for (name, unit) in UNITS {
        if bytes >= unit {
            if unit == 1 {
                return format!("{bytes} B");
            }
            let tenths = bytes * 10 / unit;
            return format!("{}.{} {name}", tenths / 10, tenths % 10);
        }
    }
    "0 B".to_string()
}

/// Abbreviate the home directory back to `~` for display.
pub fn abbreviate_home(path: &str, home: &str) -> String {
    if path == home {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(home) {
        if rest.starts_with('/') {
            format!("~{rest}")
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

/// The kind word for the metadata subtitle, from the name and whether
/// the entry is a directory. `.app` bundles read as applications.
pub fn kind_word(name: &str, is_dir: bool) -> &'static str {
    if name.ends_with(".app") {
        "Application"
    } else if is_dir {
        "Folder"
    } else {
        "File"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/tester";

    fn expanded(q: &str) -> String {
        parse(q, HOME).expect("path-shaped").expanded
    }

    #[test]
    fn recognizes_path_shapes() {
        assert_eq!(expanded("~"), "/Users/tester");
        assert_eq!(expanded("~/Documents"), "/Users/tester/Documents");
        assert_eq!(expanded("/tmp/x"), "/tmp/x");
        assert_eq!(expanded("./rel"), "./rel");
        assert_eq!(expanded("../up"), "../up");
    }

    #[test]
    fn rejects_non_paths() {
        assert_eq!(parse("safari", HOME), None);
        assert_eq!(parse("2 + 2", HOME), None);
        assert_eq!(parse("clip foo", HOME), None);
        assert_eq!(parse("", HOME), None);
        assert_eq!(parse("   ", HOME), None);
        assert_eq!(parse("file://not-absolute", HOME), None);
    }

    #[test]
    fn trailing_slash_is_the_browse_signal() {
        let q = parse("~/geist/", HOME).unwrap();
        assert!(q.trailing_slash);
        assert_eq!(q.expanded, "/Users/tester/geist");
        let q = parse("~/geist", HOME).unwrap();
        assert!(!q.trailing_slash);
        // Root alone is not a trailing-slash browse of nothing.
        let q = parse("/", HOME).unwrap();
        assert_eq!(q.expanded, "/");
        assert!(!q.trailing_slash);
    }

    #[test]
    fn quotes_and_shell_escapes_unwrap() {
        assert_eq!(expanded("'/tmp/My Folder'"), "/tmp/My Folder");
        assert_eq!(expanded("\"~/Documents\""), "/Users/tester/Documents");
        assert_eq!(expanded("/tmp/My\\ Folder"), "/tmp/My Folder");
        assert_eq!(expanded("~/a\\(b\\)"), "/Users/tester/a(b)");
    }

    #[test]
    fn file_urls_decode() {
        assert_eq!(expanded("file:///tmp/x"), "/tmp/x");
        assert_eq!(expanded("file://localhost/tmp/x"), "/tmp/x");
        assert_eq!(
            expanded("file:///Users/tester/My%20Folder/a%2Bb.txt"),
            "/Users/tester/My Folder/a+b.txt"
        );
        // Invalid escapes pass through rather than erroring.
        assert_eq!(expanded("file:///tmp/100%zz"), "/tmp/100%zz");
    }

    #[test]
    fn sizes_format_with_integer_tenths() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 200 * 1024), "3.1 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn home_abbreviates_for_display() {
        assert_eq!(abbreviate_home("/Users/tester/x", HOME), "~/x");
        assert_eq!(abbreviate_home("/Users/tester", HOME), "~");
        assert_eq!(abbreviate_home("/tmp/x", HOME), "/tmp/x");
        assert_eq!(
            abbreviate_home("/Users/testerx/y", HOME),
            "/Users/testerx/y"
        );
    }

    #[test]
    fn kinds_classify() {
        assert_eq!(kind_word("Beckon.app", true), "Application");
        assert_eq!(kind_word("dist", true), "Folder");
        assert_eq!(kind_word("report.pdf", false), "File");
    }
}
