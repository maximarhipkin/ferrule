//! Signal adapter (M39 §6), through signal-cli's daemon: what comes in is
//! its Server-Sent Events stream (`GET /api/v1/events`, `events`); what
//! goes out is JSON-RPC (`POST /api/v1/rpc`). Ferrule neither vendors nor
//! downloads signal-cli: it talks to a daemon the owner runs (`url`), or
//! starts the one on PATH itself and keeps it running (`daemon`).
//!
//! Who gets in: a DM from an allow-listed number (or ACI uuid, when the
//! number is hidden); a group on `allowed_groups` when the account is
//! mentioned or replied to. On a number linked to the owner's own phone,
//! their "Note to Self" reaches the agent when their number is allowed.
//!
//! No buttons (approvals are the reply keyword) and no edits (no
//! streaming). Markdown goes out as Signal's own text styles (`style`).

mod daemon;
mod events;
pub mod style;

use crate::channel::{Channel, ChannelCapabilities};
use crate::channels::access::Access;
use crate::channels::files::{self, Inbox};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use crate::stream::chunks;
use base64::Engine;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// One message's text: Signal's clients cut a longer body into an
/// attachment, which signal-cli doesn't make.
pub const MESSAGE_LIMIT: usize = 2_000;
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    backoff_min: Duration,
    backoff_max: Duration,
    /// How often an open event stream is checked with a `version` call.
    ping: Duration,
    /// A daemon up this long has started well: its failures count afresh.
    healthy: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            ping: Duration::from_secs(60),
            healthy: Duration::from_secs(60),
        }
    }
}

/// The daemon ferrule starts, when the owner doesn't run one.
#[derive(Clone, Debug)]
pub struct Daemon {
    /// `signal-cli` (resolved on PATH by the CLI).
    pub program: PathBuf,
    /// Its HTTP port on 127.0.0.1.
    pub port: u16,
    /// Where its output goes (`<data>/gateway/signal/daemon.log`).
    pub log: Option<PathBuf>,
}

impl Daemon {
    /// The command line: the account, the HTTP endpoint on loopback, and
    /// receiving only while ferrule listens (what arrives meanwhile waits
    /// on Signal's servers). No secret is ever on it.
    pub fn args(&self, account: &str) -> Vec<String> {
        vec![
            "-a".into(),
            account.into(),
            "daemon".into(),
            "--http".into(),
            format!("127.0.0.1:{}", self.port),
            "--receive-mode".into(),
            "on-connection".into(),
            "--no-receive-stdout".into(),
        ]
    }
}

/// Everything the adapter needs.
#[derive(Clone, Debug)]
pub struct SignalConfig {
    /// The registered or linked number, `+972…`.
    pub account: String,
    /// The daemon's HTTP base, `http://127.0.0.1:7583`.
    pub url: String,
    /// `Some`: ferrule starts the daemon at `url` itself.
    pub daemon: Option<Daemon>,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
}

/// A JSON-RPC failure.
#[derive(Debug, Clone)]
struct RpcError {
    /// JSON-RPC's code; 0 when the daemon wasn't reached.
    code: i64,
    message: String,
}

impl RpcError {
    fn transport(url: &str, e: reqwest::Error) -> Self {
        let why = if e.is_connect() {
            format!("nothing answers at {url}: is the signal-cli daemon running?")
        } else {
            format!("signal-cli at {url} failed: {}", e.without_url())
        };
        Self {
            code: 0,
            message: why,
        }
    }

    /// signal-cli's RATE_LIMIT_ERROR, or a message saying so.
    fn rate_limited(&self) -> bool {
        self.code == -5 || self.message.to_ascii_lowercase().contains("rate limit")
    }
}

pub struct SignalChannel {
    cfg: SignalConfig,
    base: String,
    client: reqwest::Client,
    /// For the event stream: no deadline on the whole answer.
    stream_client: reqwest::Client,
    access: Access,
    timing: Timing,
    rpc_id: AtomicU64,
    /// The daemon serves several accounts: every call names ours.
    multi: AtomicBool,
    /// Our ACI uuid, once seen (mentions may carry only that).
    me_uuid: Mutex<Option<String>>,
    last_poll: Mutex<Option<SystemTime>>,
    events_problem: Mutex<Option<String>>,
    daemon_problem: Mutex<Option<String>>,
}

impl SignalChannel {
    pub fn new(cfg: SignalConfig) -> Self {
        let base = cfg.url.trim_end_matches('/').to_string();
        Self {
            client: crate::channels::ws::http_client(&base, CONNECT_DEADLINE, REQUEST_DEADLINE),
            stream_client: crate::channels::ws::http_client(
                &base,
                CONNECT_DEADLINE,
                Duration::from_secs(30 * 24 * 3600),
            ),
            base,
            access: Self::access(vec![], vec![]),
            timing: Timing::default(),
            rpc_id: AtomicU64::new(1),
            multi: AtomicBool::new(false),
            me_uuid: Mutex::new(None),
            last_poll: Mutex::new(None),
            events_problem: Mutex::new(None),
            daemon_problem: Mutex::new(None),
            cfg,
        }
    }

    fn access(users: Vec<String>, groups: Vec<String>) -> Access {
        Access::new("signal", "Signal number", users, groups).with_keys(
            "[gateway.signal] allowed_users",
            "[gateway.signal] allowed_groups",
        )
    }

    /// Who may DM it (`+972…` or an ACI uuid), and the groups (base64 ids)
    /// where a mention reaches it.
    pub fn with_allowed(mut self, users: Vec<String>, groups: Vec<String>) -> Self {
        self.access = Self::access(users, groups);
        self
    }

    /// Setup only: the first DM that is exactly `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(&mut self.access, Self::access(vec![], vec![]));
        self.access = access.with_pairing(code);
        self
    }

    /// Who paired during setup, `(number or uuid, name)`.
    pub fn paired(&self) -> Option<(String, String)> {
        self.access.paired()
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            ping: Duration::from_millis(200),
            healthy: Duration::from_millis(500),
        };
        self
    }

    /// One JSON-RPC call; its `result`. A daemon serving several accounts
    /// is noticed on the first refusal and named ours from then on.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        loop {
            let mut p = params.clone();
            let named = self.multi.load(Ordering::Relaxed);
            if named {
                p["account"] = json!(self.cfg.account);
            }
            match self.rpc_once(method, p).await {
                Err(e)
                    if !named
                        && e.code != 0
                        && e.message.to_ascii_lowercase().contains("account") =>
                {
                    self.multi.store(true, Ordering::Relaxed);
                }
                r => return r,
            }
        }
    }

    async fn rpc_once(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.rpc_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": id });
        let url = format!("{}/api/v1/rpc", self.base);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| RpcError::transport(&self.base, e))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| RpcError::transport(&self.base, e))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let message = err["message"]
                .as_str()
                .map(|m| crate::health::clip(m, 300))
                .unwrap_or_else(|| "no message".into());
            return Err(RpcError {
                code: err["code"].as_i64().unwrap_or(-1),
                message: format!("signal-cli {method} failed: {message}"),
            });
        }
        if !status.is_success() {
            return Err(RpcError {
                code: -1,
                message: format!(
                    "signal-cli {method} failed: status {status}{}",
                    if status.as_u16() == 404 {
                        format!(
                            " ({} doesn't look like signal-cli's HTTP daemon)",
                            self.base
                        )
                    } else {
                        String::new()
                    }
                ),
            });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    fn explain(&self, e: RpcError) -> GatewayError {
        if e.rate_limited() {
            return GatewayError::RateLimited {
                retry_after: self.timing.backoff_max,
            };
        }
        GatewayError::Channel(e.message)
    }

    /// Where a chat's messages go: a number or uuid, else a group.
    fn target(&self, chat: &str) -> Value {
        if is_group(chat) {
            json!({ "groupId": chat })
        } else {
            json!({ "recipient": [chat] })
        }
    }

    /// Text and files: all of `msg`; the last message's id.
    async fn deliver(&self, msg: &OutboundMessage) -> Result<Option<String>, GatewayError> {
        let mut files = vec![];
        for att in &msg.attachments {
            let (name, mime, bytes) = files::read_outgoing(att)?;
            let mime = if att.kind.contains('/') {
                att.kind.clone()
            } else {
                mime.to_string()
            };
            files.push(format!(
                "data:{mime};filename={};base64,{}",
                files::safe_name(&name),
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ));
        }
        let mut pieces: Vec<String> = chunks(&msg.text, MESSAGE_LIMIT)
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .collect();
        if pieces.is_empty() && !files.is_empty() {
            pieces.push(String::new());
        }
        let mut quote = msg.reply_to.as_deref().and_then(parse_id);
        let mut last = None;
        for piece in pieces {
            let (text, styles) = style::render(&piece);
            let mut params = self.target(&msg.chat_id);
            params["message"] = json!(text);
            if !styles.is_empty() {
                params["textStyle"] = json!(styles);
            }
            if !files.is_empty() {
                params["attachments"] = json!(std::mem::take(&mut files));
            }
            if let Some((ts, author)) = quote.take() {
                params["quoteTimestamp"] = json!(ts);
                params["quoteAuthor"] = json!(author);
            }
            let r = self
                .rpc("send", params)
                .await
                .map_err(|e| self.explain(e))?;
            check_sent(&r, &msg.chat_id)?;
            last = r["timestamp"]
                .as_i64()
                .map(|ts| format!("{ts}:{}", self.cfg.account));
        }
        Ok(last)
    }

    /// A short answer from the adapter itself (pairing, a refused file).
    async fn tell(&self, chat: &str, text: String) {
        let mut params = self.target(chat);
        params["message"] = json!(text);
        if let Err(e) = self.rpc("send", params).await {
            tracing::warn!("signal: couldn't answer {chat}: {}", e.message);
        }
    }
}

/// A group's id is base64; a person is `+…` or an ACI uuid.
pub fn is_group(chat: &str) -> bool {
    !chat.starts_with('+') && uuid::Uuid::parse_str(chat).is_err()
}

/// A message id is `<timestamp>:<author>`: what a reaction, receipt or
/// quote needs to point at a message.
fn parse_id(id: &str) -> Option<(i64, String)> {
    let (ts, author) = id.split_once(':')?;
    Some((ts.parse().ok()?, author.to_string()))
}

/// A send's per-recipient results: an error when nobody got it.
fn check_sent(r: &Value, chat: &str) -> Result<(), GatewayError> {
    let Some(results) = r["results"].as_array().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    if results.iter().any(|x| x["type"] == "SUCCESS") {
        return Ok(());
    }
    let kind = results[0]["type"].as_str().unwrap_or("FAILURE");
    let why = match kind {
        "UNREGISTERED_FAILURE" => format!("{chat} isn't on Signal"),
        "IDENTITY_FAILURE" => format!(
            "{chat}'s safety number changed; trust it with `signal-cli -a <account> trust -a {chat}`"
        ),
        "RATE_LIMIT_FAILURE" => {
            return Err(GatewayError::RateLimited {
                retry_after: Duration::from_secs(60),
            })
        }
        other => format!("{chat}: {other}"),
    };
    Err(GatewayError::Channel(format!(
        "signal: not delivered: {why}"
    )))
}

/// What the probe learned about the daemon and the account.
#[derive(Debug, Clone)]
pub struct Probe {
    pub account: String,
    pub version: String,
    /// The account's groups, `(id, name)`.
    pub groups: Vec<(String, String)>,
}

impl Probe {
    /// "+972… · signal-cli 0.13.4 · 2 groups".
    pub fn summary(&self) -> String {
        format!(
            "{} · signal-cli {} · {} group{}",
            self.account,
            self.version,
            self.groups.len(),
            if self.groups.len() == 1 { "" } else { "s" }
        )
    }
}

/// Asks the daemon at `url` its version and the account's groups: nothing
/// is sent.
pub async fn probe(url: &str, account: &str) -> Result<Probe, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "the daemon's address is a URL like http://127.0.0.1:7583, not {url:?}"
        ));
    }
    let ch = SignalChannel::new(SignalConfig {
        account: account.to_string(),
        url: url.to_string(),
        daemon: None,
        inbox: None,
    });
    let v = ch.rpc("version", json!({})).await.map_err(|e| e.message)?;
    let version = v["version"].as_str().unwrap_or("?").to_string();
    let g = ch
        .rpc("listGroups", json!({}))
        .await
        .map_err(|e| e.message)?;
    let groups = g
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|g| g["isMember"] != false)
                .filter_map(|g| {
                    Some((
                        g["id"].as_str()?.to_string(),
                        g["name"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Probe {
        account: account.to_string(),
        version,
        groups,
    })
}

#[async_trait::async_trait]
impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: true,
            edits: false,
            attachments: true,
            buttons: false,
        }
    }

    fn polls(&self) -> bool {
        true
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        self.daemon_problem
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.events_problem.lock().unwrap().clone())
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        match self.cfg.daemon.clone() {
            Some(d) => {
                tokio::select! {
                    r = self.run_events(tx) => r,
                    _ = self.supervise(&d) => Ok(()),
                }
            }
            None => self.run_events(tx).await,
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.deliver(&msg).await.map(|_| ())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver(&msg).await
    }

    /// The 👀: a read receipt to the author, and the reaction.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let Some((ts, author)) = parse_id(message_id) else {
            return Ok(());
        };
        if author != self.cfg.account {
            let receipt = json!({ "recipient": author, "targetTimestamp": [ts], "type": "read" });
            if let Err(e) = self.rpc("sendReceipt", receipt).await {
                tracing::debug!("signal: read receipt: {}", e.message);
            }
        }
        let mut params = self.target(chat_id);
        params["emoji"] = json!(emoji);
        params["targetAuthor"] = json!(author);
        params["targetTimestamp"] = json!(ts);
        self.rpc("sendReaction", params)
            .await
            .map(|_| ())
            .map_err(|e| self.explain(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_and_ids() {
        assert!(!is_group("+972501234567"));
        assert!(!is_group("a1b2c3d4-0000-4000-8000-000000000001"));
        assert!(is_group("kKPJ+Qm3yK8Qn0bM1F2dA2l8Q3mK0yQ4pD5e6f7g8h0="));
        assert_eq!(
            parse_id("1760000000123:+97250"),
            Some((1_760_000_000_123, "+97250".into()))
        );
        assert_eq!(parse_id("nope"), None);
    }

    #[test]
    fn the_daemon_listens_on_loopback_only_and_waits_for_us() {
        let d = Daemon {
            program: "signal-cli".into(),
            port: 7583,
            log: None,
        };
        let a = d.args("+97250");
        assert_eq!(a[..3], ["-a", "+97250", "daemon"]);
        assert!(a.windows(2).any(|w| w == ["--http", "127.0.0.1:7583"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["--receive-mode", "on-connection"]));
    }

    #[test]
    fn a_send_nobody_got_is_an_error_in_words() {
        assert!(check_sent(&json!({"timestamp": 1}), "+1").is_ok());
        let ok = json!({"results": [{"type": "UNREGISTERED_FAILURE"}, {"type": "SUCCESS"}]});
        assert!(check_sent(&ok, "g").is_ok());
        let e = check_sent(
            &json!({"results": [{"type": "UNREGISTERED_FAILURE"}]}),
            "+1",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("+1 isn't on Signal"), "{e}");
        assert!(matches!(
            check_sent(&json!({"results": [{"type": "RATE_LIMIT_FAILURE"}]}), "+1"),
            Err(GatewayError::RateLimited { .. })
        ));
    }
}
