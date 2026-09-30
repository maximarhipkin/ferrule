//! The ChatGPT plan's model path (M35, docs/m35-subscriptions.md §4): the
//! Responses driver's body and parser, sent to the Codex backend
//! (`chatgpt.com/backend-api/codex`) with a signed-in account's token
//! instead of an API key.
//!
//! The protocol is the Codex CLI's, read from `openai/codex` at tag
//! `rust-v0.157.1`, commit 36650394c5b38c2990ccf2a3457165ca3e9d9726. It
//! isn't a published API.
//!
//! The token comes from a [`PlanAuth`] (`ferrule-plans` keeps it sealed and
//! refreshes it); a 401 asks it to refresh once. The `x-codex-*` rate-limit
//! headers and a usage-limit 429 go back to it, so `/status` can show the
//! plan's windows.

use crate::common;
use crate::responses::{read_stream, reasoning_rejected, ResponsesProvider};
use crate::DriverOptions;
use ferrule_core::error::CoreError;
use ferrule_core::failure::{requires_newer_client, Failure, Kind};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, DeltaSink, Provider};
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// The `originator` the backend knows the Codex CLI's client by.
pub const ORIGINATOR: &str = "codex_cli_rs";
/// The Codex CLI version ferrule speaks as: sent as `client_version` when
/// listing models (the catalog filters on each model's
/// `minimal_client_version`) and as the `version` header on every request,
/// where the backend reads it as the client's version and refuses a model
/// newer than it. Since M36 this is only the floor: the version in use is
/// learned ([`version`]), and `FERRULE_CODEX_CLIENT_VERSION` overrides it.
pub const CLIENT_VERSION: &str = "0.157.1";

pub mod version;

/// The client version in use now: learned, overridden, or [`CLIENT_VERSION`].
pub fn client_version() -> String {
    version::global().current()
}

/// What to send as the account's credentials on one request.
#[derive(Clone)]
pub struct PlanCredentials {
    pub access_token: String,
    /// `ChatGPT-Account-ID`.
    pub account_id: Option<String>,
    /// FedRAMP workspaces get `X-OpenAI-Fedramp: true`.
    pub fedramp: bool,
}

impl std::fmt::Debug for PlanCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanCredentials")
            .field("account_id", &self.account_id)
            .field("fedramp", &self.fedramp)
            .finish_non_exhaustive()
    }
}

/// One usage window, as the headers give it.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitWindow {
    pub minutes: Option<u64>,
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: Option<u64>,
}

/// What a response said about the plan's limits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateLimits {
    pub windows: Vec<LimitWindow>,
    /// A usage-limit stop: nothing more until then (unix seconds).
    pub limited_until: Option<u64>,
    pub plan: Option<String>,
}

/// Where a plan's token comes from.
#[async_trait::async_trait]
pub trait PlanAuth: Send + Sync {
    /// A token that's good for the next request (refreshed ahead of expiry).
    async fn credentials(&self) -> Result<PlanCredentials, CoreError>;
    /// The backend answered 401 to `used`: refresh (unless another process
    /// already has). An error means sign in again.
    async fn refused(&self, used: &str) -> Result<(), CoreError>;
    /// The rate-limit headers of a response, or a usage-limit stop.
    fn observe(&self, limits: &RateLimits);
}

pub struct CodexProvider {
    body: ResponsesProvider,
    name: String,
    base_url: String,
    auth: Arc<dyn PlanAuth>,
    client: reqwest::Client,
    identity: Arc<version::ClientIdentity>,
}

impl CodexProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        options: DriverOptions,
        auth: Arc<dyn PlanAuth>,
    ) -> Self {
        let name = name.into();
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            body: ResponsesProvider::for_codex(name.clone(), model.into(), options),
            name,
            base_url: if base_url.is_empty() {
                DEFAULT_BASE_URL.into()
            } else {
                base_url
            },
            auth,
            client: common::client(),
            identity: version::global(),
        }
    }

    /// Speak as `identity`'s version instead of the process's.
    pub fn with_identity(mut self, identity: Arc<version::ClientIdentity>) -> Self {
        self.identity = identity;
        self
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        creds: &PlanCredentials,
        version: &str,
    ) -> reqwest::RequestBuilder {
        let mut rb = self
            .client
            .request(method, url)
            .bearer_auth(&creds.access_token)
            .header("originator", ORIGINATOR)
            .header(reqwest::header::USER_AGENT, user_agent(version))
            .header("version", version);
        if let Some(id) = &creds.account_id {
            rb = rb.header("ChatGPT-Account-ID", id);
        }
        if creds.fedramp {
            rb = rb.header("X-OpenAI-Fedramp", "true");
        }
        rb
    }

    /// Send `body` and read the stream to its final response object. A 401
    /// refreshes the sign-in once; a 429 is read for the plan's limit; a
    /// "requires a newer version" refusal learns the client version now
    /// and, if it moved, retries once (docs/m36-self-update.md §1.4).
    async fn post(&self, body: &Value, sink: Option<&DeltaSink>) -> Result<Value, CoreError> {
        let url = format!("{}/responses", self.base_url);
        let session = body["prompt_cache_key"].as_str().unwrap_or("").to_string();
        let mut refreshed = false;
        let mut relearned = false;
        loop {
            let creds = self.auth.credentials().await?;
            let version = self.identity.current();
            let rb = self
                .request(reqwest::Method::POST, &url, &creds, &version)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header("session-id", &session)
                .json(body);
            let resp = common::connect(rb).await?;
            let mut limits = rate_limits(resp.headers());
            match resp.status().as_u16() {
                401 if !refreshed => {
                    refreshed = true;
                    self.auth.refused(&creds.access_token).await?;
                    continue;
                }
                401 => return Err(CoreError::Provider(SIGN_IN_AGAIN.into())),
                429 => {
                    let wait = common::retry_after(resp.headers());
                    let text = resp.text().await.unwrap_or_default();
                    let err = limit_error(&text, wait, &mut limits, now());
                    self.auth.observe(&limits);
                    return Err(err);
                }
                _ => {}
            }
            if !limits.windows.is_empty() {
                self.auth.observe(&limits);
            }
            let quiet = DeltaSink::new(|_| {});
            let opened = match common::opened_as_stream(resp).await {
                Err(CoreError::Provider(m)) if requires_newer_client(&m) => {
                    if !relearned {
                        relearned = true;
                        if let Some(newer) = self.identity.refresh_past(&version).await {
                            warn!(
                                provider = %self.name, from = %version, to = %newer,
                                "the backend wants a newer Codex client; retrying once as {newer}"
                            );
                            continue;
                        }
                    }
                    return Err(too_old(&version, &m));
                }
                other => other?,
            };
            let reply = match opened {
                common::Opened::Events(events) => {
                    return read_stream(events, sink.unwrap_or(&quiet)).await
                }
                common::Opened::Json(reply) => reply,
            };
            if let Some(err) = reply.body.get("error").filter(|e| !e.is_null()) {
                return Err(common::error_in_body(err, reply.wait));
            }
            return Ok(reply.body);
        }
    }

    /// `GET /models`: the slugs the account may use, in the backend's own
    /// order (by `priority`), hidden ones left out.
    pub async fn list_models(&self) -> Result<Vec<String>, CoreError> {
        let creds = self.auth.credentials().await?;
        let version = self.identity.current();
        let url = format!(
            "{}/models?client_version={}",
            self.base_url,
            version::triple(&version)
        );
        let reply =
            common::send(self.request(reqwest::Method::GET, &url, &creds, &version)).await?;
        let mut models: Vec<(i64, String)> = reply.body["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["visibility"].as_str() != Some("hide"))
            // Offered only to a newer client: the backend would refuse it.
            .filter(|m| {
                m["minimal_client_version"]
                    .as_str()
                    .is_none_or(|min| !version::newer(min, &version))
            })
            .filter_map(|m| {
                Some((
                    m["priority"].as_i64().unwrap_or(i64::MAX),
                    m["slug"].as_str()?.to_string(),
                ))
            })
            .collect();
        models.sort();
        Ok(models.into_iter().map(|(_, s)| s).collect())
    }
}

/// A refusal that learning a newer client version didn't cure.
fn too_old(version: &str, message: &str) -> CoreError {
    let words = match message.find('{') {
        Some(at) => &message[at..],
        None => message,
    };
    Failure {
        kind: Kind::ClientTooOld,
        message: format!(
            "HTTP 400: the ChatGPT backend wants a newer Codex client than {version} for this model: {words}"
        ),
        retry_after: None,
    }
    .into()
}

pub const SIGN_IN_AGAIN: &str =
    "HTTP 401: the ChatGPT plan refused the sign-in even after a refresh; run `ferrule login chatgpt`";

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn sees_images(&self) -> bool {
        self.body.sees()
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let pixels = crate::vision::has_pixels(&req.messages, self.body.sees());
        match self.attempt(&req).await {
            Err(e) if pixels && crate::vision::image_rejected(&e) => {
                warn!(provider = %self.name, "photos refused ({e}); sending notes instead");
                self.body.refuse_images();
                self.attempt(&req).await
            }
            done => done,
        }
    }
}

impl CodexProvider {
    async fn attempt(&self, req: &CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let first = self.payload(req, false);
        let sink = req.stream.as_ref();
        let body = match self.post(&first.0, sink).await {
            Err(CoreError::Provider(m)) if first.1 && reasoning_rejected(&m) => {
                warn!(
                    provider = %self.name,
                    "reasoning items rejected, retrying once without them"
                );
                self.post(&self.payload(req, true).0, sink).await?
            }
            other => other?,
        };
        self.body.parse_response(&body)
    }
}

impl CodexProvider {
    /// The Responses body in the Codex shape: typed input messages, and
    /// the conversation's cache key.
    fn payload(&self, req: &CompletionRequest, plain: bool) -> (Value, bool) {
        let p = self.body.payload(req, plain);
        let mut body = p.body;
        if let Some(items) = body["input"].as_array_mut() {
            for item in items.iter_mut() {
                typed_message(item);
            }
        }
        body["prompt_cache_key"] = json!(cache_key(req, self.body.model()));
        (body, p.replayed)
    }
}

/// `{"role","content":"text"}` → `{"type":"message","role","content":[…]}`,
/// the form the Codex CLI sends.
fn typed_message(item: &mut Value) {
    // Photos: the content is already parts, only the wrapper is missing.
    if item.get("type").is_none() {
        if let (Some(role), Some(parts)) = (
            item.get("role").and_then(Value::as_str).map(str::to_string),
            item.get("content").filter(|c| c.is_array()).cloned(),
        ) {
            *item = json!({"type": "message", "role": role, "content": parts});
            return;
        }
    }
    let (Some(role), Some(text)) = (
        item.get("role").and_then(Value::as_str).map(str::to_string),
        item.get("content")
            .and_then(Value::as_str)
            .map(str::to_string),
    ) else {
        return;
    };
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    *item = json!({
        "type": "message",
        "role": role,
        "content": [{"type": kind, "text": text}],
    });
}

/// A stable key per conversation: its opening (the system prompt and the
/// first user message) and the model. The backend caches the prompt
/// prefix under it, like Codex's session id.
fn cache_key(req: &CompletionRequest, model: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    model.hash(&mut h);
    let mut users = 0;
    for m in &req.messages {
        m.content.hash(&mut h);
        for img in &m.images {
            img.path.hash(&mut h);
        }
        if m.role == ferrule_core::Role::User {
            users += 1;
            if users == 1 {
                break;
            }
        }
    }
    format!("ferrule-{:016x}", h.finish())
}

/// The Codex CLI's shape with its version, then ferrule's own name: the
/// backend reads the client version here too, and it's still honest about
/// what is calling.
fn user_agent(version: &str) -> String {
    format!(
        "{ORIGINATOR}/{version} ({}; {}) ferrule/{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `x-codex-{primary,secondary}-*` headers.
pub fn rate_limits(headers: &reqwest::header::HeaderMap) -> RateLimits {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let mut out = RateLimits::default();
    for which in ["primary", "secondary"] {
        let used = get(&format!("x-codex-{which}-used-percent")).and_then(|v| v.parse().ok());
        let minutes = get(&format!("x-codex-{which}-window-minutes")).and_then(|v| v.parse().ok());
        let resets = get(&format!("x-codex-{which}-reset-at")).and_then(|v| v.parse().ok());
        if let Some(used_percent) = used {
            out.windows.push(LimitWindow {
                minutes,
                used_percent,
                resets_at: resets,
            });
        }
    }
    out
}

/// A 429's error: a usage-limit stop waits for its reset (so the agent's
/// retry budget gives up at once and M21 falls back), a plan without Codex
/// is final, anything else is an ordinary rate limit.
fn limit_error(text: &str, wait: Option<Duration>, limits: &mut RateLimits, now: u64) -> CoreError {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let err = &body["error"];
    let kind = err["type"].as_str().or(err["code"].as_str()).unwrap_or("");
    let plan = err["plan_type"].as_str().map(str::to_string);
    limits.plan = plan.clone();
    let plan_words = plan.map(|p| format!(" ({p})")).unwrap_or_default();
    match kind {
        "usage_limit_reached" => {
            let resets = err["resets_at"].as_u64().or_else(|| {
                err["resets_in_seconds"]
                    .as_u64()
                    .map(|s| now.saturating_add(s))
            });
            limits.limited_until = resets.or(Some(now + 300));
            let when = resets
                .map(|at| format!("; it resets {}", describe_reset(at, now)))
                .unwrap_or_default();
            CoreError::Transient {
                message: format!(
                    "HTTP 429 usage_limit_reached: the ChatGPT plan's usage limit is reached{plan_words}{when}"
                ),
                retry_after: Some(Duration::from_secs(
                    resets.map_or(300, |at| at.saturating_sub(now).max(1)),
                )),
            }
        }
        "usage_not_included" => CoreError::Provider(format!(
            "HTTP 429 usage_not_included: this ChatGPT account's plan{plan_words} doesn't include Codex use"
        )),
        _ => common::transient(
            format!(
                "HTTP 429 {kind}: {}",
                text.chars().take(300).collect::<String>()
            ),
            wait,
        ),
    }
}

/// `in 2 h 4 min (15:10 UTC)`, for a reset at unix `at`.
pub fn describe_reset(at: u64, now: u64) -> String {
    let left = at.saturating_sub(now);
    let (d, h, m) = (left / 86_400, left % 86_400 / 3600, left % 3600 / 60);
    let span = if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{} min", m.max(1))
    };
    let clock = format!("{:02}:{:02} UTC", at % 86_400 / 3600, at % 3600 / 60);
    format!("in {span} ({clock})")
}

#[cfg(test)]
mod tests;
