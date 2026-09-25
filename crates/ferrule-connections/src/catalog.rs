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
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct File {
    service: Vec<Service>,
}

impl Service {
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
        match self.auth {
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
                "atlassian",
                "attio",
                "gmail",
                "gdrive",
                "github",
                "notion",
                "linear"
            ]
        );
        let gmail = cat.get("gmail").unwrap();
        assert_eq!(gmail.client, ClientKind::Owner);
        assert!(!gmail.resource_param);
        assert_eq!(
            gmail.scopes(false),
            ["https://www.googleapis.com/auth/gmail.readonly"]
        );
        assert_eq!(cat.get("linear").unwrap().scopes(true), ["read", "write"]);
        assert_eq!(cat.get("github").unwrap().auth, AuthKind::ApiKey);
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
        assert!(err.contains("known: atlassian"), "{err}");
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
