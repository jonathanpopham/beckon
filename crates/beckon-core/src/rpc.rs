//! The beckon plugin protocol: JSON-RPC 2.0 over stdio.
//!
//! This module is the specification. A beckon plugin is any executable, in
//! any language, that reads JSON-RPC 2.0 requests from stdin (one per line)
//! and writes responses to stdout (one per line). No SDK, no linking, no
//! registration: drop an executable in `~/.beckon/plugins/` and beckon
//! speaks this protocol to it.
//!
//! This layer is pure: it encodes request lines and parses response lines
//! over the [`crate::persist`] codec, the only JSON codec in this codebase.
//! Process management (spawning, timeouts, blacklisting) lives in the host,
//! not here, so everything in this file tests on Linux CI.
//!
//! # Transport
//!
//! - beckon spawns the plugin on first use and keeps it running for the
//!   session. On beckon exit the plugin's stdin closes; exit on EOF.
//! - Each request is a single line of UTF-8 JSON terminated by `\n`.
//!   Each response is a single line of UTF-8 JSON terminated by `\n`.
//! - The plugin must answer every request, in order, echoing the integer
//!   `id` of the request. beckon always sends integer ids, so a response
//!   whose `id` is missing or not an integer is a protocol violation.
//! - stdout carries protocol lines ONLY. Diagnostics go to stderr; beckon
//!   forwards plugin stderr to its own stderr.
//! - Responses must not contain JSON float literals. beckon's codec stores
//!   integers only and rejects any float (see [`crate::persist`]).
//! - The host enforces a per-response timeout (2 seconds) and a per-line
//!   size cap (1 MiB). A plugin that misses either is disabled for the
//!   session.
//!
//! # Methods
//!
//! The protocol is versioned by the integer `protocol` field of the
//! manifest result. This module implements protocol version 1
//! ([`PROTOCOL_VERSION`]).
//!
//! ## beckon.manifest
//!
//! Sent once, immediately after spawn (the handshake). Params: `{}`.
//! Result: an object with these fields, all required:
//!
//! - `protocol` (integer): the protocol version the plugin speaks. Must
//!   be `1`.
//! - `name` (string, non-empty): the plugin's identity. Used in item ids,
//!   so keep it short and stable. Dots and whitespace are rewritten to
//!   `-` by the host.
//! - `version` (string): the plugin's own version, informational.
//! - `keyword` (string, non-empty): the trigger word. Typing
//!   `<keyword> <text>` in the launcher routes `<text>` to this plugin.
//! - `description` (string): one line describing the plugin.
//!
//! ```text
//! -> {"id":1,"jsonrpc":"2.0","method":"beckon.manifest","params":{}}
//! <- {"jsonrpc":"2.0","id":1,"result":{"protocol":1,"name":"demo","version":"1.0.0","keyword":"demo","description":"Echoes your query"}}
//! ```
//!
//! ## beckon.query
//!
//! Sent every time the user's input changes while the plugin's keyword is
//! active. Params: `{"query": string}` (the text after the keyword, which
//! may be empty). Result: `{"items": [...]}` where each item is an object
//! with:
//!
//! - `id` (string, non-empty, required): the plugin's identifier for this
//!   item, echoed back verbatim in `beckon.activate`.
//! - `title` (string, required): the row's main text.
//! - `subtitle` (string, optional, defaults to empty): the secondary line.
//!
//! ```text
//! -> {"id":2,"jsonrpc":"2.0","method":"beckon.query","params":{"query":"hello"}}
//! <- {"jsonrpc":"2.0","id":2,"result":{"items":[{"id":"echo","title":"Echo: hello","subtitle":"Copy to clipboard"}]}}
//! ```
//!
//! ## beckon.activate
//!
//! Sent when the user picks one of the plugin's items. Params:
//! `{"id": string}` (the item id the plugin returned from `beckon.query`).
//! Result: `{"action": ..., "value": ...}` where `action` is one of:
//!
//! - `"none"`: nothing further; `value` is not required.
//! - `"copy"`: beckon copies `value` (string, required) to the clipboard.
//! - `"paste"`: beckon copies `value` (string, required) and pastes it
//!   into the frontmost app.
//! - `"open"`: beckon opens `value` (string, required) as a URL or path.
//!
//! ```text
//! -> {"id":3,"jsonrpc":"2.0","method":"beckon.activate","params":{"id":"echo"}}
//! <- {"jsonrpc":"2.0","id":3,"result":{"action":"copy","value":"hello"}}
//! ```
//!
//! # Errors
//!
//! A plugin reports a per-request failure with a standard JSON-RPC 2.0
//! error response instead of a result:
//!
//! ```text
//! <- {"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}
//! ```
//!
//! `code` must be an integer and `message` a string. An error response is
//! a well-formed protocol exchange: the plugin stays alive. Garbage on
//! stdout, a missed timeout, or an oversized line is not: the host kills
//! and blacklists the plugin for the session.

use crate::persist::{self, ParseError, Value};
use std::collections::BTreeMap;
use std::fmt;

/// The plugin protocol version this module speaks. A manifest whose
/// `protocol` field differs is rejected with
/// [`RpcError::UnsupportedProtocol`].
pub const PROTOCOL_VERSION: i128 = 1;

/// A successfully parsed JSON-RPC response carrying a result.
///
/// Error responses never become an `RpcResponse`; they surface as
/// [`RpcError::Remote`] so the caller handles exactly one failure channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponse {
    /// The echoed request id. The caller checks it against the id it sent.
    pub id: i128,
    /// The `result` member, whatever shape the method defines.
    pub result: Value,
}

/// Everything that can go wrong turning a response line into a usable
/// result. Every malformed input is a typed variant; nothing panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The line is not valid JSON (or contains a float, which this
    /// codebase rejects by design).
    Json(ParseError),
    /// The line parsed but the top level is not an object.
    NotAnObject,
    /// The `jsonrpc` member is missing or is not the string `"2.0"`.
    BadVersion,
    /// The `id` member is missing or not an integer. beckon only ever
    /// sends integer ids, so anything else cannot match a request.
    BadId,
    /// Both `result` and `error` are present. JSON-RPC 2.0 requires
    /// exactly one.
    ResultAndError,
    /// Neither `result` nor `error` is present.
    NoResultOrError,
    /// The `error` member is not an object with integer `code` and
    /// string `message`.
    BadErrorShape,
    /// A well-formed JSON-RPC error response: the plugin reported a
    /// failure for this request. The plugin itself is fine.
    Remote {
        /// The echoed request id.
        id: i128,
        /// The plugin's error code.
        code: i128,
        /// The plugin's error message.
        message: String,
    },
    /// The manifest declares a protocol version this host does not speak.
    UnsupportedProtocol {
        /// The version the plugin declared.
        got: i128,
    },
    /// A result parsed as JSON but does not have the shape the method
    /// requires. The message says which field is wrong.
    Shape(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Json(e) => write!(f, "response is not valid JSON: {e}"),
            RpcError::NotAnObject => write!(f, "response is not a JSON object"),
            RpcError::BadVersion => {
                write!(f, "response is missing the jsonrpc \"2.0\" member")
            }
            RpcError::BadId => write!(f, "response id is missing or not an integer"),
            RpcError::ResultAndError => {
                write!(f, "response has both result and error members")
            }
            RpcError::NoResultOrError => {
                write!(f, "response has neither result nor error member")
            }
            RpcError::BadErrorShape => write!(
                f,
                "error member is not an object with integer code and string message"
            ),
            RpcError::Remote { id, code, message } => {
                write!(f, "plugin error {code} for request {id}: {message}")
            }
            RpcError::UnsupportedProtocol { got } => write!(
                f,
                "plugin speaks protocol {got}; this host speaks {PROTOCOL_VERSION}"
            ),
            RpcError::Shape(what) => write!(f, "result has the wrong shape: {what}"),
        }
    }
}

impl std::error::Error for RpcError {}

fn shape(what: &str) -> RpcError {
    RpcError::Shape(what.to_string())
}

/// Encode one JSON-RPC 2.0 request as a canonical single line, newline
/// terminated. Canonical means byte deterministic: keys sorted, no
/// whitespace, minimal escapes (see [`crate::persist`]).
pub fn encode_request(id: i128, method: &str, params: Value) -> String {
    let mut map = BTreeMap::new();
    map.insert("jsonrpc".to_string(), Value::Str("2.0".to_string()));
    map.insert("id".to_string(), Value::Int(id));
    map.insert("method".to_string(), Value::Str(method.to_string()));
    map.insert("params".to_string(), params);
    let mut line = Value::Object(map).to_canonical_string();
    line.push('\n');
    line
}

/// One string-valued params object, the shape every current method uses.
fn str_params(pairs: &[(&str, &str)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::Str((*v).to_string())))
            .collect(),
    )
}

/// The `beckon.manifest` handshake request (params `{}`).
pub fn manifest_request(id: i128) -> String {
    encode_request(id, "beckon.manifest", Value::Object(BTreeMap::new()))
}

/// A `beckon.query` request carrying the user's text after the keyword.
pub fn query_request(id: i128, query: &str) -> String {
    encode_request(id, "beckon.query", str_params(&[("query", query)]))
}

/// A `beckon.activate` request carrying the plugin's own item id.
pub fn activate_request(id: i128, item_id: &str) -> String {
    encode_request(id, "beckon.activate", str_params(&[("id", item_id)]))
}

/// Parse one response line. Enforces JSON-RPC 2.0: the `jsonrpc` marker,
/// an integer `id`, and exactly one of `result` or `error`. A well-formed
/// error response comes back as [`RpcError::Remote`]; every malformed
/// input is another typed [`RpcError`]. Never panics. The caller is
/// responsible for matching [`RpcResponse::id`] against the id it sent.
pub fn parse_response(line: &str) -> Result<RpcResponse, RpcError> {
    let value = persist::parse(line.trim()).map_err(RpcError::Json)?;
    let obj = value.as_object().ok_or(RpcError::NotAnObject)?;
    match obj.get("jsonrpc").and_then(Value::as_str) {
        Some("2.0") => {}
        _ => return Err(RpcError::BadVersion),
    }
    let id = obj
        .get("id")
        .and_then(Value::as_int)
        .ok_or(RpcError::BadId)?;
    match (obj.get("result"), obj.get("error")) {
        (Some(_), Some(_)) => Err(RpcError::ResultAndError),
        (None, None) => Err(RpcError::NoResultOrError),
        (Some(result), None) => Ok(RpcResponse {
            id,
            result: result.clone(),
        }),
        (None, Some(error)) => {
            let eo = error.as_object().ok_or(RpcError::BadErrorShape)?;
            let code = eo
                .get("code")
                .and_then(Value::as_int)
                .ok_or(RpcError::BadErrorShape)?;
            let message = eo
                .get("message")
                .and_then(Value::as_str)
                .ok_or(RpcError::BadErrorShape)?
                .to_string();
            Err(RpcError::Remote { id, code, message })
        }
    }
}

/// The decoded `beckon.manifest` result: the plugin's identity card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The protocol version the plugin speaks (always [`PROTOCOL_VERSION`]
    /// after a successful decode).
    pub protocol: i128,
    /// The plugin's stable name; the middle segment of every item id.
    pub name: String,
    /// The plugin's own version string, informational only.
    pub version: String,
    /// The trigger word that routes queries to this plugin.
    pub keyword: String,
    /// One line shown to the user describing the plugin.
    pub description: String,
}

/// Decode a `beckon.manifest` result. Unknown extra fields are ignored
/// for forward compatibility; missing or mistyped required fields are
/// [`RpcError::Shape`]; a protocol version other than
/// [`PROTOCOL_VERSION`] is [`RpcError::UnsupportedProtocol`].
pub fn decode_manifest(result: &Value) -> Result<Manifest, RpcError> {
    let obj = result
        .as_object()
        .ok_or_else(|| shape("manifest result is not an object"))?;
    let protocol = obj
        .get("protocol")
        .and_then(Value::as_int)
        .ok_or_else(|| shape("manifest is missing integer field \"protocol\""))?;
    if protocol != PROTOCOL_VERSION {
        return Err(RpcError::UnsupportedProtocol { got: protocol });
    }
    let field = |key: &str| -> Result<String, RpcError> {
        obj.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| shape(&format!("manifest is missing string field {key:?}")))
    };
    let name = field("name")?;
    if name.is_empty() {
        return Err(shape("manifest field \"name\" is empty"));
    }
    let keyword = field("keyword")?;
    if keyword.is_empty() {
        return Err(shape("manifest field \"keyword\" is empty"));
    }
    Ok(Manifest {
        protocol,
        name,
        version: field("version")?,
        keyword,
        description: field("description")?,
    })
}

/// One decoded row from a `beckon.query` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryItem {
    /// The plugin's own id for this item, echoed back on activate.
    pub id: String,
    /// The row's main text.
    pub title: String,
    /// The secondary line; empty when the plugin omitted it.
    pub subtitle: String,
}

/// Decode a `beckon.query` result into its items. `subtitle` is optional
/// and defaults to empty; `id` (non-empty) and `title` are required.
pub fn decode_query_items(result: &Value) -> Result<Vec<QueryItem>, RpcError> {
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| shape("query result is missing array field \"items\""))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| shape("query item is not an object"))?;
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| shape("query item is missing string field \"id\""))?;
        if id.is_empty() {
            return Err(shape("query item field \"id\" is empty"));
        }
        let title = obj
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| shape("query item is missing string field \"title\""))?;
        let subtitle = match obj.get("subtitle") {
            None => "",
            Some(v) => v
                .as_str()
                .ok_or_else(|| shape("query item field \"subtitle\" is not a string"))?,
        };
        out.push(QueryItem {
            id: id.to_string(),
            title: title.to_string(),
            subtitle: subtitle.to_string(),
        });
    }
    Ok(out)
}

/// The decoded `beckon.activate` result: what beckon should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// The plugin handled everything itself; beckon does nothing.
    None,
    /// Copy the payload to the clipboard.
    Copy(String),
    /// Copy the payload and paste it into the frontmost app.
    Paste(String),
    /// Open the payload as a URL or path.
    Open(String),
}

/// Decode a `beckon.activate` result. `action` selects the variant;
/// every action except `"none"` requires a string `value`.
pub fn decode_activation(result: &Value) -> Result<Activation, RpcError> {
    let obj = result
        .as_object()
        .ok_or_else(|| shape("activate result is not an object"))?;
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| shape("activate result is missing string field \"action\""))?;
    let value = || -> Result<String, RpcError> {
        obj.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                shape(&format!(
                    "action {action:?} requires string field \"value\""
                ))
            })
    };
    match action {
        "none" => Ok(Activation::None),
        "copy" => Ok(Activation::Copy(value()?)),
        "paste" => Ok(Activation::Paste(value()?)),
        "open" => Ok(Activation::Open(value()?)),
        other => Err(shape(&format!("unknown action {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- request encoding goldens ------------------------------------
    // These strings ARE the spec examples in the module docs. A plugin
    // author can copy them verbatim as test vectors.

    #[test]
    fn golden_manifest_request() {
        assert_eq!(
            manifest_request(1),
            "{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"beckon.manifest\",\"params\":{}}\n"
        );
    }

    #[test]
    fn golden_query_request() {
        assert_eq!(
            query_request(2, "hello"),
            "{\"id\":2,\"jsonrpc\":\"2.0\",\"method\":\"beckon.query\",\
             \"params\":{\"query\":\"hello\"}}\n"
        );
        // Empty query is legal: the keyword alone is active.
        assert_eq!(
            query_request(7, ""),
            "{\"id\":7,\"jsonrpc\":\"2.0\",\"method\":\"beckon.query\",\
             \"params\":{\"query\":\"\"}}\n"
        );
    }

    #[test]
    fn golden_activate_request() {
        assert_eq!(
            activate_request(3, "echo"),
            "{\"id\":3,\"jsonrpc\":\"2.0\",\"method\":\"beckon.activate\",\
             \"params\":{\"id\":\"echo\"}}\n"
        );
    }

    #[test]
    fn requests_are_single_newline_terminated_lines() {
        for line in [
            manifest_request(1),
            query_request(2, "with \"quotes\" and\nnewline"),
            activate_request(3, "id"),
        ] {
            assert!(line.ends_with('\n'));
            // Exactly one newline: the terminator. Payload newlines are
            // escaped by the codec, so the line stays a single line.
            assert_eq!(line.matches('\n').count(), 1);
        }
    }

    #[test]
    fn encode_request_escapes_payload() {
        let line = query_request(1, "a\"b\\c\nd");
        assert_eq!(
            line,
            "{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"beckon.query\",\
             \"params\":{\"query\":\"a\\\"b\\\\c\\nd\"}}\n"
        );
    }

    // ---- response parsing --------------------------------------------

    #[test]
    fn parse_success_response() {
        let resp = parse_response("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}")
            .expect("parse");
        assert_eq!(resp.id, 1);
        assert_eq!(resp.result.get("ok").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_tolerates_whitespace_and_key_order() {
        // The parser is liberal in what it accepts: standard JSON with
        // any key order and surrounding whitespace.
        let resp = parse_response("  { \"result\" : 5 , \"id\" : 9 , \"jsonrpc\" : \"2.0\" }\r\n")
            .expect("parse");
        assert_eq!(resp.id, 9);
        assert_eq!(resp.result, Value::Int(5));
    }

    #[test]
    fn parse_error_response_is_remote() {
        let err = parse_response(
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32601,\
             \"message\":\"method not found\"}}",
        )
        .expect_err("error response");
        assert_eq!(
            err,
            RpcError::Remote {
                id: 2,
                code: -32601,
                message: "method not found".to_string(),
            }
        );
    }

    #[test]
    fn parse_rejects_garbage_lines() {
        assert!(matches!(
            parse_response("this is not json"),
            Err(RpcError::Json(_))
        ));
        assert!(matches!(parse_response(""), Err(RpcError::Json(_))));
        // Floats are rejected by the codec itself.
        assert!(matches!(
            parse_response("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":1.5}"),
            Err(RpcError::Json(ParseError::FloatRejected { .. }))
        ));
    }

    #[test]
    fn parse_rejects_non_objects() {
        assert_eq!(parse_response("[1,2,3]"), Err(RpcError::NotAnObject));
        assert_eq!(parse_response("42"), Err(RpcError::NotAnObject));
        assert_eq!(parse_response("\"hi\""), Err(RpcError::NotAnObject));
    }

    #[test]
    fn parse_rejects_bad_version_marker() {
        assert_eq!(
            parse_response("{\"id\":1,\"result\":1}"),
            Err(RpcError::BadVersion)
        );
        assert_eq!(
            parse_response("{\"jsonrpc\":\"1.0\",\"id\":1,\"result\":1}"),
            Err(RpcError::BadVersion)
        );
        assert_eq!(
            parse_response("{\"jsonrpc\":2,\"id\":1,\"result\":1}"),
            Err(RpcError::BadVersion)
        );
    }

    #[test]
    fn parse_rejects_bad_ids() {
        assert_eq!(
            parse_response("{\"jsonrpc\":\"2.0\",\"result\":1}"),
            Err(RpcError::BadId)
        );
        assert_eq!(
            parse_response("{\"jsonrpc\":\"2.0\",\"id\":\"one\",\"result\":1}"),
            Err(RpcError::BadId)
        );
        assert_eq!(
            parse_response("{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":1}"),
            Err(RpcError::BadId)
        );
    }

    #[test]
    fn parse_enforces_result_xor_error() {
        assert_eq!(
            parse_response(
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":1,\
                 \"error\":{\"code\":1,\"message\":\"m\"}}"
            ),
            Err(RpcError::ResultAndError)
        );
        assert_eq!(
            parse_response("{\"jsonrpc\":\"2.0\",\"id\":1}"),
            Err(RpcError::NoResultOrError)
        );
    }

    #[test]
    fn parse_rejects_malformed_error_members() {
        for line in [
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":\"boom\"}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"m\"}}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":1}}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":\"1\",\"message\":\"m\"}}",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":1,\"message\":2}}",
        ] {
            assert_eq!(
                parse_response(line),
                Err(RpcError::BadErrorShape),
                "line: {line}"
            );
        }
    }

    #[test]
    fn parse_never_panics_on_adversarial_input() {
        for line in [
            "{",
            "}",
            "null",
            "true",
            "-",
            "\"",
            "{\"jsonrpc\":",
            "\u{0}",
            "{\"jsonrpc\":\"2.0\",\"id\":99999999999999999999999999999999999999999,\"result\":1}",
        ] {
            let _ = parse_response(line);
        }
    }

    // ---- round trips ---------------------------------------------------

    #[test]
    fn request_lines_are_valid_canonical_json() {
        // Every emitted request parses back through the codec and is a
        // canonical fixed point.
        for line in [
            manifest_request(1),
            query_request(-5, "ünïcode 🎉"),
            activate_request(i128::MAX, "item"),
        ] {
            let trimmed = line.strip_suffix('\n').expect("newline terminated");
            let value = persist::parse(trimmed).expect("request parses");
            assert_eq!(value.to_canonical_string(), trimmed);
        }
    }

    // ---- manifest decoding ---------------------------------------------

    fn manifest_value(protocol: i128) -> Value {
        persist::parse(&format!(
            "{{\"protocol\":{protocol},\"name\":\"demo\",\"version\":\"1.0.0\",\
             \"keyword\":\"demo\",\"description\":\"Echoes your query\"}}"
        ))
        .expect("fixture parses")
    }

    #[test]
    fn decode_manifest_happy_path() {
        // This is the spec example manifest from the module docs.
        let m = decode_manifest(&manifest_value(1)).expect("decode");
        assert_eq!(
            m,
            Manifest {
                protocol: 1,
                name: "demo".to_string(),
                version: "1.0.0".to_string(),
                keyword: "demo".to_string(),
                description: "Echoes your query".to_string(),
            }
        );
    }

    #[test]
    fn decode_manifest_ignores_unknown_fields() {
        let v = persist::parse(
            "{\"protocol\":1,\"name\":\"n\",\"version\":\"v\",\"keyword\":\"k\",\
             \"description\":\"d\",\"future\":\"stuff\"}",
        )
        .expect("parse");
        assert!(decode_manifest(&v).is_ok());
    }

    #[test]
    fn decode_manifest_rejects_wrong_protocol() {
        assert_eq!(
            decode_manifest(&manifest_value(2)),
            Err(RpcError::UnsupportedProtocol { got: 2 })
        );
        assert_eq!(
            decode_manifest(&manifest_value(0)),
            Err(RpcError::UnsupportedProtocol { got: 0 })
        );
    }

    #[test]
    fn decode_manifest_rejects_missing_or_empty_fields() {
        for json in [
            "{}",
            "{\"protocol\":1}",
            "{\"protocol\":\"1\",\"name\":\"n\",\"version\":\"v\",\"keyword\":\"k\",\"description\":\"d\"}",
            "{\"protocol\":1,\"name\":\"\",\"version\":\"v\",\"keyword\":\"k\",\"description\":\"d\"}",
            "{\"protocol\":1,\"name\":\"n\",\"version\":\"v\",\"keyword\":\"\",\"description\":\"d\"}",
            "{\"protocol\":1,\"name\":\"n\",\"keyword\":\"k\",\"description\":\"d\"}",
            "{\"protocol\":1,\"name\":7,\"version\":\"v\",\"keyword\":\"k\",\"description\":\"d\"}",
        ] {
            let v = persist::parse(json).expect("fixture parses");
            assert!(
                matches!(decode_manifest(&v), Err(RpcError::Shape(_))),
                "should reject: {json}"
            );
        }
        assert!(matches!(
            decode_manifest(&Value::Int(1)),
            Err(RpcError::Shape(_))
        ));
    }

    // ---- query item decoding ---------------------------------------------

    #[test]
    fn decode_query_items_happy_path() {
        // The spec example query result from the module docs.
        let v = persist::parse(
            "{\"items\":[{\"id\":\"echo\",\"title\":\"Echo: hello\",\
             \"subtitle\":\"Copy to clipboard\"}]}",
        )
        .expect("parse");
        assert_eq!(
            decode_query_items(&v).expect("decode"),
            vec![QueryItem {
                id: "echo".to_string(),
                title: "Echo: hello".to_string(),
                subtitle: "Copy to clipboard".to_string(),
            }]
        );
    }

    #[test]
    fn decode_query_items_subtitle_defaults_to_empty() {
        let v = persist::parse("{\"items\":[{\"id\":\"a\",\"title\":\"A\"}]}").expect("parse");
        let items = decode_query_items(&v).expect("decode");
        assert_eq!(items[0].subtitle, "");
    }

    #[test]
    fn decode_query_items_empty_list_is_fine() {
        let v = persist::parse("{\"items\":[]}").expect("parse");
        assert_eq!(decode_query_items(&v).expect("decode"), vec![]);
    }

    #[test]
    fn decode_query_items_rejects_bad_shapes() {
        for json in [
            "{}",
            "{\"items\":7}",
            "{\"items\":[7]}",
            "{\"items\":[{\"title\":\"no id\"}]}",
            "{\"items\":[{\"id\":\"\",\"title\":\"empty id\"}]}",
            "{\"items\":[{\"id\":\"a\"}]}",
            "{\"items\":[{\"id\":\"a\",\"title\":\"A\",\"subtitle\":9}]}",
        ] {
            let v = persist::parse(json).expect("fixture parses");
            assert!(
                matches!(decode_query_items(&v), Err(RpcError::Shape(_))),
                "should reject: {json}"
            );
        }
    }

    // ---- activation decoding ---------------------------------------------

    #[test]
    fn decode_activation_all_actions() {
        let cases = [
            ("{\"action\":\"none\"}", Activation::None),
            (
                "{\"action\":\"copy\",\"value\":\"text\"}",
                Activation::Copy("text".to_string()),
            ),
            (
                "{\"action\":\"paste\",\"value\":\"text\"}",
                Activation::Paste("text".to_string()),
            ),
            (
                "{\"action\":\"open\",\"value\":\"https://example.com\"}",
                Activation::Open("https://example.com".to_string()),
            ),
        ];
        for (json, want) in cases {
            let v = persist::parse(json).expect("fixture parses");
            assert_eq!(decode_activation(&v).expect("decode"), want, "{json}");
        }
        // "none" with a stray value is still none: value is ignored.
        let v = persist::parse("{\"action\":\"none\",\"value\":\"x\"}").expect("parse");
        assert_eq!(decode_activation(&v).expect("decode"), Activation::None);
    }

    #[test]
    fn decode_activation_rejects_bad_shapes() {
        for json in [
            "{}",
            "{\"action\":7}",
            "{\"action\":\"copy\"}",
            "{\"action\":\"paste\"}",
            "{\"action\":\"open\"}",
            "{\"action\":\"copy\",\"value\":7}",
            "{\"action\":\"explode\"}",
        ] {
            let v = persist::parse(json).expect("fixture parses");
            assert!(
                matches!(decode_activation(&v), Err(RpcError::Shape(_))),
                "should reject: {json}"
            );
        }
    }

    // ---- full conversation: what a plugin author tests against ----------

    #[test]
    fn spec_example_conversation_round_trip() {
        // Handshake.
        assert_eq!(
            manifest_request(1),
            "{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"beckon.manifest\",\"params\":{}}\n"
        );
        let resp = parse_response(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol\":1,\"name\":\"demo\",\
             \"version\":\"1.0.0\",\"keyword\":\"demo\",\"description\":\"Echoes your query\"}}",
        )
        .expect("manifest response");
        assert_eq!(resp.id, 1);
        let manifest = decode_manifest(&resp.result).expect("manifest");
        assert_eq!(manifest.keyword, "demo");

        // Query.
        let resp = parse_response(
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"items\":[{\"id\":\"echo\",\
             \"title\":\"Echo: hello\",\"subtitle\":\"Copy to clipboard\"}]}}",
        )
        .expect("query response");
        assert_eq!(resp.id, 2);
        let items = decode_query_items(&resp.result).expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "echo");

        // Activate.
        let resp = parse_response(
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"action\":\"copy\",\"value\":\"hello\"}}",
        )
        .expect("activate response");
        assert_eq!(resp.id, 3);
        assert_eq!(
            decode_activation(&resp.result).expect("activation"),
            Activation::Copy("hello".to_string())
        );
    }
}
