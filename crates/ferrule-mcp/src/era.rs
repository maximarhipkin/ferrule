//! Two eras of MCP (wave C, `docs/m17-mcp-add.md` "MCP 2026-07-28"):
//! "legacy" servers (protocol 2025-11-25 and earlier) open with an
//! `initialize` handshake and, over HTTP, a session; "modern" ones
//! (2026-07-28) have neither, and every request carries its protocol
//! version and the client's identity and capabilities in `_meta`
//! ([Versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)).
//! ferrule speaks both, and finds out which a server speaks once: on stdio
//! by probing with `server/discover`, on HTTP by trying it and reading a
//! `4xx`'s body. Here: the pieces both transports share.

use crate::error::McpError;
use base64::Engine;
use serde_json::{json, Value};

/// The modern revision ferrule speaks.
pub(crate) const MODERN_VERSION: &str = "2026-07-28";

/// The JSON-RPC errors only a modern server sends (2026-07-28, changelog
/// "Minor changes" 12): `HeaderMismatch`, `MissingRequiredClientCapability`,
/// `UnsupportedProtocolVersion`. Any other error to a probe means a legacy
/// server.
pub(crate) const HEADER_MISMATCH: i64 = -32020;
pub(crate) const MISSING_CAPABILITY: i64 = -32021;
pub(crate) const UNSUPPORTED_VERSION: i64 = -32022;

/// Which era a server speaks; found once per server process (stdio) or
/// address (HTTP), as the spec asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Era {
    /// Per-request `_meta`, no handshake, no session: this version.
    Modern(String),
    /// `initialize`, then (over HTTP) a session.
    Legacy,
}

/// `params` with the modern `_meta` keys added (and any `_meta` it had
/// kept): the protocol version, ferrule's identity and capabilities (none).
pub(crate) fn with_meta(params: Value, version: &str) -> Value {
    let mut params = match params {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    let meta = params.entry("_meta").or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "io.modelcontextprotocol/protocolVersion".into(),
            json!(version),
        );
        meta.insert(
            "io.modelcontextprotocol/clientInfo".into(),
            json!({ "name": "ferrule", "version": env!("CARGO_PKG_VERSION") }),
        );
        meta.insert(
            "io.modelcontextprotocol/clientCapabilities".into(),
            json!({}),
        );
    }
    Value::Object(params)
}

/// The JSON-RPC error code of a response, if it is an error.
pub(crate) fn error_code(resp: &Value) -> Option<i64> {
    resp.get("error")?.get("code")?.as_i64()
}

/// What a `server/discover` probe's answer says about the server: a
/// `DiscoverResult` or a modern error means modern, anything else
/// legacy. The spec: the fallback "MUST NOT be keyed to one specific error
/// code". `Err` when the server is modern but can't be spoken to.
pub(crate) fn era_of(resp: &Value) -> Result<Era, McpError> {
    if let Some(versions) = resp
        .get("result")
        .and_then(|r| r.get("supportedVersions"))
        .and_then(Value::as_array)
    {
        return choose(versions);
    }
    match error_code(resp) {
        Some(UNSUPPORTED_VERSION) => {
            let supported = resp["error"]["data"]["supported"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            choose(&supported)
        }
        Some(code @ (HEADER_MISMATCH | MISSING_CAPABILITY)) => Err(McpError::Handshake(format!(
            "the server refused ferrule's request ({code}: {})",
            message(resp)
        ))),
        _ => Ok(Era::Legacy),
    }
}

/// A version both sides speak: the modern one if the server lists it,
/// else the handshake (legacy versions are dated before it).
fn choose(versions: &[Value]) -> Result<Era, McpError> {
    let versions: Vec<&str> = versions.iter().filter_map(Value::as_str).collect();
    if versions.contains(&MODERN_VERSION) {
        return Ok(Era::Modern(MODERN_VERSION.to_string()));
    }
    if versions.iter().any(|v| *v < MODERN_VERSION) {
        return Ok(Era::Legacy);
    }
    Err(McpError::Handshake(format!(
        "the server speaks MCP {}, and ferrule speaks {MODERN_VERSION} and 2025-06-18",
        versions.join(", ")
    )))
}

fn message(resp: &Value) -> String {
    resp["error"]["message"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// 2026-07-28's `resultType`: `"input_required"` is a server asking the
/// client for more (sampling, elicitation, roots: Multi Round-Trip
/// Requests), which ferrule doesn't give, as it never answered those
/// requests before. A missing one is `"complete"`.
pub(crate) fn complete(result: Value) -> Result<Value, McpError> {
    match result.get("resultType").and_then(Value::as_str) {
        Some("input_required") => Err(McpError::Rpc {
            code: MISSING_CAPABILITY,
            message: "the server asked for input ferrule doesn't give (sampling, elicitation \
                      or roots); nothing was done"
                .into(),
        }),
        _ => Ok(result),
    }
}

/// A value for an `Mcp-Name` or `Mcp-Param-*` header: as is when it is
/// plain printable ASCII without spaces at either end, else
/// `=?base64?…?=` of its UTF-8 (Streamable HTTP, "Value Encoding"). A
/// plain value shaped like that sentinel is encoded too.
pub(crate) fn header_value(v: &str) -> String {
    let plain = v.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && !v.starts_with(' ')
        && !v.ends_with(' ')
        && !(v.starts_with("=?base64?") && v.ends_with("?="));
    if plain {
        v.to_string()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(v.as_bytes())
        )
    }
}

/// One tool parameter mirrored into an `Mcp-Param-{name}` header
/// (`x-mcp-header` in its schema): the chain of `properties` keys to it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamHeader {
    pub name: String,
    pub path: Vec<String>,
}

/// The `x-mcp-header` parameters of a tool's `inputSchema`, checked as the
/// spec asks (Streamable HTTP, "Schema Extension"): a non-empty token, unique
/// regardless of case, on an integer, string or boolean property reached
/// only through `properties`. `Err` names what's wrong: the tool is then
/// left out of the list.
pub(crate) fn param_headers(schema: &Value) -> Result<Vec<ParamHeader>, String> {
    let mut out: Vec<ParamHeader> = Vec::new();
    walk(schema, true, &mut Vec::new(), &mut out)?;
    let mut seen = std::collections::HashSet::new();
    for h in &out {
        if !seen.insert(h.name.to_ascii_lowercase()) {
            return Err(format!("x-mcp-header `{}` is there twice", h.name));
        }
    }
    Ok(out)
}

fn walk(
    node: &Value,
    reachable: bool,
    path: &mut Vec<String>,
    out: &mut Vec<ParamHeader>,
) -> Result<(), String> {
    match node {
        Value::Array(items) => {
            for item in items {
                walk(item, false, path, out)?;
            }
        }
        Value::Object(map) => {
            if let Some(h) = map.get("x-mcp-header") {
                if !reachable || path.is_empty() {
                    return Err("an x-mcp-header outside a chain of `properties`".into());
                }
                let name = h.as_str().unwrap_or("");
                let token = !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
                if !token {
                    return Err(format!("x-mcp-header {h} isn't a header name"));
                }
                let primitive = matches!(
                    map.get("type").and_then(Value::as_str),
                    Some("integer" | "string" | "boolean")
                );
                if !primitive {
                    return Err(format!(
                        "x-mcp-header `{name}` is on a property that isn't an integer, string or boolean"
                    ));
                }
                out.push(ParamHeader {
                    name: name.to_string(),
                    path: path.clone(),
                });
            }
            for (key, value) in map {
                match key.as_str() {
                    "properties" => {
                        if let Some(props) = value.as_object() {
                            for (prop, sub) in props {
                                path.push(prop.clone());
                                walk(sub, reachable, path, out)?;
                                path.pop();
                            }
                        }
                    }
                    // Values, not schemas.
                    "x-mcp-header" | "enum" | "const" | "default" | "examples" | "required" => {}
                    _ => walk(value, false, path, out)?,
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The headers a call's `arguments` give, `(Mcp-Param-{name}, value)`. A
/// parameter that isn't there, or is null, sends none; a number that isn't
/// a safe integer, or any other non-primitive, none either.
pub(crate) fn param_values(headers: &[ParamHeader], arguments: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for h in headers {
        let mut at = arguments;
        for key in &h.path {
            at = match at.get(key) {
                Some(v) => v,
                None => &Value::Null,
            };
        }
        let text = match at {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => match n.as_i64() {
                Some(i) if i.unsigned_abs() < (1u64 << 53) => i.to_string(),
                _ => continue,
            },
            _ => continue,
        };
        out.push((format!("Mcp-Param-{}", h.name), header_value(&text)));
    }
    out
}
