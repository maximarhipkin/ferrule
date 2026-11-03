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
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const API_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CLIENT_ID: &str = "";
pub const CLIENT_SECRET: &str = "";
const SCOPE: &str = "openid email profile https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email";
pub const PORTS: [u16; 3] = [51121, 8085, 0];
const CALLBACK_PATH: &str = "/auth/callback";

/// Who signs in: the Google OAuth endpoints, Cloud Code API base, client ID,
/// and loopback ports (tests move all of them).
#[derive(Debug, Clone)]
pub struct Issuer {
    pub auth_url: String,
    pub token_url: String,
    pub revoke_url: String,
    pub api_base: String,
    pub client_id: String,
    pub client_secret: String,
    pub ports: Vec<u16>,
}

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
                client_id: CLIENT_ID.into(),
                client_secret: CLIENT_SECRET.into(),
                ports: PORTS.to_vec(),
            }
        } else {
            Self {
                auth_url: format!("{base}/o/oauth2/v2/auth"),
                token_url: format!("{base}/token"),
                revoke_url: format!("{base}/revoke"),
                api_base: base.to_string(),
                client_id: CLIENT_ID.into(),
                client_secret: CLIENT_SECRET.into(),
                ports: PORTS.to_vec(),
            }
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
pub fn authorize_url(issuer: &Issuer, redirect: &str, pkce: &Pkce, state: &str) -> String {
    let mut url = url::Url::parse(&issuer.auth_url).expect("a valid Google OAuth URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &issuer.client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("scope", SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url.into()
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
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", &issuer.client_id),
        ("client_secret", &issuer.client_secret),
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
    let form = [
        ("client_id", issuer.client_id.as_str()),
        ("client_secret", issuer.client_secret.as_str()),
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
}

pub fn platform_enum() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "DARWIN_ARM64",
        ("macos", _) => "DARWIN_AMD64",
        ("linux", "aarch64") => "LINUX_ARM64",
        ("windows", _) => "WINDOWS_AMD64",
        _ => "LINUX_AMD64",
    }
}

/// Resolves the user's `cloudaicompanionProject` and tier name from
/// `v1internal:loadCodeAssist` (and `v1internal:onboardUser` if needed).
pub async fn load_code_assist(
    http: &reqwest::Client,
    issuer: &Issuer,
    access_token: &str,
) -> Option<CodeAssistInfo> {
    let url = format!("{}/v1internal:loadCodeAssist", issuer.api_base);
    let metadata = json!({
        "ideType": "ANTIGRAVITY",
        "ideVersion": "1.4.12",
        "platform": platform_enum(),
        "pluginType": "GEMINI",
    });
    let resp = http
        .post(&url)
        .bearer_auth(access_token)
        .header(
            reqwest::header::USER_AGENT,
            format!(
                "antigravity/1.4.12 {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        )
        .timeout(Duration::from_secs(15))
        .json(&json!({ "metadata": metadata }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let doc: Value = resp.json().await.ok()?;
    let mut project_id = doc["cloudaicompanionProject"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            doc["cloudaicompanionProject"]["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let tier = doc["paidTier"]["name"]
        .as_str()
        .or_else(|| doc["paidTier"]["id"].as_str())
        .or_else(|| doc["currentTier"]["name"].as_str())
        .or_else(|| doc["currentTier"]["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if project_id.is_none() {
        let tier_id = doc["allowedTiers"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|t| t["isDefault"].as_bool() == Some(true))
                    .or_else(|| arr.first())
            })
            .and_then(|t| t["id"].as_str())
            .unwrap_or("free-tier");
        let onboard_url = format!("{}/v1internal:onboardUser", issuer.api_base);
        if let Ok(r) = http
            .post(&onboard_url)
            .bearer_auth(access_token)
            .timeout(Duration::from_secs(15))
            .json(&json!({
                "tierId": tier_id,
                "metadata": metadata,
            }))
            .send()
            .await
        {
            if let Ok(ob) = r.json::<Value>().await {
                project_id = ob["response"]["cloudaicompanionProject"]["id"]
                    .as_str()
                    .or_else(|| ob["cloudaicompanionProject"]["id"].as_str())
                    .or_else(|| ob["cloudaicompanionProject"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
        }
    }

    Some(CodeAssistInfo { project_id, tier })
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
    let resp = http
        .post(&url)
        .bearer_auth(access_token)
        .header(
            reqwest::header::USER_AGENT,
            format!(
                "antigravity/1.4.12 {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        )
        .timeout(Duration::from_secs(10))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let doc: Value = resp.json().await.ok()?;
    let limits = ferrule_providers::cloudcode::parse_quota_summary(&doc);
    (!limits.windows.is_empty()).then_some(limits)
}

/// The loopback listener the browser is redirected back to.
pub struct Callback {
    listener: TcpListener,
    pub redirect: String,
}

impl Callback {
    /// Binds 127.0.0.1 on the first free port of the issuer's list.
    pub async fn bind(issuer: &Issuer) -> Result<Self> {
        for port in &issuer.ports {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", *port)).await {
                let port = listener.local_addr()?.port();
                return Ok(Self {
                    listener,
                    redirect: format!("http://localhost:{port}{CALLBACK_PATH}"),
                });
            }
        }
        bail!(
            "ports {:?} are busy, so the browser can't come back here; use --paste",
            issuer.ports
        )
    }

    /// Waits for the browser's redirect with this `state`; returns the code.
    pub async fn wait(self, state: &str, limit: Duration) -> Result<String> {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (mut sock, _) = tokio::time::timeout(left, self.listener.accept())
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
            if !target.starts_with(CALLBACK_PATH) && !target.starts_with("/oauth-callback") {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            }
            let url = format!("http://localhost{target}");
            let outcome = code_from_redirect(&url, state);
            let page = match &outcome {
                Ok(_) => {
                    "Signed in to ferrule (Gemini / Antigravity plan). You can close this tab."
                }
                Err(_) => "The sign-in didn't complete. Go back to ferrule.",
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            return outcome;
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
