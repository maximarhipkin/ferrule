//! MCP's Streamable HTTP transport, the client half: each JSON-RPC message
//! is a POST, answered with JSON or a short SSE stream. Only what tool
//! calls need — no server-initiated GET stream, no resumption.

use crate::client::PROTOCOL_VERSION;
use crate::error::McpError;
use ferrule_sandbox::Egress;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

const SESSION_HEADER: &str = "mcp-session-id";

pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    startup_timeout: Duration,
    session: AsyncMutex<Option<Session>>,
}

#[derive(Clone)]
struct Session {
    id: Option<String>,
    protocol: String,
}

impl HttpTransport {
    /// `headers` values may hold `${VAR}`, expanded through `lookup`.
    pub(crate) fn new(
        url: &str,
        headers: &HashMap<String, String>,
        lookup: impl Fn(&str) -> Option<String>,
        egress: Option<&Egress>,
        startup_timeout: Duration,
    ) -> Result<Self, McpError> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(McpError::Config(format!("`{url}` isn't an http(s) URL")));
        }
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let value = expand(value, &lookup)?;
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| McpError::Config(format!("header `{name}`: {e}")))?;
            let mut value = HeaderValue::from_str(&value)
                .map_err(|e| McpError::Config(format!("header `{name}`: {e}")))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let client = ferrule_tools::egress::client_builder(egress)
            .and_then(|b| b.build())
            .map_err(|e| McpError::Config(format!("HTTP client: {e}")))?;
        Ok(Self {
            client,
            url: url.to_string(),
            headers: map,
            startup_timeout,
            session: AsyncMutex::new(None),
        })
    }

    /// One request, answered within `timeout`. A session the server has
    /// forgotten (404) is started again, once.
    pub(crate) async fn request(
        &self,
        next_id: &AtomicU64,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        for attempt in 0..2 {
            let session = self.session(next_id).await?;
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            match self.exchange(&msg, Some(&session), id, timeout).await {
                Err(Expired) if attempt == 0 => {
                    *self.session.lock().await = None;
                }
                Err(Expired) => {
                    return Err(McpError::Http(
                        "the server keeps dropping the session".into(),
                    ))
                }
                Ok(result) => return result.map(|(value, _)| value),
            }
        }
        unreachable!("the loop returns on its second pass")
    }

    async fn session(&self, next_id: &AtomicU64) -> Result<Session, McpError> {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_ref() {
            return Ok(session.clone());
        }
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let init = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "ferrule", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let (resp, session_id) = self
            .exchange(&init, None, id, self.startup_timeout)
            .await
            .map_err(|Expired| McpError::Handshake("404 on initialize".into()))?
            .map_err(|e| McpError::Handshake(e.to_string()))?;
        let result =
            crate::client::extract_result(resp).map_err(|e| McpError::Handshake(e.to_string()))?;
        let session = Session {
            id: session_id,
            protocol: result
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION)
                .to_string(),
        };
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}});
        let sent = self
            .post(&note, Some(&session))
            .send()
            .await
            .map_err(|e| McpError::Handshake(e.to_string()))?;
        if !sent.status().is_success() {
            return Err(McpError::Handshake(format!(
                "notifications/initialized: HTTP {}",
                sent.status()
            )));
        }
        *guard = Some(session.clone());
        Ok(session)
    }

    fn post(&self, msg: &Value, session: Option<&Session>) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(msg);
        if let Some(session) = session {
            req = req.header("mcp-protocol-version", &session.protocol);
            if let Some(id) = &session.id {
                req = req.header(SESSION_HEADER, id);
            }
        }
        req
    }

    /// POST `msg` and wait for the response with `id`, plus the session id
    /// the server handed out, if any.
    async fn exchange(
        &self,
        msg: &Value,
        session: Option<&Session>,
        id: u64,
        timeout: Duration,
    ) -> Result<Result<(Value, Option<String>), McpError>, Expired> {
        let has_session = session.and_then(|s| s.id.as_ref()).is_some();
        let call = async {
            let resp = self.post(msg, session).send().await.map_err(http_error)?;
            let status = resp.status();
            if status == StatusCode::NOT_FOUND && has_session {
                return Ok(Err(Expired));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let body: String = body.chars().take(300).collect();
                return Err(McpError::Http(format!("HTTP {status}: {body}")));
            }
            let session_id = resp
                .headers()
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let is_sse = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.starts_with("text/event-stream"));
            let value = if is_sse {
                read_sse(resp, id).await?
            } else {
                let body: Value = resp.json().await.map_err(http_error)?;
                find_response(body, id).ok_or_else(|| {
                    McpError::Http(format!("no response with id {id} in the reply"))
                })?
            };
            Ok(Ok((value, session_id)))
        };
        match tokio::time::timeout(timeout, call).await {
            Ok(Ok(Ok(done))) => Ok(Ok(done)),
            Ok(Ok(Err(Expired))) => Err(Expired),
            Ok(Err(e)) => Ok(Err(e)),
            Err(_) => Ok(Err(McpError::Timeout)),
        }
    }
}

/// The server no longer knows our session.
struct Expired;

fn http_error(e: reqwest::Error) -> McpError {
    // reqwest hides the cause (a TLS failure, a refused connection) a
    // level down; that is the part worth reading.
    let mut text = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    McpError::Http(text)
}

/// The response to `id` in a JSON body: one message, or a batch.
fn find_response(body: Value, id: u64) -> Option<Value> {
    let is_ours =
        |m: &Value| m.get("id").and_then(Value::as_u64) == Some(id) && m.get("method").is_none();
    match body {
        Value::Array(items) => items.into_iter().find(is_ours),
        one if is_ours(&one) => Some(one),
        _ => None,
    }
}

/// Read an SSE stream until the event carrying the response to `id`.
/// Everything else on it (progress, logs, requests to us) is skipped.
async fn read_sse(mut resp: reqwest::Response, id: u64) -> Result<Value, McpError> {
    let mut buf = String::new();
    loop {
        while let Some(end) = buf.find("\n\n") {
            let event: String = buf.drain(..end + 2).collect();
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if data.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&data.join("\n")) {
                if let Some(found) = find_response(value, id) {
                    return Ok(found);
                }
            }
        }
        match resp.chunk().await.map_err(http_error)? {
            Some(chunk) => buf.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n")),
            None => return Err(McpError::ConnectionClosed),
        }
    }
}

/// `${VAR}` → its value. A variable that isn't there is an error, not an
/// empty string: a header without its token would only fail later, and
/// more confusingly.
fn expand(value: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, McpError> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| McpError::Config(format!("unclosed `${{` in `{value}`")))?;
        let name = &after[..end];
        let var = lookup(name).ok_or_else(|| {
            McpError::Config(format!(
                "${{{name}}} isn't set, or looks secret and isn't in [secrets]: \
                 put it there, bound to this server's host"
            ))
        })?;
        out.push_str(&var);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_expand_and_a_missing_one_is_an_error() {
        let lookup = |name: &str| (name == "TOKEN").then(|| "ph_123".to_string());
        assert_eq!(expand("Bearer ${TOKEN}", lookup).unwrap(), "Bearer ph_123");
        assert_eq!(expand("plain", lookup).unwrap(), "plain");
        assert!(expand("Bearer ${NOPE}", lookup).is_err());
        assert!(expand("Bearer ${TOKEN", lookup).is_err());
    }

    #[test]
    fn the_response_is_picked_out_of_a_batch_and_requests_are_not() {
        let body = json!([
            {"jsonrpc": "2.0", "id": 7, "method": "ping"},
            {"jsonrpc": "2.0", "id": 7, "result": {"ok": true}},
        ]);
        assert_eq!(find_response(body, 7).unwrap()["result"]["ok"], true);
        assert!(find_response(json!({"jsonrpc": "2.0", "id": 8, "result": {}}), 7).is_none());
    }
}
