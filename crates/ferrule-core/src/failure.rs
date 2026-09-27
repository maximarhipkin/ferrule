//! M36 self-repair (docs/m36-self-update.md §6): what a failed call *is*,
//! so the loop, the drivers, the router and the owner notices agree on it.
//!
//! A driver that can tell from the reply's structure what went wrong (a
//! status and an error code, a stream event's reason) says so with
//! [`CoreError::Failed`]. Everything else is read from the error's text,
//! the way M21's [`FailureClass`] always has. Each [`Kind`] then says what
//! can repair it, how long the model that hit it stays down, and the plain
//! words for a chat and for the owner.

use crate::error::{CoreError, FailureClass};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// What went wrong, as far as anyone can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The ChatGPT backend wants a newer Codex client version.
    ClientTooOld,
    /// The `claude` CLI is below the version Claude's servers accept.
    ClaudeTooOld,
    /// The `claude` CLI isn't where it was, or its install is broken.
    ClaudeMissing,
    /// The ChatGPT plan's sign-in expired, or its refresh was refused.
    ChatgptSignin,
    /// The Claude plan's sign-in expired or was revoked.
    ClaudeSignin,
    /// A plan's usage limit: nothing more until it resets.
    UsageLimit,
    RateLimited,
    Overloaded,
    Server,
    Timeout,
    Connect,
    /// The model is gone, renamed, or not offered to this account.
    ModelGone,
    ContextTooLong,
    /// An API key was refused.
    Auth,
    Refused,
    BadRequest,
    Malformed,
    DiskFull,
    /// The data dir can't be written (permissions, a read-only mount).
    DataUnwritable,
    Unknown,
}

/// A failure a driver has already recognised.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub kind: Kind,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl Failure {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }
}

impl From<Failure> for CoreError {
    fn from(f: Failure) -> Self {
        CoreError::Failed(f)
    }
}

/// What can be done about a failure without asking anyone. The one who
/// can do it (a driver, the engine, the gateway) does it once, then the
/// call is retried once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Repair {
    None,
    /// Learn the current Codex client version now, bypassing the cache.
    RefreshClientVersion,
    /// Run `claude`'s own updater (or its package manager's).
    UpdateClaude,
    /// Look for `claude` again: on PATH and where installers put it.
    FindClaude,
    /// Ask the provider for its model list again.
    RefreshModels,
}

impl Kind {
    pub fn repair(self) -> Repair {
        match self {
            Kind::ClientTooOld => Repair::RefreshClientVersion,
            Kind::ClaudeTooOld => Repair::UpdateClaude,
            Kind::ClaudeMissing => Repair::FindClaude,
            Kind::ModelGone => Repair::RefreshModels,
            _ => Repair::None,
        }
    }

    /// The snake_case name, for logs and the repair log.
    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// How long a model that failed this way is left alone before it's
    /// tried again; `None` for the M21 default.
    pub fn down_for(self, retry_after: Option<Duration>) -> Option<Duration> {
        let minutes = |m: u64| Some(Duration::from_secs(m * 60));
        match self {
            Kind::UsageLimit => retry_after.or(minutes(60)),
            Kind::ChatgptSignin | Kind::ClaudeSignin | Kind::Auth => minutes(60),
            Kind::ClientTooOld => minutes(60),
            Kind::ClaudeTooOld | Kind::ClaudeMissing => minutes(30),
            Kind::ModelGone => minutes(24 * 60),
            _ => None,
        }
    }

    /// Why a model is down, in a few words: "{model} isn't answering
    /// ({this})".
    pub fn short_reason(self) -> &'static str {
        match self {
            Kind::ClientTooOld => "it wants a newer Codex client version",
            Kind::ClaudeTooOld => "the claude CLI is too old",
            Kind::ClaudeMissing => "the claude CLI is missing",
            Kind::ChatgptSignin | Kind::ClaudeSignin => "its sign-in expired",
            Kind::UsageLimit => "its usage limit is reached",
            Kind::RateLimited => "rate-limited",
            Kind::Overloaded => "overloaded",
            Kind::Server => "a server error",
            Kind::Timeout => "timed out",
            Kind::Connect => "unreachable",
            Kind::ModelGone => "the model is gone",
            Kind::ContextTooLong => "the conversation is too long for it",
            Kind::Auth => "its key was refused",
            Kind::Refused => "it refused",
            Kind::BadRequest => "it refused the request",
            Kind::Malformed => "a garbled reply",
            Kind::DiskFull => "the disk is full",
            Kind::DataUnwritable => "the data dir isn't writable",
            Kind::Unknown => "an error",
        }
    }

    /// What the chat reads when no model could answer.
    pub fn chat_words(self) -> &'static str {
        match self {
            Kind::ClientTooOld => {
                "ChatGPT wants a newer Codex client version for this model, and no other model was free to answer. \
                 I'll try again with the newest version on the next message."
            }
            Kind::ClaudeTooOld => {
                "the claude CLI on the server is too old for Claude's servers and couldn't be updated by itself. \
                 On the server: `claude update`."
            }
            Kind::ClaudeMissing => {
                "the claude CLI isn't installed where I look for it. \
                 `ferrule doctor` on the server says where it went."
            }
            Kind::ChatgptSignin => {
                "the ChatGPT plan's sign-in expired or was revoked. \
                 Sign in again: send /login chatgpt here, or run `ferrule login chatgpt` on the server."
            }
            Kind::ClaudeSignin => {
                "the Claude plan's sign-in expired or was revoked. \
                 Run `ferrule login claude` on the server (Claude's sign-in can't be done from a chat)."
            }
            Kind::UsageLimit => {
                "the plan's usage limit is reached. Set `[models] fallback` so another model answers until it resets."
            }
            Kind::RateLimited => {
                "the model provider is rate-limiting us. Try again in a few minutes, \
                 or set `[models] fallback` so another model answers when this one is busy."
            }
            Kind::Overloaded | Kind::Server | Kind::Timeout | Kind::Connect => {
                "the model provider isn't answering right now. Please try again in a few minutes."
            }
            Kind::ModelGone => {
                "this model isn't available any more (it may have been renamed or retired). \
                 Pick another: /model here, or `ferrule model default` on the server."
            }
            Kind::ContextTooLong => {
                "this conversation is too long for the model. Pick one with a bigger window: /model."
            }
            Kind::Auth => {
                "the model provider refused the API key. `ferrule doctor` on the server checks it, \
                 `ferrule setup` replaces it."
            }
            Kind::Refused => "the model refused to answer this.",
            Kind::DiskFull => {
                "the server's disk is full, so I can't save anything. Free some space on it."
            }
            Kind::DataUnwritable => {
                "I can't write to my data folder (no permission, or it's read-only). \
                 `ferrule doctor` on the server says which."
            }
            Kind::BadRequest | Kind::Malformed | Kind::Unknown => {
                "something went wrong on the way to the model."
            }
        }
    }
}

/// What `e` is: the structured kind when a driver set one, else read from
/// the text.
pub fn classify(e: &CoreError) -> Kind {
    match e {
        CoreError::Failed(f) => f.kind,
        CoreError::Io(io) => classify_io(io).unwrap_or(Kind::Unknown),
        CoreError::MalformedResponse(_) => Kind::Malformed,
        CoreError::Provider(t) | CoreError::Transient { message: t, .. } => {
            classify_text(t).unwrap_or_else(|| from_class(e.class()))
        }
        CoreError::ToolFailed { message, .. } => classify_text(message)
            .filter(|k| matches!(k, Kind::DiskFull | Kind::DataUnwritable))
            .unwrap_or(Kind::Unknown),
        _ => Kind::Unknown,
    }
}

/// An I/O error that is about the machine, not the call.
pub fn classify_io(e: &std::io::Error) -> Option<Kind> {
    use std::io::ErrorKind as E;
    match e.kind() {
        E::StorageFull | E::QuotaExceeded => Some(Kind::DiskFull),
        E::PermissionDenied | E::ReadOnlyFilesystem => Some(Kind::DataUnwritable),
        _ => classify_text(&e.to_string())
            .filter(|k| matches!(k, Kind::DiskFull | Kind::DataUnwritable)),
    }
}

fn from_class(class: FailureClass) -> Kind {
    match class {
        FailureClass::RateLimited => Kind::RateLimited,
        FailureClass::Overloaded => Kind::Overloaded,
        FailureClass::Server => Kind::Server,
        FailureClass::Timeout => Kind::Timeout,
        FailureClass::Connect => Kind::Connect,
        FailureClass::Auth => Kind::Auth,
        FailureClass::BadRequest => Kind::BadRequest,
        FailureClass::ContextTooLong => Kind::ContextTooLong,
        FailureClass::ModelNotFound => Kind::ModelGone,
        FailureClass::Refused => Kind::Refused,
        FailureClass::Malformed => Kind::Malformed,
        FailureClass::Other => Kind::Unknown,
    }
}

/// The kinds the text alone can tell, the specific ones first; `None`
/// leaves it to the status rules.
pub fn classify_text(text: &str) -> Option<Kind> {
    let t = text.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if requires_newer_client(text) {
        return Some(Kind::ClientTooOld);
    }
    if any(&[
        "cli_version_too_old",
        "needs an update. a newer version",
        "your version of claude code",
    ]) {
        return Some(Kind::ClaudeTooOld);
    }
    if any(&[
        "claude code isn't installed",
        "native binary not installed",
        "claude native binary",
    ]) {
        return Some(Kind::ClaudeMissing);
    }
    if any(&["ferrule login chatgpt", "chatgpt plan refused the sign-in"])
        || (t.contains("chatgpt")
            && any(&[
                "expired or was revoked",
                "invalid_grant",
                "refresh_token_reused",
                "refresh_token_expired",
                "refresh_token_invalidated",
            ]))
    {
        return Some(Kind::ChatgptSignin);
    }
    if any(&["ferrule login claude", "claude code isn't signed in"])
        || (t.contains("claude")
            && any(&[
                "expired or was revoked",
                "not logged in",
                "please run /login",
                "oauth token has expired",
                "oauth token revoked",
            ]))
    {
        return Some(Kind::ClaudeSignin);
    }
    if any(&["usage_limit_reached", "usage limit is reached"]) {
        return Some(Kind::UsageLimit);
    }
    if any(&[
        "no space left on device",
        "os error 28",
        "disk quota exceeded",
        "storage full",
    ]) {
        return Some(Kind::DiskFull);
    }
    if any(&["read-only file system", "os error 30"]) {
        return Some(Kind::DataUnwritable);
    }
    if any(&[
        "model_not_found",
        "model not found",
        "does not exist or you do not have access",
        "unknown model",
        "unsupported model",
        "model is not supported",
        "has been deprecated",
        "is no longer available",
        "invalid model",
    ]) {
        return Some(Kind::ModelGone);
    }
    None
}

/// Whether an error from the ChatGPT backend says the client is too old
/// for the model: by shape, not only by the sentence OpenAI uses today.
/// The error object's `code`/`type` naming a client version, a
/// `minimal_client_version` field, or a message about a newer version.
pub fn requires_newer_client(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.contains("requires a newer version")
        || t.contains("newer version of codex")
        || t.contains("upgrade codex")
        || t.contains("update codex")
        || t.contains("minimal_client_version")
        || t.contains("min_client_version")
    {
        return true;
    }
    // `{"error":{"code":"unsupported_client_version", ...}}` and friends.
    let Some(at) = text.find('{') else {
        return false;
    };
    let Ok(body) = serde_json::from_str::<serde_json::Value>(&text[at..]) else {
        return false;
    };
    let err = body.get("error").unwrap_or(&body);
    ["code", "type"].iter().any(|field| {
        err.get(*field)
            .and_then(|v| v.as_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|c| {
                c.contains("version")
                    && (c.contains("client")
                        || c.contains("codex")
                        || c.contains("outdated")
                        || c.contains("unsupported"))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(t: &str) -> CoreError {
        CoreError::Provider(t.into())
    }

    fn transient(t: &str) -> CoreError {
        CoreError::Transient {
            message: t.into(),
            retry_after: None,
        }
    }

    /// Every known shape: the error, its kind, its repair.
    #[test]
    fn every_known_failure_has_its_kind_and_repair() {
        let table: Vec<(CoreError, Kind, Repair)> = vec![
            (
                provider(r#"HTTP 400 Bad Request: {"detail":"The 'gpt-5.6-sol' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again."}"#),
                Kind::ClientTooOld,
                Repair::RefreshClientVersion,
            ),
            (
                provider(r#"HTTP 400 Bad Request: {"error":{"code":"unsupported_client_version","message":"x"}}"#),
                Kind::ClientTooOld,
                Repair::RefreshClientVersion,
            ),
            (
                provider(r#"HTTP 400 Bad Request: {"error":{"type":"invalid_request_error","message":"model gpt-7 needs minimal_client_version 0.170.0"}}"#),
                Kind::ClientTooOld,
                Repair::RefreshClientVersion,
            ),
            (
                Failure::new(Kind::ClientTooOld, "structured").into(),
                Kind::ClientTooOld,
                Repair::RefreshClientVersion,
            ),
            (
                provider("Claude Code ended without an answer (exit 1): It looks like your version of Claude Code (2.1.1) needs an update. A newer version (2.1.200 or higher) is required to continue."),
                Kind::ClaudeTooOld,
                Repair::UpdateClaude,
            ),
            (
                provider("Claude Code: cli_version_too_old"),
                Kind::ClaudeTooOld,
                Repair::UpdateClaude,
            ),
            (
                provider("Claude Code isn't installed here (claude not found): npm install -g @anthropic-ai/claude-code"),
                Kind::ClaudeMissing,
                Repair::FindClaude,
            ),
            (
                provider("Claude Code ended without an answer (exit 1): Error: claude native binary not installed."),
                Kind::ClaudeMissing,
                Repair::FindClaude,
            ),
            (
                provider("HTTP 401: the ChatGPT plan refused the sign-in even after a refresh; run `ferrule login chatgpt`"),
                Kind::ChatgptSignin,
                Repair::None,
            ),
            (
                provider("the ChatGPT plan's sign-in expired or was revoked (invalid_grant); run `ferrule login chatgpt`"),
                Kind::ChatgptSignin,
                Repair::None,
            ),
            (
                provider("Claude Code isn't signed in: run `ferrule login claude` on the server"),
                Kind::ClaudeSignin,
                Repair::None,
            ),
            (
                transient("HTTP 429 usage_limit_reached: the ChatGPT plan's usage limit is reached (plus); it resets in 2 h"),
                Kind::UsageLimit,
                Repair::None,
            ),
            (
                transient("usage_limit_reached: the Claude plan's usage limit is reached (5-hour)"),
                Kind::UsageLimit,
                Repair::None,
            ),
            (
                transient(r#"HTTP 429 Too Many Requests: {"error":{"message":"slow down"}}"#),
                Kind::RateLimited,
                Repair::None,
            ),
            (
                transient(r#"HTTP 529 <unknown status code>: {"error":{"type":"overloaded_error"}}"#),
                Kind::Overloaded,
                Repair::None,
            ),
            (transient("HTTP 502 Bad Gateway, not JSON: <html>"), Kind::Server, Repair::None),
            (transient("request timed out: x"), Kind::Timeout, Repair::None),
            (transient("could not connect: x"), Kind::Connect, Repair::None),
            (
                provider(r#"HTTP 404 Not Found: {"error":{"code":"model_not_found","message":"The model `gpt-4.2` does not exist or you do not have access to it."}}"#),
                Kind::ModelGone,
                Repair::RefreshModels,
            ),
            (
                provider(r#"HTTP 400 Bad Request: {"detail":"The 'gpt-5.1' model is not supported when using Codex with a ChatGPT account."}"#),
                Kind::ModelGone,
                Repair::RefreshModels,
            ),
            (
                provider("HTTP 400 Bad Request: prompt is too long: 210000 tokens > 200000 maximum"),
                Kind::ContextTooLong,
                Repair::None,
            ),
            (
                provider("HTTP 401 Unauthorized: invalid x-api-key"),
                Kind::Auth,
                Repair::None,
            ),
            (provider("refused: cyber"), Kind::Refused, Repair::None),
            (
                CoreError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull)),
                Kind::DiskFull,
                Repair::None,
            ),
            (
                CoreError::Io(std::io::Error::from_raw_os_error(28)),
                Kind::DiskFull,
                Repair::None,
            ),
            (
                CoreError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                Kind::DataUnwritable,
                Repair::None,
            ),
            (
                CoreError::ToolFailed {
                    tool: "write".into(),
                    message: "No space left on device (os error 28)".into(),
                },
                Kind::DiskFull,
                Repair::None,
            ),
            (
                CoreError::MalformedResponse("x".into()),
                Kind::Malformed,
                Repair::None,
            ),
            (provider("something new and strange"), Kind::Unknown, Repair::None),
            (CoreError::MaxIterations(3), Kind::Unknown, Repair::None),
        ];
        for (e, kind, repair) in table {
            assert_eq!(classify(&e), kind, "{e}");
            assert_eq!(kind.repair(), repair, "{e}");
            assert!(!kind.chat_words().is_empty());
            assert!(!kind.short_reason().is_empty());
        }
    }

    #[test]
    fn the_owner_is_told_how_to_sign_in_where_that_works() {
        assert!(Kind::ChatgptSignin.chat_words().contains("/login chatgpt"));
        assert!(Kind::ChatgptSignin
            .chat_words()
            .contains("ferrule login chatgpt"));
        // Claude's sign-in never works from a chat.
        assert!(!Kind::ClaudeSignin.chat_words().contains("/login"));
        assert!(Kind::ClaudeSignin
            .chat_words()
            .contains("ferrule login claude"));
    }

    #[test]
    fn a_structured_failure_reads_like_a_provider_error() {
        let e: CoreError = Failure::new(Kind::ClientTooOld, "HTTP 400: too old").into();
        assert_eq!(e.to_string(), "provider error: HTTP 400: too old");
        assert!(!e.is_transient());
    }

    #[test]
    fn down_times_follow_the_kind() {
        let hour = Duration::from_secs(3600);
        assert_eq!(
            Kind::UsageLimit.down_for(Some(Duration::from_secs(90))),
            Some(Duration::from_secs(90))
        );
        assert_eq!(Kind::UsageLimit.down_for(None), Some(hour));
        assert_eq!(Kind::ModelGone.down_for(None), Some(hour * 24));
        assert_eq!(Kind::Server.down_for(None), None);
        assert_eq!(Kind::ClientTooOld.name(), "client_too_old");
    }

    #[test]
    fn an_ordinary_bad_request_is_not_a_client_version() {
        assert!(!requires_newer_client(
            r#"HTTP 400 Bad Request: {"error":{"code":"invalid_value","message":"bad"}}"#
        ));
        assert!(!requires_newer_client("HTTP 400, not JSON: <html>"));
    }
}
