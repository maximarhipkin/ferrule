//! M37: connections whose tools run inside ferrule. Jira and Confluence
//! over Atlassian's REST API with an email and API token, Gmail over IMAP
//! and SMTP with an app password, and Drive/Sheets/Docs/Calendar over
//! Google's REST APIs with a service account's key (or an OAuth grant).
//! Each is a [`LocalServer`]: the MCP client lists and calls its tools like
//! any server's, gated and read-only-hinted the same way, and the
//! credential never leaves this process.

pub mod gmail;
pub mod google;
pub mod imap;
pub mod jira;
pub mod jwt;
pub mod mime;
pub mod net;
pub mod smtp;

use crate::catalog::Service;
use ferrule_mcp::local::LocalServer;
use ferrule_mcp::CredentialSource;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// `native = "…"` in the catalog.
pub const KINDS: &[&str] = &["jira", "gmail", "google"];

/// Where the native connections go. Tests point them at local mocks.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub google_token: String,
    /// One base for every Google API (`{base}/drive/v3/…`), for tests.
    pub google_base: Option<String>,
    pub imap: net::Addr,
    pub smtp: net::Addr,
    /// An `http://127.0.0.1:…` Atlassian site is accepted (tests only).
    pub loopback: bool,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            google_token: "https://oauth2.googleapis.com/token".into(),
            google_base: None,
            imap: net::Addr::tls("imap.gmail.com", 993),
            smtp: net::Addr::tls("smtp.gmail.com", 465),
            loopback: false,
        }
    }
}

/// What a native connection authenticates with.
#[derive(Clone)]
pub enum Cred {
    /// The fields the owner typed (sealed at rest).
    Fields(BTreeMap<String, String>),
    /// An OAuth grant's header source (Google's advanced option).
    Source(Arc<dyn CredentialSource>),
}

fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, String> {
    fields
        .get(name)
        .map(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("the {} is missing", name.replace('_', " ")))
}

/// Checks and tidies what the owner typed, before anything is tried: a
/// site becomes `https://<name>.atlassian.net`, an app password loses its
/// spaces, a service account's JSON must be one. The message says which
/// field is wrong and how.
pub fn normalize(
    kind: &str,
    fields: &mut BTreeMap<String, String>,
    ep: &Endpoints,
) -> Result<(), String> {
    for v in fields.values_mut() {
        *v = v.trim().to_string();
    }
    match kind {
        "jira" => {
            let site = jira::site_url(field(fields, "site")?, ep.loopback)?;
            fields.insert("site".into(), site);
            let email = field(fields, "email")?;
            if !email.contains('@') {
                return Err("the email should be the address you sign in to Atlassian with".into());
            }
            field(fields, "token")?;
            Ok(())
        }
        "gmail" => {
            let email = field(fields, "email")?.to_string();
            if !email.contains('@') {
                return Err("the email should be your Gmail address".into());
            }
            let password: String = field(fields, "app_password")?
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            if password.len() != 16 || !password.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(
                    "an app password is 16 letters (Google shows it in four groups of \
                     four; spaces are fine). Your normal Google password won't work here: \
                     make one at https://myaccount.google.com/apppasswords"
                        .into(),
                );
            }
            fields.insert("app_password".into(), password);
            Ok(())
        }
        "google" => {
            let key = google::ServiceAccount::parse(field(fields, "key")?)?;
            // The key is kept as it came; the email is shown, not secret.
            fields.insert("client_email".into(), key.email);
            Ok(())
        }
        other => Err(format!("`{other}` isn't a native connection")),
    }
}

/// The hosts a native connection's credential is ever sent to.
pub fn hosts(kind: &str, fields: &BTreeMap<String, String>, ep: &Endpoints) -> Vec<String> {
    match kind {
        "jira" => fields
            .get("site")
            .map(|s| vec![crate::explain::host_of(s)])
            .unwrap_or_default(),
        "gmail" => vec![ep.imap.host.clone(), ep.smtp.host.clone()],
        "google" => google::hosts(ep),
        _ => Vec::new(),
    }
}

/// Tries the credential once, the way the tools will use it: Jira's
/// `/myself`, an IMAP login, a service-account token. `Ok` says who it
/// is; `Err` is the exact, plain reason.
pub async fn probe(
    kind: &str,
    service: &Service,
    fields: &BTreeMap<String, String>,
    http: &reqwest::Client,
    ep: &Endpoints,
) -> Result<String, String> {
    match kind {
        "jira" => jira::Jira::new("probe", service.clone(), fields, false, http.clone())?
            .whoami()
            .await
            .map(|name| format!("signed in to Jira as {name}")),
        "gmail" => gmail::Gmail::new("probe", fields, false, ep.clone())?
            .check()
            .await
            .map(|email| format!("signed in to Gmail as {email}")),
        "google" => {
            let g = google::Google::new(
                "probe",
                service.clone(),
                &Cred::Fields(fields.clone()),
                false,
                http.clone(),
                ep.clone(),
            )?;
            g.token().await?;
            Ok(format!(
                "the key works; share files, sheets and calendars with {} to use them",
                fields
                    .get("client_email")
                    .map_or("the service account", |s| s)
            ))
        }
        other => Err(format!("`{other}` isn't a native connection")),
    }
}

/// The server for a connection `id` (its record name).
pub fn server(
    kind: &str,
    id: &str,
    service: &Service,
    cred: Cred,
    write: bool,
    http: &reqwest::Client,
    ep: &Endpoints,
) -> Result<Arc<dyn LocalServer>, String> {
    let fields = match &cred {
        Cred::Fields(f) => Some(f),
        Cred::Source(_) => None,
    };
    Ok(match (kind, fields) {
        ("jira", Some(f)) => Arc::new(jira::Jira::new(
            id,
            service.clone(),
            f,
            write,
            http.clone(),
        )?),
        ("gmail", Some(f)) => Arc::new(gmail::Gmail::new(id, f, write, ep.clone())?),
        ("google", _) => Arc::new(google::Google::new(
            id,
            service.clone(),
            &cred,
            write,
            http.clone(),
            ep.clone(),
        )?),
        (other, _) => return Err(format!("`{other}` can't run with this credential")),
    })
}

/// A tool as MCP lists it.
pub(crate) fn tool(
    name: &str,
    description: &str,
    props: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
        "annotations": { "readOnlyHint": read_only, "destructiveHint": false },
    })
}

pub(crate) fn arg_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args[name]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("`{name}` is required"))
}

pub(crate) fn opt_str<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args[name].as_str().filter(|s| !s.trim().is_empty())
}

pub(crate) fn limit(args: &Value, default: u64, max: u64) -> u64 {
    args["limit"].as_u64().unwrap_or(default).clamp(1, max)
}

/// The most text a tool returns; a model reads the rest by asking again.
pub(crate) const MAX_TEXT: usize = 60_000;

pub(crate) fn cap(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[cut: the rest is longer than this answer allows]");
    }
    text
}

/// A path segment, percent-encoded.
pub(crate) fn seg(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// A REST answer: the JSON of a 2xx, or the plain reason.
pub(crate) async fn answer(
    service: &Service,
    resp: Result<reqwest::Response, reqwest::Error>,
    what: &str,
) -> Result<Value, String> {
    let resp = resp.map_err(|_| crate::explain::unreachable(service, what))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(crate::explain::http(service, status, &body).text);
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body)
        .map_err(|_| format!("{} sent an answer ferrule couldn't read", service.title()))
}
