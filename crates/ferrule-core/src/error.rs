use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("provider error: {0}")]
    Provider(String),
    /// A provider failure that may pass if the call is repeated: a timeout,
    /// a dropped connection, HTTP 408, 429 or 5xx. The agent retries these
    /// with backoff; any other provider error is final.
    #[error("provider temporarily unavailable: {message}")]
    Transient {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    #[error("provider returned malformed response: {0}")]
    MalformedResponse(String),
    #[error("tool `{0}` not found")]
    ToolNotFound(String),
    #[error("tool `{tool}` failed: {message}")]
    ToolFailed { tool: String, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("agent exceeded max iterations ({0})")]
    MaxIterations(usize),
    /// The run was stopped before it finished: it went in circles (see
    /// `crate::stuck`) or its check kept failing, and even the status
    /// answer couldn't be had.
    #[error("agent stopped before finishing: {0}")]
    Stopped(String),
    #[error("run aborted: {0}")]
    Aborted(String),
    /// A provider failure its driver has already recognised (M36): see
    /// [`crate::failure`]. It reads like [`CoreError::Provider`].
    #[error("provider error: {}", .0.message)]
    Failed(crate::failure::Failure),
}

impl CoreError {
    pub fn is_transient(&self) -> bool {
        matches!(self, CoreError::Transient { .. })
    }

    /// The failures a real bot hits most, in words the owner can act on
    /// (M19c); `None` for the rest. The raw error still goes along with it.
    pub fn plain_words(&self) -> Option<String> {
        let text = self.to_string();
        let lower = text.to_ascii_lowercase();
        if lower.contains("no endpoints found that support tool use") {
            return Some(
                "this model has no endpoint on OpenRouter that supports tools, and ferrule needs tools. \
                 Pick another model (a `:free` one is often the cause): /model."
                    .into(),
            );
        }
        // A subscription plan's own limit (M35): the drivers word it
        // `usage_limit_reached: <what, and when it resets>`, and the reset
        // time is the part the owner needs.
        if let Some(at) = text.find("usage_limit_reached: ") {
            let what = &text[at + "usage_limit_reached: ".len()..];
            let what = what.rfind(" (tried ").map_or(what, |end| &what[..end]);
            return Some(format!(
                "{what}. Set `[models] fallback` so another model answers until then."
            ));
        }
        let free_pool = [
            "free-models-per",
            ":free",
            "rate-limited upstream",
            "could not verify available credits",
        ]
        .iter()
        .any(|m| lower.contains(m));
        let rate_limited = lower.contains("http 429")
            || lower.contains("\"code\":429")
            || lower.contains("rate limit")
            || lower.contains("could not verify available credits");
        if !rate_limited {
            return None;
        }
        let tried = retries(&text)
            .map(|n| format!(" I tried {n} times before giving up."))
            .unwrap_or_default();
        Some(if free_pool {
            format!(
                "the model provider is rate-limiting us (HTTP 429). This is OpenRouter's shared pool for free models, \
                 which everyone on a `:free` model draws from.{tried} Try again in a few minutes, or use the paid model id \
                 (without `:free`) or set `[models] fallback` so another model answers when this one is busy."
            )
        } else {
            format!(
                "the model provider is rate-limiting us (HTTP 429).{tried} Try again in a few minutes, \
                 or set `[models] fallback` so another model answers when this one is busy."
            )
        })
    }

    /// The same error, noting that it came after `attempts` tries.
    pub fn after_attempts(self, attempts: u32) -> Self {
        match self {
            CoreError::Transient {
                message,
                retry_after,
            } if attempts > 1 => CoreError::Transient {
                message: format!("{message} (tried {attempts} times)"),
                retry_after,
            },
            other => other,
        }
    }
}

/// What kind of failure a provider error is, for a router that escalates
/// on some and not others (M25) and for a driver that asks again another
/// way after a bad request. It is the coarse view of a
/// [`crate::failure::Kind`], the one taxonomy ([`crate::failure::Kind::class`]),
/// read from the error's structure and HTTP status
/// ([`crate::failure::status_kind`]), never from the text-only kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// HTTP 429.
    RateLimited,
    /// Anthropic's 529, or a 503 or body that says overloaded.
    Overloaded,
    /// Another 5xx.
    Server,
    /// The call timed out (or HTTP 408).
    Timeout,
    /// The provider couldn't be reached.
    Connect,
    /// The key was refused (401/403).
    Auth,
    /// The request was refused as invalid (400/422).
    BadRequest,
    /// The prompt is longer than the model's context window.
    ContextTooLong,
    /// The model name isn't known (404).
    ModelNotFound,
    /// The model declined to answer (a safety refusal).
    Refused,
    /// The provider answered, but not in a shape we understand.
    Malformed,
    /// Anything else, including errors that aren't the provider's.
    Other,
}

impl CoreError {
    /// This error's [`FailureClass`]: what its structure and HTTP status
    /// say. A `400` that names a model or a client version stays a bad
    /// request here; [`crate::failure::classify`] reads those words too.
    pub fn class(&self) -> FailureClass {
        crate::failure::status_kind(self).class()
    }
}

/// N from the "(tried N times)" [`CoreError::after_attempts`] adds.
fn retries(text: &str) -> Option<u32> {
    let rest = &text[text.rfind("(tried ")? + "(tried ".len()..];
    rest.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // OpenRouter's own bodies, as the provider adapter words them.
    const NO_TOOLS: &str = r#"HTTP 404 Not Found: {"error":{"code":404,"message":"No endpoints found that support tool use. To learn more about provider routing, visit: https://openrouter.ai/docs/provider-routing"}}"#;
    const FREE_PER_MIN: &str = r#"HTTP 429 Too Many Requests: {"error":{"code":429,"message":"Rate limit exceeded: free-models-per-min. ","metadata":{"headers":{"X-RateLimit-Limit":"20","X-RateLimit-Remaining":"0"}}}}"#;
    const UPSTREAM: &str = r#"error in an HTTP 200 response: {"code":429,"message":"Provider returned error","metadata":{"provider_name":"Chutes","raw":"qwen/qwen3.8-27b:free is temporarily rate-limited upstream. Please retry shortly, or add your own key to accumulate your rate limits: https://openrouter.ai/settings/integrations"}}"#;

    fn transient(message: &str) -> CoreError {
        CoreError::Transient {
            message: message.into(),
            retry_after: None,
        }
    }

    #[test]
    fn no_tool_endpoint_says_to_pick_another_model() {
        let words = CoreError::Provider(NO_TOOLS.into()).plain_words().unwrap();
        assert!(words.starts_with("this model has no endpoint on OpenRouter that supports tools"));
        assert!(words.contains(":free"));
    }

    #[test]
    fn a_429_names_the_free_pool_and_the_retries() {
        let e = transient(FREE_PER_MIN).after_attempts(4);
        let words = e.plain_words().unwrap();
        assert!(
            words.starts_with("the model provider is rate-limiting us"),
            "{words}"
        );
        assert!(words.contains("shared pool for free models"), "{words}");
        assert!(words.contains("I tried 4 times"), "{words}");
        let upstream = transient(UPSTREAM).plain_words().unwrap();
        assert!(upstream.contains("shared pool"), "{upstream}");
        assert!(
            !upstream.contains("tried"),
            "one try isn't a retry: {upstream}"
        );
    }

    #[test]
    fn a_paid_429_leaves_the_free_pool_out_and_other_errors_have_no_plain_words() {
        let paid = transient(r#"HTTP 429 Too Many Requests: {"error":{"message":"slow down"}}"#)
            .plain_words()
            .unwrap();
        assert!(!paid.contains("free"), "{paid}");
        assert!(transient("HTTP 502 Bad Gateway, not JSON: <html>")
            .plain_words()
            .is_none());
        assert!(CoreError::MaxIterations(3).plain_words().is_none());
    }

    #[test]
    fn a_plans_usage_limit_keeps_its_reset_time() {
        let words = transient(
            "HTTP 429 usage_limit_reached: the ChatGPT plan's usage limit is reached (plus); \
             it resets in 2 h 4 min (15:10 UTC)",
        )
        .after_attempts(1)
        .plain_words()
        .unwrap();
        assert_eq!(
            words,
            "the ChatGPT plan's usage limit is reached (plus); it resets in 2 h 4 min (15:10 UTC). \
             Set `[models] fallback` so another model answers until then."
        );
    }

    #[test]
    fn failure_classes_come_from_the_drivers_error_text() {
        use FailureClass::*;
        let p = |t: &str| CoreError::Provider(t.into()).class();
        let t = |m: &str| transient(m).class();
        assert_eq!(t(FREE_PER_MIN), RateLimited);
        assert_eq!(t(UPSTREAM), RateLimited);
        assert_eq!(
            t(
                r#"HTTP 529 <unknown status code>: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            Overloaded
        );
        assert_eq!(t("HTTP 502 Bad Gateway, not JSON: <html>"), Server);
        assert_eq!(t("HTTP 408 Request Timeout: x"), Timeout);
        assert_eq!(t("request timed out: error sending request"), Timeout);
        assert_eq!(t("could not connect: error sending request"), Connect);
        assert_eq!(p("HTTP 401 Unauthorized: invalid x-api-key"), Auth);
        assert_eq!(p(NO_TOOLS), ModelNotFound);
        assert_eq!(
            p(r#"HTTP 400 Bad Request: {"error":{"message":"bad"}}"#),
            BadRequest
        );
        assert_eq!(
            p("HTTP 400 Bad Request: prompt is too long: 210000 tokens > 200000 maximum"),
            ContextTooLong
        );
        assert_eq!(
            p(r#"HTTP 400 Bad Request: {"error":{"code":"context_length_exceeded"}}"#),
            ContextTooLong
        );
        assert_eq!(p("refused: cyber"), Refused);
        assert_eq!(CoreError::MalformedResponse("x".into()).class(), Malformed);
        assert_eq!(CoreError::MaxIterations(3).class(), Other);
    }

    #[test]
    fn only_a_retried_transient_error_notes_its_attempts() {
        assert_eq!(
            transient("x").after_attempts(3).to_string(),
            "provider temporarily unavailable: x (tried 3 times)"
        );
        assert_eq!(
            transient("x").after_attempts(1).to_string(),
            "provider temporarily unavailable: x"
        );
        assert_eq!(
            CoreError::Provider("y".into())
                .after_attempts(3)
                .to_string(),
            "provider error: y"
        );
    }
}
