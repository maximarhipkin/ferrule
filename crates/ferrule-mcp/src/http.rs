//! MCP's Streamable HTTP transport, the client half: each JSON-RPC message
//! is a POST, answered with JSON or a short SSE stream. Only what tool
//! calls need — no server-initiated GET stream, no resumption.
//!
//! Wave C: both eras (`crate::era`). The first request to an address is a
//! `server/discover` in the 2026-07-28 shape; a `DiscoverResult` or a
//! modern error means a modern server, spoken to with `_meta` on every
//! request and the `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name`
//! headers, no session; anything else (a `4xx` whose body isn't a modern
//! JSON-RPC error, a `-32601`, an empty answer) means a legacy one, opened
//! with `initialize` and a session, as before
//! ([Streamable HTTP, "Backward Compatibility"](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http#backward-compatibility)).

use crate::auth::Auth;
use crate::client::PROTOCOL_VERSION;
use crate::era::{self, Era};
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

/// Headers one request carries beyond the configured ones.
type Extra = Vec<(HeaderName, HeaderValue)>;

pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    startup_timeout: Duration,
    /// Which era the server speaks, found by the first request (wave C).
    era: AsyncMutex<Option<Era>>,
    /// A legacy server's session.
    session: AsyncMutex<Option<Session>>,
    /// Asked for a credential per request (a connection's token).
    auth: Option<Auth>,
}

/// A credential header as sent, and its value, to scrub from error text.
struct Credential {
    name: HeaderName,
    value: HeaderValue,
    raw: String,
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
        auth: Option<Auth>,
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
            era: AsyncMutex::new(None),
            session: AsyncMutex::new(None),
            auth,
        })
    }

    /// One request, answered within `timeout`. `params_headers` are a tool
    /// call's `Mcp-Param-*` headers, sent to a modern server. A legacy
    /// session the server has forgotten (404) is started again, once; a
    /// modern server that stops speaking ferrule's version is asked again,
    /// once.
    pub(crate) async fn request(
        &self,
        next_id: &AtomicU64,
        method: &str,
        params: Value,
        params_headers: &[(String, String)],
        timeout: Duration,
    ) -> Result<Value, McpError> {
        for attempt in 0..2 {
            match self.era(next_id).await? {
                Era::Legacy => return self.legacy(next_id, method, params, timeout).await,
                Era::Modern(version) => {
                    let resp = self
                        .modern(
                            next_id,
                            &version,
                            method,
                            params.clone(),
                            params_headers,
                            timeout,
                        )
                        .await?;
                    if attempt == 0 && era::error_code(&resp) == Some(era::UNSUPPORTED_VERSION) {
                        *self.era.lock().await = None;
                        continue;
                    }
                    return Ok(resp);
                }
            }
        }
        Err(McpError::Http(
            "the server keeps refusing ferrule's protocol version".into(),
        ))
    }

    /// The server's era, found once per address: concurrent first requests
    /// wait for the one probe.
    async fn era(&self, next_id: &AtomicU64) -> Result<Era, McpError> {
        let mut guard = self.era.lock().await;
        if let Some(era) = guard.as_ref() {
            return Ok(era.clone());
        }
        let era = self.probe(next_id).await?;
        tracing::debug!(url = %self.url, "mcp server era: {era:?}");
        *guard = Some(era.clone());
        Ok(era)
    }

    /// `server/discover` in the modern shape. A `DiscoverResult` or a
    /// modern error is a modern server; a `4xx` whose body is anything else,
    /// or an answer without either, a legacy one. A refused credential, a
    /// denial by the egress policy, a `5xx` or no answer is an error, not a
    /// guess.
    async fn probe(&self, next_id: &AtomicU64) -> Result<Era, McpError> {
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let method = "server/discover";
        let params = era::with_meta(json!({}), era::MODERN_VERSION);
        let headers = modern_headers(era::MODERN_VERSION, method, &params, &[])?;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let call = async {
            let (resp, raw) = self.send(&msg, &headers).await?;
            let status = resp.status();
            if ferrule_tools::egress::is_denial(&resp) {
                let why = resp.text().await.unwrap_or_default();
                return Err(McpError::Http(why.trim().to_string()));
            }
            if status.is_success() {
                let value = if is_sse(&resp) {
                    read_sse(resp, id).await?
                } else {
                    resp.json::<Value>()
                        .await
                        .ok()
                        .and_then(|body| find_response(body, id))
                        .unwrap_or(Value::Null)
                };
                return era::era_of(&value);
            }
            if status == StatusCode::UNAUTHORIZED && raw.is_some() {
                return Err(McpError::Http(format!(
                    "HTTP {status}: the service refused the connection's credential \
                     even after a refresh; the owner may need to reconnect it"
                )));
            }
            if status.is_client_error() {
                let body = resp.text().await.unwrap_or_default();
                let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                return era::era_of(&value);
            }
            Err(McpError::Http(format!("HTTP {status} to {method}")))
        };
        tokio::time::timeout(self.startup_timeout, call)
            .await
            .map_err(|_| McpError::Timeout)?
    }

    /// One request to a modern server: `_meta` in the body, the version,
    /// method and name in headers (Streamable HTTP, "Request Metadata").
    async fn modern(
        &self,
        next_id: &AtomicU64,
        version: &str,
        method: &str,
        params: Value,
        params_headers: &[(String, String)],
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let params = era::with_meta(params, version);
        let headers = modern_headers(version, method, &params, params_headers)?;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self
            .exchange(&msg, &headers, false, true, id, timeout)
            .await
        {
            Ok(result) => result.map(|(value, _)| value),
            Err(Expired) => Err(McpError::Http("the server dropped the request".into())),
        }
    }

    /// One request to a legacy server, in its session.
    async fn legacy(
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
            let headers = legacy_headers(&session)?;
            match self
                .exchange(&msg, &headers, session.id.is_some(), false, id, timeout)
                .await
            {
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
            .exchange(&init, &[], false, false, id, self.startup_timeout)
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
        let (sent, _) = self
            .send(&note, &legacy_headers(&session)?)
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

    /// POST `msg` with the current credential; on a 401, once more with a
    /// refreshed one. Also hands back the credential's value, which the
    /// caller scrubs from any error text built from the response.
    async fn send(
        &self,
        msg: &Value,
        extra: &[(HeaderName, HeaderValue)],
    ) -> Result<(reqwest::Response, Option<String>), McpError> {
        let Some(auth) = &self.auth else {
            let resp = self.post(msg, extra, None).send().await;
            return Ok((resp.map_err(http_error)?, None));
        };
        let cred = credential(auth.0.header().await)?;
        let resp = self
            .post(msg, extra, Some(&cred))
            .send()
            .await
            .map_err(|e| scrubbed(http_error(e), &cred.raw))?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok((resp, Some(cred.raw)));
        }
        let fresh = credential(auth.0.refreshed(&cred.raw).await)?;
        let resp = self
            .post(msg, extra, Some(&fresh))
            .send()
            .await
            .map_err(|e| scrubbed(http_error(e), &fresh.raw))?;
        Ok((resp, Some(fresh.raw)))
    }

    fn post(
        &self,
        msg: &Value,
        extra: &[(HeaderName, HeaderValue)],
        cred: Option<&Credential>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(msg);
        if let Some(cred) = cred {
            req = req.header(cred.name.clone(), cred.value.clone());
        }
        for (name, value) in extra {
            req = req.header(name.clone(), value.clone());
        }
        req
    }

    /// POST `msg` and wait for the response with `id`, plus the session id
    /// the server handed out, if any. `expiring`: a 404 means the session
    /// is gone. `rpc_errors`: a failed status whose body is a JSON-RPC
    /// error comes back as that error (a modern server's way).
    async fn exchange(
        &self,
        msg: &Value,
        extra: &[(HeaderName, HeaderValue)],
        expiring: bool,
        rpc_errors: bool,
        id: u64,
        timeout: Duration,
    ) -> Result<Result<(Value, Option<String>), McpError>, Expired> {
        let call = async {
            let (resp, raw) = self.send(msg, extra).await?;
            let status = resp.status();
            if status == StatusCode::NOT_FOUND && expiring {
                return Ok(Err(Expired));
            }
            if ferrule_tools::egress::is_denial(&resp) {
                let why = resp.text().await.unwrap_or_default();
                return Err(McpError::Http(why.trim().to_string()));
            }
            if !status.is_success() {
                if status == StatusCode::UNAUTHORIZED && raw.is_some() {
                    return Err(McpError::Http(format!(
                        "HTTP {status}: the service refused the connection's credential \
                         even after a refresh; the owner may need to reconnect it"
                    )));
                }
                let body = resp.text().await.unwrap_or_default();
                let body = match &raw {
                    Some(raw) => scrub(&body, raw),
                    None => body,
                };
                if rpc_errors {
                    if let Some(err) = serde_json::from_str::<Value>(&body)
                        .ok()
                        .filter(|v| v.get("error").is_some())
                    {
                        return Ok(Ok((err, None)));
                    }
                }
                let body: String = body.chars().take(300).collect();
                return Err(McpError::Http(format!("HTTP {status}: {body}")));
            }
            let session_id = resp
                .headers()
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let value = if is_sse(&resp) {
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

fn is_sse(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"))
}

fn header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), McpError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| McpError::Config(format!("header `{name}`: not a header name")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|_| McpError::Config(format!("header `{name}`: not a header value")))?;
    Ok((name, value))
}

/// A legacy session's headers: the negotiated version and its id.
fn legacy_headers(session: &Session) -> Result<Extra, McpError> {
    let mut out = vec![header("mcp-protocol-version", &session.protocol)?];
    if let Some(id) = &session.id {
        out.push(header(SESSION_HEADER, id)?);
    }
    Ok(out)
}

/// A modern request's headers: `MCP-Protocol-Version`, `Mcp-Method`, and
/// for `tools/call`, `prompts/get` and `resources/read` `Mcp-Name`, plus a
/// tool call's `Mcp-Param-*` (Streamable HTTP, "Standard Request Headers").
fn modern_headers(
    version: &str,
    method: &str,
    params: &Value,
    params_headers: &[(String, String)],
) -> Result<Extra, McpError> {
    let mut out = vec![
        header("mcp-protocol-version", version)?,
        header("mcp-method", method)?,
    ];
    let name = match method {
        "tools/call" | "prompts/get" => params.get("name").and_then(Value::as_str),
        "resources/read" => params.get("uri").and_then(Value::as_str),
        _ => None,
    };
    if let Some(name) = name {
        out.push(header("mcp-name", &era::header_value(name))?);
    }
    for (name, value) in params_headers {
        out.push(header(name, value)?);
    }
    Ok(out)
}

/// The server no longer knows our session.
struct Expired;

fn credential(got: Result<(String, String), String>) -> Result<Credential, McpError> {
    let (name, raw) = got.map_err(|e| McpError::Http(format!("credential: {e}")))?;
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| McpError::Config("credential: a bad header name".into()))?;
    let mut value = HeaderValue::from_str(&raw)
        .map_err(|_| McpError::Http("credential: not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(Credential { name, value, raw })
}

/// `text` with the credential `raw`, and the token after its `Bearer `,
/// taken out.
fn scrub(text: &str, raw: &str) -> String {
    let token = raw.rsplit(' ').next().unwrap_or(raw);
    if token.len() < 8 {
        return text.replace(raw, "[credential]");
    }
    text.replace(raw, "[credential]")
        .replace(token, "[credential]")
}

fn scrubbed(e: McpError, raw: &str) -> McpError {
    match e {
        McpError::Http(text) => McpError::Http(scrub(&text, raw)),
        other => other,
    }
}

fn http_error(e: reqwest::Error) -> McpError {
    // reqwest hides the cause (a TLS failure, a refused connection) a
    // level down; that is the part worth reading. The URL isn't: it can
    // carry a token, and this text reaches logs and the model.
    let e = e.without_url();
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
    // A character, or a CRLF, can be split across chunks: the bytes of an
    // unfinished character wait here for the rest of it.
    let mut raw: Vec<u8> = Vec::new();
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
            Some(chunk) => {
                raw.extend_from_slice(&chunk);
                let upto = match std::str::from_utf8(&raw) {
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    _ => raw.len(),
                };
                buf.push_str(&String::from_utf8_lossy(&raw[..upto]));
                raw.drain(..upto);
                if buf.contains('\r') {
                    buf = buf.replace("\r\n", "\n");
                }
            }
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
    fn a_credential_is_scrubbed_from_error_text() {
        let raw = "Bearer lin_oauth_abcdef123456";
        assert_eq!(
            scrub(
                "bad token lin_oauth_abcdef123456 for Bearer lin_oauth_abcdef123456",
                raw
            ),
            "bad token [credential] for [credential]"
        );
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
