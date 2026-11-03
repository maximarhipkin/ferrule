//! The Google / Antigravity OAuth 2.0 PKCE sign-in protocol and Cloud Code
//! onboarding (`v1internal:loadCodeAssist`), spoken directly by ferrule so
//! no external CLI (`agy` or `gemini`) has to be installed on the server.
//! Errors are fixed phrases plus the OAuth error code, never a server's
//! free text or a token (M20's rule).

use anyhow::{anyhow, bail, Context, Result};
use ferrule_connections::oauth::TokenError;
use ferrule_connections::seal::{b64, random, sha256_b64};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub use ferrule_providers::cloudcode::platform_enum;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const API_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
/// Where the OAuth client the sign-in uses comes from. It isn't in the
/// source: the owner puts it in the secrets file (`private/secrets.env`)
/// or the environment, and it's read each time a sign-in or a refresh
/// needs it.
pub const CLIENT_ID_VAR: &str = "FERRULE_GEMINI_CLIENT_ID";
pub const CLIENT_SECRET_VAR: &str = "FERRULE_GEMINI_CLIENT_SECRET";
/// What a sign-in or a refresh says when there's no client.
pub const NO_CLIENT: &str = "Google sign-in needs an OAuth client first: put FERRULE_GEMINI_CLIENT_ID and FERRULE_GEMINI_CLIENT_SECRET in private/secrets.env or the environment, then try again";
const SCOPE: &str = "openid email profile https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email";
pub const PORTS: [u16; 3] = [51121, 8085, 0];
const CALLBACK_PATH: &str = "/auth/callback";

/// Who signs in: the Google OAuth endpoints, Cloud Code API base, client,
/// loopback ports, and how onboarding is waited for (tests move all of them).
/// An empty `client_id` / `client_secret` means "the secrets file's", read
/// when it's needed ([`Issuer::client`]), not when the plan is made.
#[derive(Debug, Clone)]
pub struct Issuer {
    pub auth_url: String,
    pub token_url: String,
    pub revoke_url: String,
    pub api_base: String,
    pub client_id: String,
    pub client_secret: String,
    pub ports: Vec<u16>,
    /// Between two looks at a running `onboardUser` operation.
    pub onboard_poll: Duration,
    /// How long onboarding may take before the lookup gives up for now.
    pub onboard_limit: Duration,
}

const ONBOARD_POLL: Duration = Duration::from_secs(5);
const ONBOARD_LIMIT: Duration = Duration::from_secs(45);

impl Default for Issuer {
    fn default() -> Self {
        Self::new("")
    }
}

impl Issuer {
    /// `base` = "" uses Google's production OAuth and Cloud Code endpoints.
    /// A custom `base` (in tests) points OAuth and Cloud Code at that mock server.
    pub fn new(base: &str) -> Self {
        let base = base.trim().trim_end_matches('/');
        if base.is_empty() {
            Self {
                auth_url: AUTH_URL.into(),
                token_url: TOKEN_URL.into(),
                revoke_url: REVOKE_URL.into(),
                api_base: API_BASE.into(),
                client_id: String::new(),
                client_secret: String::new(),
                ports: PORTS.to_vec(),
                onboard_poll: ONBOARD_POLL,
                onboard_limit: ONBOARD_LIMIT,
            }
        } else {
            Self {
                auth_url: format!("{base}/o/oauth2/v2/auth"),
                token_url: format!("{base}/token"),
                revoke_url: format!("{base}/revoke"),
                api_base: base.to_string(),
                client_id: String::new(),
                client_secret: String::new(),
                ports: PORTS.to_vec(),
                onboard_poll: ONBOARD_POLL,
                onboard_limit: ONBOARD_LIMIT,
            }
        }
    }

    /// The client id and secret: the issuer's own when set (tests), else
    /// [`CLIENT_ID_VAR`] and [`CLIENT_SECRET_VAR`]. Both or [`NO_CLIENT`].
    pub fn client(&self) -> Result<(String, String)> {
        let pick = |own: &str, var: &str| {
            Some(own.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    ferrule_sandbox::secret_env::non_empty(var).map(|v| v.trim().to_string())
                })
                .filter(|v| !v.is_empty())
        };
        match (
            pick(&self.client_id, CLIENT_ID_VAR),
            pick(&self.client_secret, CLIENT_SECRET_VAR),
        ) {
            (Some(id), Some(secret)) => Ok((id, secret)),
            _ => bail!(NO_CLIENT),
        }
    }
}

/// What a sign-in or a refresh gave back.
#[derive(Clone, Default, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(…)")
    }
}

/// S256 PKCE with a 64-byte verifier.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = b64(&random::<64>());
        let challenge = sha256_b64(verifier.as_bytes());
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

pub fn new_state() -> String {
    b64(&random::<32>())
}

/// The Google OAuth 2.0 browser sign-in URL with PKCE and offline access.
/// [`NO_CLIENT`] when there's no OAuth client to sign in with.
pub fn authorize_url(issuer: &Issuer, redirect: &str, pkce: &Pkce, state: &str) -> Result<String> {
    let (client_id, _) = issuer.client()?;
    let mut url = url::Url::parse(&issuer.auth_url).expect("a valid Google OAuth URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("scope", SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.into())
}

fn oauth_code(doc: &Value) -> Option<String> {
    let e = &doc["error"];
    e.as_str()
        .or(e["status"].as_str())
        .or(e["code"].as_str())
        .or(doc["code"].as_str())
        .filter(|c| {
            c.len() <= 64
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "_-.".contains(ch))
        })
        .map(str::to_string)
}

/// `authorization_code` → tokens, a form POST to Google's token endpoint.
pub async fn exchange(
    http: &reqwest::Client,
    issuer: &Issuer,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<Tokens> {
    let (client_id, client_secret) = issuer.client()?;
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", &client_id),
        ("client_secret", &client_secret),
        ("code_verifier", verifier),
    ];
    let resp = http
        .post(&issuer.token_url)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|_| anyhow!("Google's sign-in server didn't answer"))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = oauth_code(&doc).unwrap_or_else(|| format!("http_{}", status.as_u16()));
        bail!("Google's sign-in server refused the code ({code})");
    }
    let tokens: Tokens =
        serde_json::from_value(doc).map_err(|_| anyhow!("the sign-in reply didn't parse"))?;
    if tokens.access_token.is_none() || tokens.refresh_token.is_none() {
        bail!("the sign-in reply had no tokens");
    }
    Ok(tokens)
}

/// Refreshes an access token with Google's token endpoint.
pub async fn refresh(
    http: &reqwest::Client,
    issuer: &Issuer,
    refresh_token: &str,
) -> Result<Tokens, TokenError> {
    // No client is the owner's setup, not a refused refresh: the sign-in
    // stays, and works again once the client is back.
    let (client_id, client_secret) = issuer
        .client()
        .map_err(|_| TokenError::Transient(NO_CLIENT.into()))?;
    let form = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    let resp = http
        .post(&issuer.token_url)
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|_| TokenError::Transient("Google's sign-in server didn't answer".into()))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return serde_json::from_value(doc)
            .map_err(|_| TokenError::Transient("the refresh reply didn't parse".into()));
    }
    let code = oauth_code(&doc);
    let permanent = status.as_u16() == 401
        || matches!(
            code.as_deref(),
            Some("invalid_grant" | "refresh_token_expired" | "unauthorized_client")
        );
    if permanent {
        return Err(TokenError::Refused(
            code.unwrap_or_else(|| format!("http_{}", status.as_u16())),
        ));
    }
    Err(TokenError::Transient(format!(
        "Google's sign-in server failed the refresh (HTTP {}{})",
        status.as_u16(),
        code.map(|c| format!(", {c}")).unwrap_or_default()
    )))
}

/// Best-effort token revocation on logout.
pub async fn revoke(http: &reqwest::Client, issuer: &Issuer, token: &str) -> bool {
    let call = http
        .post(&issuer.revoke_url)
        .timeout(Duration::from_secs(10))
        .form(&[("token", token)])
        .send();
    matches!(call.await, Ok(r) if r.status().is_success())
}

/// Companion project and tier information returned by `v1internal:loadCodeAssist`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodeAssistInfo {
    pub project_id: Option<String>,
    pub tier: Option<String>,
    /// Why there is no project, in fixed words and Google's codes, when
    /// there is none.
    pub problem: Option<String>,
}

impl CodeAssistInfo {
    fn missing(tier: Option<String>, problem: impl Into<String>) -> Self {
        Self {
            project_id: None,
            tier,
            problem: Some(problem.into()),
        }
    }
}

/// A Cloud Code `v1internal` call with the Antigravity client's identity
/// (the one [`ferrule_providers::cloudcode::client_version`]): its status
/// and JSON body, or why there was none.
async fn cloud_code(
    access_token: &str,
    request: reqwest::RequestBuilder,
    limit: Duration,
) -> Result<(reqwest::StatusCode, Value), String> {
    let version = ferrule_providers::cloudcode::client_version();
    let resp = request
        .bearer_auth(access_token)
        .header(
            reqwest::header::USER_AGENT,
            ferrule_providers::cloudcode::user_agent(&version),
        )
        .timeout(limit)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "it timed out".to_string()
            } else {
                "it didn't answer".to_string()
            }
        })?;
    let status = resp.status();
    let doc = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, doc))
}

fn refused_words(what: &str, status: reqwest::StatusCode, doc: &Value) -> String {
    let code = oauth_code(doc)
        .map(|c| format!(", {c}"))
        .unwrap_or_default();
    format!("{what} was refused (HTTP {}{code})", status.as_u16())
}

fn project_in(doc: &Value) -> Option<String> {
    let p = &doc["cloudaicompanionProject"];
    p.as_str()
        .or_else(|| p["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An operation name safe to put in a URL path (`operations/…`).
fn operation_name(doc: &Value) -> Option<&str> {
    doc["name"].as_str().filter(|n| {
        !n.is_empty()
            && n.len() <= 200
            && !n.contains("..")
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
    })
}

/// Resolves the user's `cloudaicompanionProject` and tier name from
/// `v1internal:loadCodeAssist`, onboarding the account with
/// `v1internal:onboardUser` (and waiting for that operation to finish,
/// within [`Issuer::onboard_limit`]) when Google says it has none yet.
/// No project is not an error here: [`CodeAssistInfo::problem`] says why.
pub async fn load_code_assist(
    http: &reqwest::Client,
    issuer: &Issuer,
    access_token: &str,
) -> CodeAssistInfo {
    let metadata = json!({
        "ideType": "ANTIGRAVITY",
        "ideVersion": ferrule_providers::cloudcode::client_version(),
        "platform": platform_enum(),
        "pluginType": "GEMINI",
    });
    let url = format!("{}/v1internal:loadCodeAssist", issuer.api_base);
    let request = http.post(&url).json(&json!({ "metadata": metadata }));
    let doc = match cloud_code(access_token, request, Duration::from_secs(15)).await {
        Ok((status, doc)) if status.is_success() => doc,
        Ok((status, doc)) => {
            return CodeAssistInfo::missing(None, refused_words("loadCodeAssist", status, &doc))
        }
        Err(why) => return CodeAssistInfo::missing(None, format!("loadCodeAssist: {why}")),
    };
    let tier = doc["paidTier"]["name"]
        .as_str()
        .or_else(|| doc["paidTier"]["id"].as_str())
        .or_else(|| doc["currentTier"]["name"].as_str())
        .or_else(|| doc["currentTier"]["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(project_id) = project_in(&doc) {
        return CodeAssistInfo {
            project_id: Some(project_id),
            tier,
            problem: None,
        };
    }
    if doc["currentTier"].is_object() {
        // Onboarded already, and still no project: a tier that wants the
        // owner's own Google Cloud project, which ferrule doesn't pick.
        return CodeAssistInfo::missing(
            tier,
            "Google onboarded the account but gave it no Cloud Code project",
        );
    }
    let allowed = doc["allowedTiers"].as_array().filter(|a| !a.is_empty());
    let Some(allowed) = allowed else {
        let reason = doc["ineligibleTiers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t["reasonCode"].as_str())
            .filter(|c| {
                c.len() <= 64 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            })
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        return CodeAssistInfo::missing(tier, format!("Google offers the account no tier{reason}"));
    };
    let chosen = allowed
        .iter()
        .find(|t| t["isDefault"].as_bool() == Some(true))
        .unwrap_or(&allowed[0]);
    let tier_id = chosen["id"].as_str().unwrap_or("free-tier").to_string();
    let tier = tier.or_else(|| {
        chosen["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    onboard(http, issuer, access_token, &tier_id, metadata, tier).await
}

/// `v1internal:onboardUser`, then its long-running operation until it is
/// `done` (re-asking `onboardUser`, or `GET v1internal/{name}` when the
/// reply names the operation), within [`Issuer::onboard_limit`].
async fn onboard(
    http: &reqwest::Client,
    issuer: &Issuer,
    access_token: &str,
    tier_id: &str,
    metadata: Value,
    tier: Option<String>,
) -> CodeAssistInfo {
    let deadline = Instant::now() + issuer.onboard_limit;
    let onboard_url = format!("{}/v1internal:onboardUser", issuer.api_base);
    let body = json!({ "tierId": tier_id, "metadata": metadata });
    let mut operation: Option<String> = None;
    loop {
        let request = match &operation {
            Some(name) => http.get(format!("{}/v1internal/{name}", issuer.api_base)),
            None => http.post(&onboard_url).json(&body),
        };
        let doc = match cloud_code(access_token, request, Duration::from_secs(15)).await {
            Ok((status, doc)) if status.is_success() => doc,
            Ok((status, doc)) => {
                return CodeAssistInfo::missing(tier, refused_words("onboardUser", status, &doc))
            }
            Err(why) => return CodeAssistInfo::missing(tier, format!("onboardUser: {why}")),
        };
        if doc["done"].as_bool() == Some(true) {
            if doc["error"].is_object() {
                let code = oauth_code(&doc)
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                return CodeAssistInfo::missing(tier, format!("onboarding failed{code}"));
            }
            return match project_in(&doc["response"]).or_else(|| project_in(&doc)) {
                Some(project_id) => CodeAssistInfo {
                    project_id: Some(project_id),
                    tier,
                    problem: None,
                },
                None => CodeAssistInfo::missing(tier, "onboarding finished without a project"),
            };
        }
        if let Some(name) = operation_name(&doc) {
            operation = Some(name.to_string());
        }
        if Instant::now() + issuer.onboard_poll > deadline {
            return CodeAssistInfo::missing(
                tier,
                format!(
                    "onboarding didn't finish within {} s",
                    issuer.onboard_limit.as_secs()
                ),
            );
        }
        tokio::time::sleep(issuer.onboard_poll).await;
    }
}

/// Fetches the user's 5-hour and weekly quota summary from
/// `v1internal:retrieveUserQuotaSummary`.
pub async fn fetch_quota(
    http: &reqwest::Client,
    issuer: &Issuer,
    access_token: &str,
    project_id: Option<&str>,
) -> Option<ferrule_providers::codex::RateLimits> {
    let url = format!("{}/v1internal:retrieveUserQuotaSummary", issuer.api_base);
    let body = match project_id {
        Some(p) if !p.is_empty() => json!({ "project": p }),
        _ => json!({}),
    };
    let request = http.post(&url).json(&body);
    let (status, doc) = cloud_code(access_token, request, Duration::from_secs(10))
        .await
        .ok()?;
    if !status.is_success() {
        return None;
    }
    let limits = ferrule_providers::cloudcode::parse_quota_summary(&doc);
    (!limits.windows.is_empty()).then_some(limits)
}

/// The loopback listener the browser is redirected back to: 127.0.0.1
/// and, where there is one, ::1 on the same port, since the redirect says
/// `localhost` (what Google's client takes) and a browser may try either.
pub struct Callback {
    listeners: Vec<TcpListener>,
    pub redirect: String,
}

/// What one request to the callback was.
#[derive(Debug, PartialEq)]
enum Visit {
    /// Not the callback path (a favicon, a probe).
    Elsewhere,
    /// The callback path, but no sign-in reply for this flow: no state,
    /// or another flow's (an older tab). The wait goes on.
    NotOurs(&'static str),
    /// This flow's reply: the code, or why there is none.
    Ours(Result<String, String>),
}

fn visit(target: &str, state: &str) -> Visit {
    if !target.starts_with(CALLBACK_PATH) && !target.starts_with("/oauth-callback") {
        return Visit::Elsewhere;
    }
    let url = format!("http://localhost{target}");
    match ferrule_connections::paste::parse(&url) {
        None => Visit::NotOurs("This isn't a sign-in reply. Go back to ferrule."),
        Some(p) if p.state != state => Visit::NotOurs(
            "This sign-in link is from an earlier attempt. Use the newest link ferrule gave you.",
        ),
        Some(_) => Visit::Ours(code_from_redirect(&url, state).map_err(|e| format!("{e:#}"))),
    }
}

impl Callback {
    /// Binds the first port of the issuer's list free on 127.0.0.1 (and on
    /// ::1 too, when this machine has IPv6 loopback).
    pub async fn bind(issuer: &Issuer) -> Result<Self> {
        for &wanted in &issuer.ports {
            // A random port may be taken on ::1 alone: a few draws.
            let draws = if wanted == 0 { 5 } else { 1 };
            for _ in 0..draws {
                let Ok(v4) = TcpListener::bind(("127.0.0.1", wanted)).await else {
                    break;
                };
                let port = v4.local_addr()?.port();
                let listeners = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
                    Ok(v6) => vec![v4, v6],
                    // Someone else has ::1 on this port: a browser trying
                    // ::1 first would reach them, not ferrule.
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                    // No IPv6 loopback here: `localhost` is 127.0.0.1.
                    Err(_) => vec![v4],
                };
                return Ok(Self {
                    listeners,
                    redirect: format!("http://localhost:{port}{CALLBACK_PATH}"),
                });
            }
        }
        bail!(
            "ports {:?} are busy, so the browser can't come back here; use --paste",
            issuer.ports
        )
    }

    /// The addresses it listens on.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect()
    }

    async fn accept(&self) -> std::io::Result<TcpStream> {
        match self.listeners.as_slice() {
            [one] => one.accept().await.map(|(s, _)| s),
            [a, b, ..] => tokio::select! {
                r = a.accept() => r.map(|(s, _)| s),
                r = b.accept() => r.map(|(s, _)| s),
            },
            [] => std::future::pending().await,
        }
    }

    /// Waits for the browser's redirect with this `state`; returns the code.
    /// A request that isn't this flow's reply (another flow's state, none
    /// at all) is answered with a short page and the wait goes on.
    pub async fn wait(self, state: &str, limit: Duration) -> Result<String> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut sock = tokio::time::timeout(left, self.accept())
                .await
                .map_err(|_| {
                    anyhow!("no sign-in came back within {} min", limit.as_secs() / 60)
                })??;
            let mut buf = vec![0u8; 8192];
            let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf))
                .await
                .unwrap_or(Ok(0))
                .unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let target = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("");
            let (code, page, outcome) = match visit(target, state) {
                Visit::Elsewhere => ("404 Not Found", "", None),
                Visit::NotOurs(page) => ("400 Bad Request", page, None),
                Visit::Ours(Ok(code)) => (
                    "200 OK",
                    "Signed in to ferrule (Gemini / Antigravity plan). You can close this tab.",
                    Some(Ok(code)),
                ),
                Visit::Ours(Err(why)) => (
                    "200 OK",
                    "The sign-in didn't complete. Go back to ferrule.",
                    Some(Err(anyhow!(why))),
                ),
            };
            let reply = format!(
                "HTTP/1.1 {code}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            if let Some(outcome) = outcome {
                return outcome;
            }
        }
    }
}

/// Extracts the authorization code from a redirect URL (from the loopback
/// listener or pasted by the user from a remote browser), verifying `state`.
pub fn code_from_redirect(text: &str, state: &str) -> Result<String> {
    let pasted = ferrule_connections::paste::parse(text)
        .context("that isn't the address the sign-in redirected to (it needs code and state)")?;
    if pasted.state != state {
        bail!("that redirect belongs to another sign-in (state doesn't match)");
    }
    if let Some(error) = pasted.error {
        let error: String = error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        bail!("the sign-in was refused ({error})");
    }
    pasted.code.context("the redirect had no code")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_flows_reply_ends_the_wait() {
        let st = "state-1";
        assert_eq!(visit("/favicon.ico", st), Visit::Elsewhere);
        assert!(matches!(visit("/auth/callback", st), Visit::NotOurs(_)));
        assert!(matches!(
            visit("/auth/callback?code=c&state=older", st),
            Visit::NotOurs(_)
        ));
        assert_eq!(
            visit("/auth/callback?code=c&state=state-1", st),
            Visit::Ours(Ok("c".into()))
        );
        assert!(matches!(
            visit("/auth/callback?error=access_denied&state=state-1", st),
            Visit::Ours(Err(why)) if why.contains("access_denied")
        ));
    }

    #[test]
    fn an_operation_name_must_be_a_plain_path() {
        assert_eq!(
            operation_name(&json!({"name": "operations/abc-1_2.x"})),
            Some("operations/abc-1_2.x")
        );
        for bad in ["", "operations/../x", "operations/a?b", "https://x/y"] {
            assert_eq!(operation_name(&json!({ "name": bad })), None, "{bad}");
        }
    }
}
