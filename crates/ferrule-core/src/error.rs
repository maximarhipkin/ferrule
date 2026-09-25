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
                 Pick another model (a `:free` one is often the cause): /model here, or `ferrule model default`."
                    .into(),
            );
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
