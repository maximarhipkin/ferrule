//! `plugin.json`: what a plugin is, what tools it offers and what it may
//! reach. Parsing validates everything the host relies on later, so a
//! manifest that loads is one the host can enforce.

use crate::schema;
use crate::PluginError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The file name next to the `.wasm`.
pub const MANIFEST_FILE: &str = "plugin.json";

/// Host maxima: a manifest may ask for less, never more.
pub const MAX_FUEL: u64 = 10_000_000_000;
pub const MAX_MEMORY_MB: u32 = 256;
pub const MAX_TIMEOUT_SECS: u64 = 120;
pub const MAX_OUTPUT_CHARS: usize = 100_000;

pub const DEFAULT_FUEL: u64 = 1_000_000_000;
pub const DEFAULT_MEMORY_MB: u32 = 64;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// The module's file name, next to the manifest.
    pub wasm: String,
    /// Hex SHA-256 of the module.
    pub sha256: String,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(default = "empty_object")]
    pub parameters: Value,
    /// The plugin's claim; honoured only when the grants agree.
    #[serde(default)]
    pub read_only: bool,
    /// Every call needs the owner's approval (M19's gate).
    #[serde(default)]
    pub approval: bool,
}

fn empty_object() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default, skip_serializing_if = "Files::is_empty")]
    pub files: Files,
    #[serde(default, skip_serializing_if = "Http::is_empty")]
    pub http: Http,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub clock: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub random: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Workspace-relative directories; `"."` is the whole workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

impl Files {
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http {
    /// Exact hosts, or `*.example.com` for its subdomains.
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default = "get_only")]
    pub methods: Vec<String>,
}

impl Default for Http {
    fn default() -> Self {
        Self {
            domains: vec![],
            methods: get_only(),
        }
    }
}

fn get_only() -> Vec<String> {
    vec!["GET".into()]
}

impl Http {
    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }
    /// Whether `host` is one of the granted domains.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.domains.iter().any(|d| domain_covers(d, &host))
    }
    pub fn allows_method(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// `pattern` is a granted domain: exact, or `*.x` for any subdomain of `x`
/// (not `x` itself). `host` may be a pattern too, for the subset check.
fn domain_covers(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }
    match pattern.strip_prefix("*.") {
        Some(base) => {
            let host = host.strip_prefix("*.").unwrap_or(host);
            host.len() > base.len() && host.ends_with(&format!(".{base}"))
        }
        None => false,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_chars: Option<usize>,
}

/// The limits a call runs under: the manifest's, capped by the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub timeout: std::time::Duration,
    /// `None`: the session's tool output cap.
    pub output_chars: Option<usize>,
}

impl Limits {
    pub fn effective(&self) -> Effective {
        Effective {
            fuel: self.fuel.unwrap_or(DEFAULT_FUEL).min(MAX_FUEL),
            memory_bytes: self
                .memory_mb
                .unwrap_or(DEFAULT_MEMORY_MB)
                .min(MAX_MEMORY_MB) as usize
                * 1024
                * 1024,
            timeout: std::time::Duration::from_secs(
                self.timeout_secs
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .min(MAX_TIMEOUT_SECS),
            ),
            output_chars: self.output_chars.map(|c| c.min(MAX_OUTPUT_CHARS)),
        }
    }
}

impl Manifest {
    /// Parse and validate. Normalises what has one spelling (hash and
    /// domains lower case, methods upper case, directories without `./`).
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let mut m: Manifest = serde_json::from_str(text)
            .map_err(|e| PluginError::Manifest(format!("plugin.json: {e}")))?;
        m.normalise();
        m.validate()?;
        Ok(m)
    }

    fn normalise(&mut self) {
        self.sha256 = self.sha256.trim().to_ascii_lowercase();
        let caps = &mut self.capabilities;
        for dirs in [&mut caps.files.read, &mut caps.files.write] {
            for d in dirs.iter_mut() {
                *d = normalise_dir(d);
            }
            dirs.sort();
            dirs.dedup();
        }
        for d in caps.http.domains.iter_mut() {
            *d = d.trim().to_ascii_lowercase();
        }
        caps.http.domains.sort();
        caps.http.domains.dedup();
        for m in caps.http.methods.iter_mut() {
            *m = m.trim().to_ascii_uppercase();
        }
        caps.http.methods.sort();
        caps.http.methods.dedup();
        caps.secrets.sort();
        caps.secrets.dedup();
    }

    fn validate(&self) -> Result<(), PluginError> {
        let bad = |m: String| Err(PluginError::Manifest(m));
        validate_name(&self.name).map_err(PluginError::Manifest)?;
        if self.version.trim().is_empty() || self.version.len() > 64 {
            return bad("`version` must be 1–64 characters".into());
        }
        if self.description.chars().count() > 2_000 {
            return bad("`description` is over 2000 characters".into());
        }
        if self.wasm.is_empty()
            || !self.wasm.ends_with(".wasm")
            || self.wasm.contains(['/', '\\'])
            || self.wasm.starts_with('.')
        {
            return bad(format!(
                "`wasm` must be a file name next to plugin.json ending in .wasm, not `{}`",
                self.wasm
            ));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return bad("`sha256` must be 64 hex digits (the SHA-256 of the .wasm)".into());
        }
        if self.tools.is_empty() || self.tools.len() > 64 {
            return bad("a plugin offers 1–64 tools".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for t in &self.tools {
            if t.name.is_empty()
                || !t
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return bad(format!(
                    "tool name `{}` must be a-z, 0-9, `_` or `-`",
                    t.name
                ));
            }
            if tool_name(&self.name, &t.name).len() > 64 {
                return bad(format!(
                    "`{}` is over the 64 characters providers accept for a tool name",
                    tool_name(&self.name, &t.name)
                ));
            }
            if !seen.insert(&t.name) {
                return bad(format!("tool `{}` is listed twice", t.name));
            }
            if t.description.trim().is_empty() || t.description.chars().count() > 2_000 {
                return bad(format!(
                    "tool `{}`: `description` must be 1–2000 characters",
                    t.name
                ));
            }
            if t.parameters.get("type").and_then(Value::as_str) != Some("object") {
                return bad(format!(
                    "tool `{}`: `parameters` must be a schema with \"type\": \"object\"",
                    t.name
                ));
            }
            schema::check_supported(&t.parameters)
                .map_err(|e| PluginError::Manifest(format!("tool `{}`: {e}", t.name)))?;
        }
        self.capabilities
            .validate()
            .map_err(PluginError::Manifest)?;
        let l = &self.limits;
        if l.fuel == Some(0)
            || l.memory_mb == Some(0)
            || l.timeout_secs == Some(0)
            || l.output_chars == Some(0)
        {
            return bad("`limits` must be positive".into());
        }
        Ok(())
    }

    /// Hex SHA-256 of the manifest as parsed (normalised), so a change to
    /// anything in it, reformatting aside, shows.
    pub fn digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }

    /// The model-facing name of one of this plugin's tools.
    pub fn tool_name(&self, tool: &str) -> String {
        tool_name(&self.name, tool)
    }
}

impl ToolSpec {
    /// A digest of what the model is told and what the host decides from.
    pub fn digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// `plugin__<plugin>__<tool>`.
pub fn tool_name(plugin: &str, tool: &str) -> String {
    format!("plugin__{plugin}__{tool}")
}

/// The extension name rule (M13): 1–40 of `a-z0-9-_`, no `__`.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !name.contains("__");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "plugin name `{name}` must be 1–40 of a-z, 0-9, `-`, `_`, with no `__`"
        ))
    }
}

fn normalise_dir(d: &str) -> String {
    let d = d.trim().replace('\\', "/");
    // An absolute path stays absolute, for `validate` to refuse.
    if d.starts_with('/') {
        return d;
    }
    let parts: Vec<&str> = d
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

impl Capabilities {
    fn validate(&self) -> Result<(), String> {
        for d in self.files.read.iter().chain(&self.files.write) {
            if d.starts_with('/')
                || d.starts_with('~')
                || d.contains(':')
                || d.split('/').any(|p| p == "..")
            {
                return Err(format!(
                    "files: `{d}` must be a directory inside the workspace, relative to it"
                ));
            }
        }
        for d in &self.http.domains {
            let host = d.strip_prefix("*.").unwrap_or(d);
            if host.is_empty()
                || host.starts_with('.')
                || host.ends_with('.')
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            {
                return Err(format!(
                    "http: `{d}` must be a host name (`api.example.com`) or `*.example.com`, with no scheme, port or path"
                ));
            }
        }
        if self.http.domains.is_empty() && self.http.methods != get_only() {
            return Err("http: `methods` without `domains`".into());
        }
        if self.http.methods.is_empty() {
            return Err("http: `methods` is empty".into());
        }
        for m in &self.http.methods {
            if !METHODS.contains(&m.as_str()) {
                return Err(format!(
                    "http: method `{m}` isn't one of {}",
                    METHODS.join(", ")
                ));
            }
        }
        for s in &self.secrets {
            if s.is_empty()
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(format!(
                    "secrets: `{s}` must be an env-style name (A-Z, 0-9, `_`)"
                ));
            }
        }
        if !self.secrets.is_empty() && self.http.is_empty() {
            return Err(
                "secrets are only used in http headers, and there is no `http` capability".into(),
            );
        }
        Ok(())
    }

    /// Anything beyond clock and randomness: always the owner's decision.
    pub fn needs_owner(&self) -> bool {
        !self.files.is_empty() || !self.http.is_empty() || !self.secrets.is_empty()
    }

    /// Whether a call may write: a writable directory, or an HTTP method
    /// other than GET and HEAD.
    pub fn writes(&self) -> bool {
        !self.files.write.is_empty()
            || (!self.http.is_empty()
                && self.http.methods.iter().any(|m| m != "GET" && m != "HEAD"))
    }

    /// Everything in `self` is also granted by `granted`.
    pub fn is_subset_of(&self, granted: &Capabilities) -> bool {
        self.widening(granted).is_empty()
    }

    /// What `self` has that `granted` doesn't, in plain words.
    pub fn widening(&self, granted: &Capabilities) -> Vec<String> {
        let mut out = vec![];
        let under = |d: &str, dirs: &[String]| {
            dirs.iter()
                .any(|g| g == "." || d == g || d.starts_with(&format!("{g}/")))
        };
        let readable: Vec<String> = granted
            .files
            .read
            .iter()
            .chain(&granted.files.write)
            .cloned()
            .collect();
        for d in &self.files.read {
            if !under(d, &readable) {
                out.push(format!("read files under `{d}`"));
            }
        }
        for d in &self.files.write {
            if !under(d, &granted.files.write) {
                out.push(format!("write files under `{d}`"));
            }
        }
        for d in &self.http.domains {
            if !granted.http.domains.iter().any(|g| domain_covers(g, d)) {
                out.push(format!("HTTPS to {d}"));
            }
        }
        if !self.http.is_empty() {
            for m in &self.http.methods {
                if !granted.http.allows_method(m) {
                    out.push(format!("HTTP {m}"));
                }
            }
        }
        for s in &self.secrets {
            if !granted.secrets.contains(s) {
                out.push(format!("secret {s}"));
            }
        }
        if self.clock && !granted.clock {
            out.push("the clock".into());
        }
        if self.random && !granted.random {
            out.push("randomness".into());
        }
        out
    }

    /// One line per capability, for the owner's approval screen.
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec![];
        let dirs = |ds: &[String]| {
            ds.iter()
                .map(|d| match d.as_str() {
                    "." => "the whole workspace".to_string(),
                    d => format!("`{d}/`"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !self.files.read.is_empty() {
            out.push(format!("read files under {}", dirs(&self.files.read)));
        }
        if !self.files.write.is_empty() {
            out.push(format!(
                "read and write files under {}",
                dirs(&self.files.write)
            ));
        }
        if !self.http.is_empty() {
            out.push(format!(
                "HTTPS {} to {}",
                self.http.methods.join("/"),
                self.http.domains.join(", ")
            ));
        }
        for s in &self.secrets {
            out.push(format!(
                "secret {s} in request headers (sent as the proxy placeholder; the plugin never sees it)"
            ));
        }
        if self.clock {
            out.push("the current time".into());
        }
        if self.random {
            out.push("random bytes".into());
        }
        if out.is_empty() {
            out.push("nothing: pure computation".into());
        }
        out
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(caps: &str) -> String {
        format!(
            r#"{{"name":"conv","version":"0.1.0","wasm":"conv.wasm","sha256":"{}",
               "tools":[{{"name":"convert","description":"Convert units.",
                          "parameters":{{"type":"object","properties":{{"v":{{"type":"number"}}}}}}}}],
               "capabilities":{caps}}}"#,
            "AB".repeat(32)
        )
    }

    #[test]
    fn a_minimal_manifest_parses_and_normalises() {
        let m = Manifest::parse(&manifest(
            r#"{"files":{"read":["./docs/","."]},"http":{"domains":["API.example.com"],"methods":["get","post"]}}"#,
        ))
        .unwrap();
        assert_eq!(m.sha256, "ab".repeat(32));
        assert_eq!(m.capabilities.files.read, vec![".", "docs"]);
        assert_eq!(m.capabilities.http.domains, vec!["api.example.com"]);
        assert_eq!(m.capabilities.http.methods, vec!["GET", "POST"]);
        assert!(m.capabilities.writes(), "POST writes");
        assert!(m.capabilities.needs_owner());
        assert_eq!(m.tool_name("convert"), "plugin__conv__convert");
    }

    #[test]
    fn bad_capabilities_are_refused() {
        for caps in [
            r#"{"files":{"read":["../up"]}}"#,
            r#"{"files":{"write":["/etc"]}}"#,
            r#"{"files":{"read":["~/.ssh"]}}"#,
            r#"{"http":{"domains":["https://x.com"]}}"#,
            r#"{"http":{"domains":["x.com:443"]}}"#,
            r#"{"http":{"domains":["x.com"],"methods":["CONNECT"]}}"#,
            r#"{"secrets":["GITHUB_TOKEN"]}"#,
            r#"{"sockets":true}"#,
        ] {
            assert!(Manifest::parse(&manifest(caps)).is_err(), "{caps}");
        }
    }

    #[test]
    fn unsupported_schema_keywords_are_refused_at_parse() {
        let text = manifest("{}").replace(
            r#"{"type":"number"}"#,
            r#"{"type":"string","pattern":"^a"}"#,
        );
        let err = Manifest::parse(&text).unwrap_err().to_string();
        assert!(err.contains("pattern"), "{err}");
    }

    #[test]
    fn widening_is_named_and_narrowing_is_a_subset() {
        let granted = Capabilities {
            files: Files {
                read: vec!["docs".into()],
                write: vec![],
            },
            http: Http {
                domains: vec!["*.example.com".into()],
                methods: vec!["GET".into()],
            },
            ..Default::default()
        };
        let narrower = Capabilities {
            files: Files {
                read: vec!["docs/api".into()],
                write: vec![],
            },
            http: Http {
                domains: vec!["api.example.com".into()],
                methods: vec!["GET".into()],
            },
            ..Default::default()
        };
        assert!(narrower.is_subset_of(&granted));
        let wider = Capabilities {
            files: Files {
                read: vec!["src".into()],
                write: vec!["docs".into()],
            },
            http: Http {
                domains: vec!["example.com".into()],
                methods: vec!["GET".into(), "POST".into()],
            },
            clock: true,
            ..Default::default()
        };
        let w = wider.widening(&granted);
        assert_eq!(
            w,
            vec![
                "read files under `src`",
                "write files under `docs`",
                "HTTPS to example.com",
                "HTTP POST",
                "the clock"
            ]
        );
    }

    #[test]
    fn limits_are_capped_by_the_host() {
        let l = Limits {
            fuel: Some(u64::MAX),
            memory_mb: Some(10_000),
            timeout_secs: Some(10_000),
            output_chars: Some(usize::MAX),
        }
        .effective();
        assert_eq!(l.fuel, MAX_FUEL);
        assert_eq!(l.memory_bytes, MAX_MEMORY_MB as usize * 1024 * 1024);
        assert_eq!(l.timeout.as_secs(), MAX_TIMEOUT_SECS);
        assert_eq!(l.output_chars, Some(MAX_OUTPUT_CHARS));
    }
}
