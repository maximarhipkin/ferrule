//! The ChatGPT sign-in protocol, as the Codex CLI speaks it at
//! `openai/codex` 67a709665ac7b50311b93e32612c9a8281684787 (see
//! docs/m35-subscriptions.md §3). Errors are fixed phrases plus the OAuth
//! error code, never a server's free text or a token (M20's rule).

use anyhow::{anyhow, bail, Context, Result};
use ferrule_connections::oauth::TokenError;
use ferrule_connections::seal::{b64, random, sha256_b64};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const ISSUER: &str = "https://auth.openai.com";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// The only two ports the client's registration allows.
pub const PORTS: [u16; 2] = [1455, 1457];
const CALLBACK_PATH: &str = "/auth/callback";
/// How long a device code stays good.
const DEVICE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Who signs in: the issuer, the client and the loopback ports (tests move
/// all three).
#[derive(Debug, Clone)]
pub struct Issuer {
    pub base: String,
    pub client_id: String,
    pub ports: Vec<u16>,
}

impl Default for Issuer {
    fn default() -> Self {
        Self::new(ISSUER)
    }
}

impl Issuer {
    /// `base` = "" is the real one.
    pub fn new(base: &str) -> Self {
        let base = if base.trim().is_empty() {
            ISSUER
        } else {
            base.trim()
        };
        Self {
            base: base.trim_end_matches('/').to_string(),
            client_id: CLIENT_ID.to_string(),
            ports: PORTS.to_vec(),
        }
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
    /// Where the owner types a device code.
    pub fn device_page(&self) -> String {
        self.url("/codex/device")
    }
}

/// What a sign-in or a refresh gave back. Every field is optional on a
/// refresh; a sign-in always has all three.
#[derive(Clone, Default, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(…)")
    }
}

/// S256 PKCE with Codex's 64-byte verifier.
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

/// The browser sign-in page, with Codex's parameters in Codex's order.
pub fn authorize_url(issuer: &Issuer, redirect: &str, pkce: &Pkce, state: &str) -> String {
    let mut url = url::Url::parse(&issuer.url("/oauth/authorize")).expect("an issuer URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &issuer.client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("scope", SCOPE)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ferrule_providers::codex::ORIGINATOR);
    url.into()
}

fn oauth_code(doc: &Value) -> Option<String> {
    let e = &doc["error"];
    e.as_str()
        .or(e["code"].as_str())
        .or(e["type"].as_str())
        .or(doc["code"].as_str())
        .filter(|c| {
            c.len() <= 64
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "_-.".contains(ch))
        })
        .map(str::to_string)
}

/// `authorization_code` → tokens, a form POST.
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
        ("code_verifier", verifier),
    ];
    let resp = http
        .post(issuer.url("/oauth/token"))
        .header("accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|_| anyhow!("the sign-in server didn't answer"))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = oauth_code(&doc).unwrap_or_else(|| format!("http_{}", status.as_u16()));
        bail!("the sign-in server refused the code ({code})");
    }
    let tokens: Tokens =
        serde_json::from_value(doc).map_err(|_| anyhow!("the sign-in reply didn't parse"))?;
    if tokens.access_token.is_none() || tokens.refresh_token.is_none() {
        bail!("the sign-in reply had no tokens");
    }
    Ok(tokens)
}

/// A refresh: JSON, not a form. A returned refresh token replaces the old
/// one (rotation).
pub async fn refresh(
    http: &reqwest::Client,
    issuer: &Issuer,
    refresh_token: &str,
) -> Result<Tokens, TokenError> {
    let resp = http
        .post(issuer.url("/oauth/token"))
        .header("accept", "application/json")
        .json(&json!({
            "client_id": issuer.client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|_| TokenError::Transient("the sign-in server didn't answer".into()))?;
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
            Some(
                "invalid_grant"
                    | "refresh_token_expired"
                    | "refresh_token_reused"
                    | "refresh_token_invalidated"
            )
        );
    if permanent {
        return Err(TokenError::Refused(
            code.unwrap_or_else(|| format!("http_{}", status.as_u16())),
        ));
    }
    Err(TokenError::Transient(format!(
        "the sign-in server failed the refresh (HTTP {}{})",
        status.as_u16(),
        code.map(|c| format!(", {c}")).unwrap_or_default()
    )))
}

/// Best effort: `true` when the server took it.
pub async fn revoke(http: &reqwest::Client, issuer: &Issuer, refresh_token: &str) -> bool {
    let call = http
        .post(issuer.url("/oauth/revoke"))
        .timeout(Duration::from_secs(10))
        .json(&json!({
            "token": refresh_token,
            "token_type_hint": "refresh_token",
            "client_id": issuer.client_id,
        }))
        .send();
    matches!(call.await, Ok(r) if r.status().is_success())
}

/// A device code to show the owner. `device_auth_id` stays in the process:
/// the code alone is useless to whoever sees it.
pub struct DeviceCode {
    pub user_code: String,
    pub page: String,
    device_auth_id: String,
    interval: Duration,
}

impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceCode({} at {})", self.user_code, self.page)
    }
}

/// Asks for a device code. `Ok(None)`: the issuer hasn't enabled device
/// sign-in for this client (a 404); use the browser or the paste flow.
pub async fn start_device(http: &reqwest::Client, issuer: &Issuer) -> Result<Option<DeviceCode>> {
    let resp = http
        .post(issuer.url("/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": issuer.client_id}))
        .send()
        .await
        .map_err(|_| anyhow!("the sign-in server didn't answer"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!(
            "the sign-in server refused a device code (HTTP {})",
            resp.status().as_u16()
        );
    }
    let doc: Value = resp
        .json()
        .await
        .map_err(|_| anyhow!("the device-code reply didn't parse"))?;
    let text = |k: &str| doc[k].as_str().map(str::to_string);
    let device_auth_id = text("device_auth_id").context("the device-code reply had no id")?;
    let user_code = text("user_code")
        .or_else(|| text("usercode"))
        .context("the device-code reply had no code")?;
    // A string at the pinned commit; a number is taken too.
    let interval = doc["interval"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or(doc["interval"].as_u64())
        .unwrap_or(5)
        .clamp(1, 60);
    Ok(Some(DeviceCode {
        user_code,
        page: issuer.device_page(),
        device_auth_id,
        interval: Duration::from_secs(interval),
    }))
}

/// Polls until the owner has typed the code (403/404 mean not yet), then
/// trades the authorization code it yields. Gives up after 15 minutes.
pub async fn finish_device(
    http: &reqwest::Client,
    issuer: &Issuer,
    code: &DeviceCode,
) -> Result<Tokens> {
    let deadline = Instant::now() + DEVICE_LIMIT;
    loop {
        let resp = http
            .post(issuer.url("/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id": code.device_auth_id, "user_code": code.user_code}))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let doc: Value = r
                    .json()
                    .await
                    .map_err(|_| anyhow!("the device sign-in reply didn't parse"))?;
                let auth_code = doc["authorization_code"]
                    .as_str()
                    .context("the device sign-in reply had no code")?;
                let verifier = doc["code_verifier"]
                    .as_str()
                    .context("the device sign-in reply had no verifier")?;
                let redirect = issuer.url("/deviceauth/callback");
                return exchange(http, issuer, auth_code, verifier, &redirect).await;
            }
            Ok(r) if matches!(r.status().as_u16(), 403 | 404) => {}
            Ok(r) => bail!("the device sign-in failed (HTTP {})", r.status().as_u16()),
            // A dropped poll is retried until the deadline.
            Err(_) => {}
        }
        if Instant::now() + code.interval > deadline {
            bail!("the device code expired (15 minutes); run the sign-in again");
        }
        tokio::time::sleep(code.interval).await;
    }
}

/// The loopback listener the browser is sent back to.
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
                    redirect: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
                });
            }
        }
        bail!(
            "ports {:?} are busy, so the browser can't come back here; use --paste",
            issuer.ports
        )
    }

    /// Waits for the browser's redirect with this `state`; returns the code.
    /// Anything else (a favicon, a stray visit) gets a 404 and is ignored.
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
            if !target.starts_with(CALLBACK_PATH) {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                continue;
            }
            let url = format!("http://127.0.0.1{target}");
            let outcome = code_from_redirect(&url, state);
            let page = match &outcome {
                Ok(_) => "Signed in to ferrule. You can close this tab.",
                Err(_) => "The sign-in didn't complete. Go back to the terminal.",
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

/// The code from a redirect URL (the listener's, or one the owner pasted),
/// after checking `state` exactly.
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
