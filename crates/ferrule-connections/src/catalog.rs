//! The services ferrule knows how to connect: `catalog.toml`, compiled in,
//! plus the owner's `[[connections.custom]]` entries, plus any MCP URL.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const BUILT_IN: &str = include_str!("../catalog.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    Oauth,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// ferrule registers a client itself (RFC 7591).
    #[default]
    Dcr,
    /// A client the owner made; its id and secret are in the secrets file.
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnly {
    Scope,
    Header,
    Endpoint,
    /// M37: a native connection lists its write tools only when connected
    /// for writing.
    Tools,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// The MCP server's name: its tools are `mcp__<name>__*`.
    pub name: String,
    #[serde(default)]
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub auth: AuthKind,
    #[serde(default)]
    pub client: ClientKind,
    /// `client = "owner"`: `<client_env>_ID` and `<client_env>_SECRET`.
    #[serde(default)]
    pub client_env: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub write_scopes: Vec<String>,
    #[serde(default)]
    pub read_only: ReadOnly,
    /// `read_only = "endpoint"`: the URL used in write mode.
    #[serde(default)]
    pub write_url: Option<String>,
    /// Send RFC 8707's `resource` (the MCP URL). Google refuses it.
    #[serde(default = "yes")]
    pub resource_param: bool,
    #[serde(default)]
    pub authorize_url: Option<String>,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub revoke_url: Option<String>,
    #[serde(default)]
    pub extra_params: BTreeMap<String, String>,
    /// `auth = "api_key"`: the header the key goes in, and its value with
    /// `{key}` where the key goes.
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub header_value: Option<String>,
    /// Sent in read-only mode only (`read_only = "header"`).
    #[serde(default)]
    pub read_only_headers: BTreeMap<String, String>,
    /// Where the owner makes a key.
    #[serde(default)]
    pub key_url: Option<String>,
    #[serde(default)]
    pub docs: Option<String>,
    /// Shown on the button message.
    #[serde(default)]
    pub note: Option<String>,
    /// Said on disconnect when there's no revocation endpoint.
    #[serde(default)]
    pub revoke_note: Option<String>,
    /// M37: tools that run inside ferrule instead of on an MCP server:
    /// "jira", "gmail" or "google" (see `native`). `url` is then only a
    /// label.
    #[serde(default)]
    pub native: Option<String>,
    /// M37: what a key-based way in asks for. An `api_key` service with
    /// none asks for one secret `key`; an OAuth service with some also
    /// takes a key (Atlassian's API token, next to its sign-in).
    #[serde(default)]
    pub fields: Vec<Field>,
    /// M37: the Connections page's tile this is an option of (default: its
    /// name), the option's label, and what it covers.
    #[serde(default)]
    pub tile: Option<String>,
    #[serde(default)]
    pub option: Option<String>,
    #[serde(default)]
    pub covers: Option<String>,
    /// M37: the steps to follow, short enough for a phone.
    #[serde(default)]
    pub guide: Vec<String>,
    /// M37: the sign-in only works with a fixed callback address (the
    /// relay): a quick tunnel's address changes every run, and the service
    /// wants it registered.
    #[serde(default)]
    pub fixed_callback: bool,
    /// M37: a vendor preview the owner's project must be enrolled in.
    #[serde(default)]
    pub preview: bool,
}

/// One input of a key-based way in. A `secret` one is write-only: the page
/// never gets it back.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub secret: bool,
    /// "text", "email", "password", "textarea", "date".
    #[serde(default = "text")]
    pub kind: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
}

fn text() -> String {
    "text".into()
}

impl Field {
    fn key() -> Self {
        Self {
            name: "key".into(),
            label: "API key".into(),
            secret: true,
            kind: "password".into(),
            optional: false,
            hint: None,
            placeholder: None,
        }
    }
}

/// `header_value` with the fields filled in: `{name}` is a field, and
/// `{basic:a:b}` is `Basic base64(a:b)`, or `Bearer b` when `a` is empty
/// (Atlassian: a personal token with its email, or a service account's
/// key alone).
pub fn fill(template: &str, fields: &BTreeMap<String, String>) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &rest[open + 1..open + close];
        let get = |n: &str| fields.get(n).map(String::as_str).unwrap_or("");
        match inner.strip_prefix("basic:").and_then(|p| p.split_once(':')) {
            Some((a, b)) if !get(a).is_empty() => {
                let pair = format!("{}:{}", get(a), get(b));
                out.push_str("Basic ");
                out.push_str(&base64::engine::general_purpose::STANDARD.encode(pair));
            }
            Some((_, b)) => {
                out.push_str("Bearer ");
                out.push_str(get(b));
            }
            None => out.push_str(get(inner)),
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct File {
    service: Vec<Service>,
}

impl Service {
    /// The inputs its key-based way in asks for; none when it has none.
    pub fn key_fields(&self) -> Vec<Field> {
        match (self.auth, self.fields.is_empty()) {
            (_, false) => self.fields.clone(),
            (AuthKind::ApiKey, true) => vec![Field::key()],
            (AuthKind::Oauth, true) => Vec::new(),
        }
    }

    /// Takes a key (alone, or next to its sign-in).
    pub fn takes_key(&self) -> bool {
        !self.key_fields().is_empty()
    }

    pub fn tile(&self) -> &str {
        self.tile.as_deref().unwrap_or(&self.name)
    }

    pub fn title(&self) -> &str {
        if self.title.is_empty() {
            &self.name
        } else {
            &self.title
        }
    }

    pub fn scopes(&self, write: bool) -> &[String] {
        if write && !self.write_scopes.is_empty() {
            &self.write_scopes
        } else {
            &self.scopes
        }
    }

    pub fn url(&self, write: bool) -> &str {
        match (&self.write_url, write) {
            (Some(url), true) => url,
            _ => &self.url,
        }
    }

    /// Whether the read-only default actually restricts anything.
    pub fn has_read_only(&self) -> bool {
        self.read_only != ReadOnly::None
    }

    /// One sentence for the owner about what read-only means here.
    pub fn read_only_story(&self, write: bool) -> String {
        match (self.read_only, write) {
            (ReadOnly::None, _) => format!(
                "{} has no read-only mode: every change it makes asks you first.",
                self.title()
            ),
            (_, false) => "Read-only; `write` asks for more.".to_string(),
            (_, true) => "With write access; changes still ask you first.".to_string(),
        }
    }

    /// An entry for a bare MCP URL: OAuth by discovery and DCR, named after
    /// its host (`mcp.example.com` → `example`).
    pub fn from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url).context("not a URL")?;
        if parsed.scheme() != "https" && !is_loopback(&parsed) {
            bail!("an MCP server's URL must be https");
        }
        let host = parsed.host_str().context("a URL with a host")?;
        let labels: Vec<&str> = host
            .split('.')
            .filter(|l| !matches!(*l, "mcp" | "api" | "www"))
            .collect();
        let base = match labels.len() {
            0 => host,
            1 => labels[0],
            n => labels[n - 2],
        };
        let name: String = base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        Ok(Self {
            name,
            title: host.to_string(),
            url: url.to_string(),
            auth: AuthKind::Oauth,
            client: ClientKind::Dcr,
            client_env: None,
            scopes: Vec::new(),
            write_scopes: Vec::new(),
            read_only: ReadOnly::None,
            write_url: None,
            resource_param: true,
            authorize_url: None,
            token_url: None,
            revoke_url: None,
            extra_params: BTreeMap::new(),
            header: None,
            header_value: None,
            read_only_headers: BTreeMap::new(),
            key_url: None,
            docs: None,
            note: None,
            revoke_note: None,
            native: None,
            fields: Vec::new(),
            tile: None,
            option: None,
            covers: None,
            guide: Vec::new(),
            fixed_callback: false,
            preview: false,
        })
    }

    fn check(&self) -> Result<()> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            bail!(
                "service name `{}`: lowercase letters, digits, - and _",
                self.name
            );
        }
        let url = url::Url::parse(&self.url).with_context(|| format!("{}: url", self.name))?;
        if url.scheme() != "https" && !is_loopback(&url) {
            bail!("{}: the url must be https", self.name);
        }
        if let Some(n) = &self.native {
            if !crate::native::KINDS.contains(&n.as_str()) {
                bail!("{}: native = \"{n}\" isn't one ferrule has", self.name);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for f in &self.fields {
            if !seen.insert(f.name.as_str()) {
                bail!("{}: the field `{}` is there twice", self.name, f.name);
            }
        }
        match self.auth {
            AuthKind::ApiKey if self.native.is_some() => {}
            AuthKind::ApiKey if !self.fields.is_empty() => {
                let template = self.header_value.as_deref().unwrap_or("Bearer {key}");
                for name in placeholders(template) {
                    if !self.fields.iter().any(|f| f.name == name) {
                        bail!(
                            "{}: header_value uses `{name}`, which isn't a field",
                            self.name
                        );
                    }
                }
            }
            AuthKind::ApiKey => {
                if !self
                    .header_value
                    .as_deref()
                    .unwrap_or("Bearer {key}")
                    .contains("{key}")
                {
                    bail!("{}: header_value needs {{key}}", self.name);
                }
            }
            AuthKind::Oauth => {
                if self.client == ClientKind::Owner
                    && (self.client_env.is_none()
                        || self.authorize_url.is_none()
                        || self.token_url.is_none())
                {
                    bail!(
                        "{}: client = \"owner\" needs client_env, authorize_url and token_url",
                        self.name
                    );
                }
            }
        }
        Ok(())
    }
}

/// The field names `template` refers to (`{a}`, `{basic:a:b}`).
fn placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let inner = &rest[open + 1..open + close];
        match inner.strip_prefix("basic:") {
            Some(pair) => out.extend(pair.split(':')),
            None => out.push(inner),
        }
        rest = &rest[open + close + 1..];
    }
    out
}

pub(crate) fn is_loopback(url: &url::Url) -> bool {
    url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]")
        )
}

/// The built-in services, then `custom` (a custom entry replaces a
/// built-in one of the same name).
#[derive(Debug, Clone)]
pub struct Catalog {
    services: Vec<Service>,
}

impl Catalog {
    pub fn built_in() -> Self {
        let file: File = toml::from_str(BUILT_IN).expect("catalog.toml parses");
        Self {
            services: file.service,
        }
    }

    pub fn with_custom(custom: &[Service]) -> Result<Self> {
        let mut cat = Self::built_in();
        for s in custom {
            s.check()?;
            cat.services.retain(|b| b.name != s.name);
            cat.services.push(s.clone());
        }
        Ok(cat)
    }

    pub fn services(&self) -> &[Service] {
        &self.services
    }

    pub fn get(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }

    /// A name, or an MCP URL.
    pub fn resolve(&self, what: &str) -> Result<Service> {
        if what.starts_with("https://") || what.starts_with("http://") {
            if let Some(s) = self.services.iter().find(|s| s.url == what) {
                return Ok(s.clone());
            }
            return Service::from_url(what);
        }
        let name = what.to_ascii_lowercase();
        self.get(&name).cloned().with_context(|| {
            format!(
                "unknown service `{what}`; known: {}, or an https:// MCP URL",
                self.names().join(", ")
            )
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.services.iter().map(|s| s.name.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_catalog_parses_and_checks() {
        let cat = Catalog::built_in();
        for s in cat.services() {
            s.check().unwrap();
        }
        assert_eq!(
            cat.names(),
            [
                "jira",
                "atlassian_token",
                "atlassian",
                "attio",
                "gmail",
                "google",
                "google_oauth",
                "gmail_mcp",
                "gdrive_mcp",
                "github",
                "notion",
                "linear",
                "linear_key",
                "stripe",
                "huggingface",
                "sentry"
            ]
        );
        let gmail = cat.get("gmail_mcp").unwrap();
        assert_eq!(gmail.client, ClientKind::Owner);
        assert!(!gmail.resource_param);
        assert_eq!(
            gmail.scopes(false),
            ["https://www.googleapis.com/auth/gmail.readonly"]
        );
        assert_eq!(cat.get("linear").unwrap().scopes(true), ["read", "write"]);
        assert_eq!(cat.get("github").unwrap().auth, AuthKind::ApiKey);
        // M37: every tile's options, simplest first.
        let tile = |t: &str| -> Vec<String> {
            cat.services()
                .iter()
                .filter(|s| s.tile() == t)
                .map(|s| s.name.clone())
                .collect()
        };
        assert_eq!(tile("atlassian"), ["jira", "atlassian_token", "atlassian"]);
        assert_eq!(
            tile("google"),
            ["gmail", "google", "google_oauth", "gmail_mcp", "gdrive_mcp"]
        );
        let jira = cat.get("jira").unwrap();
        assert_eq!(jira.native.as_deref(), Some("jira"));
        assert!(jira
            .key_fields()
            .iter()
            .any(|f| f.name == "token" && f.secret));
        assert!(!jira
            .key_fields()
            .iter()
            .any(|f| f.name == "site" && f.secret));
        assert!(cat.get("atlassian").unwrap().fixed_callback);
        assert!(cat.get("gdrive_mcp").unwrap().preview);
        for s in cat.services() {
            assert!(
                s.covers.is_some() || s.tile.is_none(),
                "{} says what it covers",
                s.name
            );
        }
    }

    #[test]
    fn a_fields_template_must_name_its_fields() {
        let mut s = Catalog::built_in().get("atlassian_token").unwrap().clone();
        s.header_value = Some("{basic:mail:token}".into());
        assert!(Catalog::with_custom(&[s]).is_err());
    }

    #[test]
    fn a_url_resolves_to_a_dcr_entry_named_after_its_host() {
        let cat = Catalog::built_in();
        let s = cat.resolve("https://mcp.example.com/mcp").unwrap();
        assert_eq!(s.name, "example");
        assert_eq!(s.client, ClientKind::Dcr);
        assert_eq!(
            cat.resolve("https://mcp.linear.app/mcp").unwrap().name,
            "linear"
        );
        assert!(cat.resolve("http://example.com/mcp").is_err(), "https only");
        let err = cat.resolve("nope").unwrap_err().to_string();
        assert!(err.contains("known: jira, atlassian_token"), "{err}");
        assert_eq!(cat.resolve("Linear").unwrap().name, "linear");
    }

    #[test]
    fn a_custom_entry_replaces_a_built_in_one() {
        let mut mine = Catalog::built_in().get("linear").unwrap().clone();
        mine.url = "https://mcp.linear.app/mcp/readonly".into();
        let cat = Catalog::with_custom(&[mine]).unwrap();
        assert_eq!(
            cat.get("linear").unwrap().url,
            "https://mcp.linear.app/mcp/readonly"
        );
        let mut bad = Catalog::built_in().get("notion").unwrap().clone();
        bad.name = "Bad Name".into();
        assert!(Catalog::with_custom(&[bad]).is_err());
    }
}
