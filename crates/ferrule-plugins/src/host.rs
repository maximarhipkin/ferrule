//! The host side of `ferrule.host_call`: every op a plugin can ask for,
//! each checked against the plugin's capabilities on every call. This is
//! the whole of a plugin's reach; there is no other import.

use crate::manifest::Capabilities;
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;

/// Caps on what one op moves.
pub const MAX_READ: u64 = 4 * 1024 * 1024;
pub const MAX_WRITE: usize = 4 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 1_000;
pub const MAX_BODY: usize = 2 * 1024 * 1024;
pub const MAX_RANDOM: usize = 1_024;

/// Headers a plugin may not set: they'd let it steer the proxy or the
/// connection rather than the request.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "transfer-encoding",
    "content-length",
    "te",
    "upgrade",
];

/// Everything an op needs, fixed for one call.
#[derive(Clone)]
pub struct Host {
    pub plugin: String,
    pub caps: Capabilities,
    pub workspace: PathBuf,
    /// The sandbox's read deny list (M26), refused even inside a grant.
    pub hidden: Vec<PathBuf>,
    /// Egress (proxy + CA) and `${NAME}` expansion, as for a sandboxed child.
    pub sandbox: Sandbox,
    /// For the `http` op, which is async underneath.
    pub runtime: tokio::runtime::Handle,
}

impl Host {
    /// Run one op. The reply is `{"ok": …}` or `{"error": "…"}`; a denied
    /// op is an error the plugin sees, not a trap.
    pub fn op(&self, request: &[u8], deadline: Instant) -> Value {
        let result = serde_json::from_slice::<Value>(request)
            .map_err(|e| format!("host_call: the request isn't JSON: {e}"))
            .and_then(|req| self.dispatch(&req, deadline));
        match result {
            Ok(v) => json!({"ok": v}),
            Err(e) => json!({"error": e}),
        }
    }

    fn dispatch(&self, req: &Value, deadline: Instant) -> Result<Value, String> {
        let op = req.get("op").and_then(Value::as_str).unwrap_or_default();
        let str_arg = |k: &str| {
            req.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{op}: `{k}` is required"))
        };
        match op {
            "read_file" => self.read_file(str_arg("path")?),
            "write_file" => self.write_file(str_arg("path")?, str_arg("content")?),
            "list_dir" => self.list_dir(req.get("path").and_then(Value::as_str).unwrap_or(".")),
            "now" => {
                if !self.caps.clock {
                    return Err("now: this plugin has no `clock` capability".into());
                }
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                Ok(json!(ms))
            }
            "random" => {
                if !self.caps.random {
                    return Err("random: this plugin has no `random` capability".into());
                }
                let len = req.get("len").and_then(Value::as_u64).unwrap_or(32) as usize;
                if len > MAX_RANDOM {
                    return Err(format!("random: at most {MAX_RANDOM} bytes"));
                }
                let mut buf = vec![0u8; len];
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf)
                    .map_err(|_| "random: the OS gave no randomness".to_string())?;
                Ok(json!(hex(&buf)))
            }
            "http" => self.http(req, deadline),
            "" => Err("host_call: `op` is required".into()),
            other => Err(format!("host_call: unknown op `{other}`")),
        }
    }

    /// `path` resolved like the file tools do (workspace, symlinks, the
    /// deny list), then inside one of `dirs`.
    fn path_in(&self, op: &str, path: &str, dirs: &[&String]) -> Result<PathBuf, String> {
        if dirs.is_empty() {
            return Err(format!("{op}: this plugin has no capability to do that"));
        }
        let resolved = ferrule_tools::fs_tools::resolve(&self.workspace, &self.hidden, path)
            .map_err(|e| format!("{op}: {}", tool_message(e)))?;
        let granted = dirs.iter().any(|d| {
            ferrule_tools::fs_tools::resolve(&self.workspace, &[], d)
                .is_ok_and(|g| resolved.starts_with(g))
        });
        if !granted {
            let list: Vec<&str> = dirs.iter().map(|d| d.as_str()).collect();
            return Err(format!(
                "{op}: `{path}` is outside the directories this plugin was granted ({})",
                list.join(", ")
            ));
        }
        Ok(resolved)
    }

    fn readable(&self) -> Vec<&String> {
        self.caps
            .files
            .read
            .iter()
            .chain(&self.caps.files.write)
            .collect()
    }

    fn read_file(&self, path: &str) -> Result<Value, String> {
        let p = self.path_in("read_file", path, &self.readable())?;
        let meta = std::fs::metadata(&p).map_err(|e| format!("read_file: `{path}`: {e}"))?;
        if meta.len() > MAX_READ {
            return Err(format!(
                "read_file: `{path}` is {} bytes, over the {MAX_READ}-byte cap",
                meta.len()
            ));
        }
        let bytes = std::fs::read(&p).map_err(|e| format!("read_file: `{path}`: {e}"))?;
        String::from_utf8(bytes)
            .map(Value::String)
            .map_err(|_| format!("read_file: `{path}` isn't UTF-8 text"))
    }

    fn write_file(&self, path: &str, content: &str) -> Result<Value, String> {
        let dirs: Vec<&String> = self.caps.files.write.iter().collect();
        let p = self.path_in("write_file", path, &dirs)?;
        if content.len() > MAX_WRITE {
            return Err(format!("write_file: over the {MAX_WRITE}-byte cap"));
        }
        ferrule_tools::fs_tools::write_atomic(&p, content.as_bytes())
            .map_err(|e| format!("write_file: `{path}`: {e}"))?;
        tracing::info!(target: "ferrule_plugins", plugin = %self.plugin, path, bytes = content.len(), "plugin wrote a file");
        Ok(json!(content.len()))
    }

    fn list_dir(&self, path: &str) -> Result<Value, String> {
        let p = self.path_in("list_dir", path, &self.readable())?;
        let mut out = vec![];
        let entries = std::fs::read_dir(&p).map_err(|e| format!("list_dir: `{path}`: {e}"))?;
        for entry in entries.flatten() {
            if out.len() >= MAX_ENTRIES {
                break;
            }
            // Hidden entries (the deny list) are left out, not just unreadable.
            let path = entry.path();
            let visible = path.to_str().is_some_and(|p| {
                ferrule_tools::fs_tools::resolve(&self.workspace, &self.hidden, p).is_ok()
            });
            if !visible {
                continue;
            }
            let dir = entry.file_type().is_ok_and(|t| t.is_dir());
            out.push(json!({"name": entry.file_name().to_string_lossy(), "dir": dir}));
        }
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(Value::Array(out))
    }

    fn http(&self, req: &Value, deadline: Instant) -> Result<Value, String> {
        let http = &self.caps.http;
        if http.is_empty() {
            return Err("http: this plugin has no `http` capability".into());
        }
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !http.allows_method(&method) {
            return Err(format!(
                "http: {method} isn't granted to this plugin (granted: {})",
                http.methods.join(", ")
            ));
        }
        let url_text = req
            .get("url")
            .and_then(Value::as_str)
            .ok_or("http: `url` is required")?;
        let url = reqwest::Url::parse(url_text).map_err(|e| format!("http: bad url: {e}"))?;
        if url.scheme() != "https" {
            return Err("http: only https:// URLs".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("http: credentials in the URL aren't allowed; use a secret header".into());
        }
        let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
        if !http.allows_host(host) {
            return Err(format!(
                "http: `{host}` isn't one of this plugin's domains ({})",
                http.domains.join(", ")
            ));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(given) = req.get("headers") {
            let Some(given) = given.as_object() else {
                return Err("http: `headers` must be an object of strings".into());
            };
            for (name, value) in given {
                let lower = name.to_ascii_lowercase();
                if FORBIDDEN_HEADERS.contains(&lower.as_str()) {
                    return Err(format!(
                        "http: the `{name}` header can't be set by a plugin"
                    ));
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("http: header `{name}` must be a string"))?;
                let value = self.expand(value)?;
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("http: bad header name `{name}`"))?;
                let mut value = reqwest::header::HeaderValue::from_str(&value)
                    .map_err(|_| format!("http: bad value for header `{name}`"))?;
                value.set_sensitive(true);
                headers.insert(name, value);
            }
        }
        let body = req.get("body").and_then(Value::as_str).map(str::to_owned);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("http: the call is out of time".into());
        }
        let client = ferrule_tools::egress::client_builder(self.sandbox.egress())
            .map_err(|e| format!("http: {e}"))?
            // A redirect is the plugin's to follow, through this same check.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(remaining)
            .user_agent(concat!("ferrule-plugin/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http: {e}"))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("http: bad method `{method}`"))?;
        let mut request = client.request(method.clone(), url.clone()).headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        let plugin = self.plugin.clone();
        let host_name = host.to_string();
        let result = self.runtime.block_on(async move {
            let resp = request.send().await?;
            let status = resp.status().as_u16();
            let headers: serde_json::Map<String, Value> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), json!(v.to_str().ok()?))))
                .collect();
            let (body, truncated) = read_capped(resp).await?;
            Ok::<_, reqwest::Error>((status, headers, body, truncated))
        });
        let (status, headers, body, truncated) = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(target: "ferrule_plugins", plugin = %plugin, %method, host = %host_name, error = %e, "plugin http failed");
                return Err(format!("http: {}", without_url(&e)));
            }
        };
        tracing::info!(target: "ferrule_plugins", plugin = %plugin, %method, host = %host_name, status, "plugin http");
        Ok(json!({
            "status": status,
            "headers": headers,
            "body": String::from_utf8_lossy(&body),
            "truncated": truncated,
        }))
    }

    /// `${NAME}` in a header value: what a sandboxed child would see for
    /// `NAME`, only for a granted secret, and never ferrule's real value.
    fn expand(&self, value: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = value;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find('}')
                .ok_or("http: an unclosed `${` in a header")?;
            let name = &after[..end];
            if !self.caps.secrets.iter().any(|s| s == name) {
                return Err(format!(
                    "http: secret `{name}` isn't granted to this plugin"
                ));
            }
            if self.sandbox.egress().is_none() {
                return Err(format!(
                    "http: the credential proxy isn't running, so secret `{name}` can't be used (the plugin's request would carry the real value)"
                ));
            }
            let v = self.sandbox.child_env_var(name).ok_or_else(|| {
                format!("http: secret `{name}` isn't set up (`ferrule secrets` binds it to hosts)")
            })?;
            if std::env::var(name).is_ok_and(|real| real == v) {
                return Err(format!(
                    "http: secret `{name}` isn't bound in the credential proxy, so it would go out as its real value; bind it to this plugin's hosts first"
                ));
            }
            out.push_str(&v);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

async fn read_capped(mut resp: reqwest::Response) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = MAX_BODY - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_BODY {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

/// reqwest's error without the URL (which a plugin already knows, and
/// which must not echo anything the host added).
fn without_url(e: &reqwest::Error) -> String {
    let mut s = if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "couldn't connect".to_string()
    } else {
        "request failed".to_string()
    };
    let mut source = std::error::Error::source(e);
    while let Some(inner) = source {
        s = format!("{s}: {inner}");
        source = inner.source();
    }
    s
}

fn tool_message(e: ferrule_core::error::CoreError) -> String {
    match e {
        ferrule_core::error::CoreError::ToolFailed { message, .. } => message,
        other => other.to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
