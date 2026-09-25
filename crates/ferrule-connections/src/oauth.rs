//! OAuth 2.1 for MCP servers: discovery (RFC 9728 → RFC 8414 / OIDC),
//! dynamic client registration (RFC 7591), PKCE S256, the code exchange,
//! refresh, and revocation (RFC 7009). Errors are fixed phrases plus the
//! OAuth `error` code: never a server's free text, never a token.

use crate::catalog::{ClientKind, Service};
use crate::seal::{b64, random};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// Where to send the owner, and where to trade codes and tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub registration: Option<String>,
    pub revocation: Option<String>,
}

/// A client, registered or the owner's.
#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug)]
pub enum TokenError {
    /// The server said no (`invalid_grant`, `invalid_client`, …): retrying
    /// won't help.
    Refused(String),
    /// Network, a 5xx, a reply that doesn't parse: maybe next time.
    Transient(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Refused(code) => write!(f, "refused ({code})"),
            TokenError::Transient(why) => write!(f, "{why}"),
        }
    }
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = b64(&random::<32>());
        let challenge =
            b64(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
        Self {
            verifier,
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("an HTTP client")
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let resp = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok().filter(Value::is_object)
}

/// `https://host/.well-known/<doc><path>`, the RFC 8414 / 9728 form.
fn well_known(base: &url::Url, doc: &str) -> String {
    let path = base.path().trim_end_matches('/');
    format!("{}/.well-known/{doc}{path}", origin(base))
}

fn origin(u: &url::Url) -> String {
    u.origin().ascii_serialization()
}

/// Find the authorization server for `service`: pinned endpoints, or the
/// MCP server's protected-resource metadata, then the AS's own metadata;
/// with neither, the MCP server's origin with the spec's default paths.
pub async fn discover(http: &reqwest::Client, service: &Service) -> Result<Endpoints> {
    if let (Some(a), Some(t)) = (&service.authorize_url, &service.token_url) {
        return Ok(Endpoints {
            authorize: a.clone(),
            token: t.clone(),
            registration: None,
            revocation: service.revoke_url.clone(),
        });
    }
    let mcp = url::Url::parse(&service.url).context("the service's URL")?;
    let mut issuer = None;
    for url in [
        well_known(&mcp, "oauth-protected-resource"),
        format!("{}/.well-known/oauth-protected-resource", origin(&mcp)),
    ] {
        if let Some(doc) = get_json(http, &url).await {
            issuer = doc["authorization_servers"][0].as_str().map(String::from);
            if issuer.is_some() {
                break;
            }
        }
    }
    let issuer = issuer.unwrap_or_else(|| origin(&mcp));
    let iss = url::Url::parse(&issuer).context("the authorization server's URL")?;
    if iss.scheme() != "https" && !crate::catalog::is_loopback(&iss) {
        bail!("the authorization server isn't https");
    }
    let mut meta = None;
    for url in [
        well_known(&iss, "oauth-authorization-server"),
        well_known(&iss, "openid-configuration"),
        format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        ),
    ] {
        if let Some(doc) = get_json(http, &url).await {
            meta = Some(doc);
            break;
        }
    }
    let base = origin(&iss);
    let Some(meta) = meta else {
        return Ok(Endpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            registration: Some(format!("{base}/register")),
            revocation: None,
        });
    };
    if let Some(methods) = meta["code_challenge_methods_supported"].as_array() {
        if !methods.iter().any(|m| m == "S256") {
            bail!("the authorization server doesn't support PKCE S256");
        }
    }
    let field = |k: &str| meta[k].as_str().map(String::from);
    Ok(Endpoints {
        authorize: field("authorization_endpoint").context("no authorization_endpoint")?,
        token: field("token_endpoint").context("no token_endpoint")?,
        registration: field("registration_endpoint"),
        revocation: field("revocation_endpoint").or_else(|| service.revoke_url.clone()),
    })
}

/// A client for this flow: registered (DCR) for `redirect`, or the owner's
/// from the secrets. The error for a missing owner client says what to do.
pub async fn client(
    http: &reqwest::Client,
    service: &Service,
    endpoints: &Endpoints,
    redirect: &str,
    secrets: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Result<Client> {
    if service.client == ClientKind::Owner {
        let env = service
            .client_env
            .as_deref()
            .unwrap_or("FERRULE_OAUTH_CLIENT");
        let id = secrets(&format!("{env}_ID"));
        let secret = secrets(&format!("{env}_SECRET"));
        return match (id, secret) {
            (Some(id), Some(secret)) => Ok(Client {
                id,
                secret: Some(secret),
            }),
            _ => Err(anyhow!(
                "{} needs your own OAuth client: {env}_ID and {env}_SECRET in the secrets file (see docs/m20-connections.md §3.3)",
                service.title()
            )),
        };
    }
    let reg = endpoints.registration.as_deref().context(
        "the service offers no client registration; add a custom entry with an owner client",
    )?;
    let mut body = serde_json::json!({
        "client_name": "Ferrule",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    // Every scope either mode may ask for, so one client serves both.
    let mut scopes: Vec<&str> = Vec::new();
    for s in service.scopes.iter().chain(&service.write_scopes) {
        if !scopes.contains(&s.as_str()) {
            scopes.push(s);
        }
    }
    if !scopes.is_empty() {
        body["scope"] = scopes.join(" ").into();
    }
    let resp = http
        .post(reg)
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow!("client registration: the server didn't answer"))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "client registration refused (HTTP {}{})",
            status.as_u16(),
            error_code(&doc)
                .map(|c| format!(", {c}"))
                .unwrap_or_default()
        );
    }
    let id = doc["client_id"]
        .as_str()
        .context("client registration: no client_id")?;
    Ok(Client {
        id: id.to_string(),
        secret: doc["client_secret"].as_str().map(String::from),
    })
}

pub struct AuthorizeRequest<'a> {
    pub service: &'a Service,
    pub endpoints: &'a Endpoints,
    pub client: &'a Client,
    pub redirect: &'a str,
    pub scopes: &'a [String],
    pub state: &'a str,
    pub pkce: &'a Pkce,
    pub write: bool,
}

pub fn authorize_url(r: &AuthorizeRequest<'_>) -> Result<String> {
    let mut url = url::Url::parse(&r.endpoints.authorize).context("authorization_endpoint")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &r.client.id)
            .append_pair("redirect_uri", r.redirect)
            .append_pair("state", r.state)
            .append_pair("code_challenge", &r.pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        if !r.scopes.is_empty() {
            q.append_pair("scope", &r.scopes.join(" "));
        }
        if r.service.resource_param {
            q.append_pair("resource", r.service.url(r.write));
        }
        for (k, v) in &r.service.extra_params {
            q.append_pair(k, v);
        }
    }
    Ok(url.to_string())
}

fn error_code(doc: &Value) -> Option<String> {
    let code = doc["error"].as_str()?;
    // Only a code-shaped value, never free text.
    code.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        .then(|| code.chars().take(40).collect())
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    client: &Client,
    mut form: Vec<(&str, String)>,
) -> Result<Tokens, TokenError> {
    form.push(("client_id", client.id.clone()));
    if let Some(secret) = &client.secret {
        form.push(("client_secret", secret.clone()));
    }
    let resp = http
        .post(endpoint)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|_| TokenError::Transient("the token endpoint didn't answer".into()))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return serde_json::from_value::<Tokens>(doc)
            .map_err(|_| TokenError::Transient("the token reply had no access_token".into()));
    }
    match error_code(&doc) {
        Some(code) if status.is_client_error() => Err(TokenError::Refused(code)),
        _ if status.is_server_error() || status.as_u16() == 429 => Err(TokenError::Transient(
            format!("the token endpoint failed (HTTP {})", status.as_u16()),
        )),
        _ => Err(TokenError::Refused(format!("http_{}", status.as_u16()))),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn exchange(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client: &Client,
    code: &str,
    verifier: &str,
    redirect: &str,
    resource: Option<&str>,
) -> Result<Tokens, TokenError> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(r) = resource {
        form.push(("resource", r.to_string()));
    }
    token_request(http, &endpoints.token, client, form).await
}

pub async fn refresh(
    http: &reqwest::Client,
    token_endpoint: &str,
    client: &Client,
    refresh_token: &str,
    resource: Option<&str>,
) -> Result<Tokens, TokenError> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if let Some(r) = resource {
        form.push(("resource", r.to_string()));
    }
    token_request(http, token_endpoint, client, form).await
}

/// RFC 7009: `true` if the server took it (200, or a 400 for a token it
/// no longer knows, which is the same outcome).
pub async fn revoke(
    http: &reqwest::Client,
    endpoint: &str,
    client: &Client,
    token: &str,
    hint: &str,
) -> bool {
    let mut form = vec![
        ("token", token.to_string()),
        ("token_type_hint", hint.to_string()),
        ("client_id", client.id.clone()),
    ];
    if let Some(secret) = &client.secret {
        form.push(("client_secret", secret.clone()));
    }
    match http.post(endpoint).form(&form).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    #[test]
    fn pkce_is_s256_of_the_verifier() {
        let p = Pkce::new();
        assert_eq!(p.verifier.len(), 43);
        let want = b64(ring::digest::digest(&ring::digest::SHA256, p.verifier.as_bytes()).as_ref());
        assert_eq!(p.challenge, want);
        assert_ne!(Pkce::new().verifier, p.verifier);
    }

    #[test]
    fn the_authorize_url_carries_pkce_state_scope_and_resource() {
        let cat = Catalog::built_in();
        let linear = cat.get("linear").unwrap();
        let ep = Endpoints {
            authorize: "https://mcp.linear.app/authorize".into(),
            token: "https://mcp.linear.app/token".into(),
            registration: None,
            revocation: None,
        };
        let client = Client {
            id: "cid".into(),
            secret: None,
        };
        let pkce = Pkce::new();
        let url = authorize_url(&AuthorizeRequest {
            service: linear,
            endpoints: &ep,
            client: &client,
            redirect: "https://relay.example/cb",
            scopes: linear.scopes(false),
            state: "STATE",
            pkce: &pkce,
            write: false,
        })
        .unwrap();
        let q: std::collections::HashMap<_, _> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pkce.challenge);
        assert_eq!(q["state"], "STATE");
        assert_eq!(q["scope"], "read");
        assert_eq!(q["resource"], "https://mcp.linear.app/mcp");
        assert!(!url.contains(&pkce.verifier));

        let gmail = cat.get("gmail").unwrap();
        let url = authorize_url(&AuthorizeRequest {
            service: gmail,
            scopes: gmail.scopes(false),
            ..AuthorizeRequest {
                service: linear,
                endpoints: &ep,
                client: &client,
                redirect: "r",
                scopes: &[],
                state: "S",
                pkce: &pkce,
                write: false,
            }
        })
        .unwrap();
        assert!(!url.contains("resource="), "Google gets no resource");
        assert!(url.contains("access_type=offline") && url.contains("prompt=consent"));
    }

    #[test]
    fn only_a_code_shaped_error_is_repeated() {
        assert_eq!(
            error_code(&serde_json::json!({"error": "invalid_grant"})).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            error_code(&serde_json::json!({"error": "token abc was <b>bad</b>"})),
            None
        );
    }
}
