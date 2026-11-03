//! What Cloud Code's error replies mean for the Gemini plan, in the words
//! the failure taxonomy reads (`ferrule_core::failure`): a 429 that is the
//! plan's usage limit (and when it resets) or only a short rate limit, a
//! 403 that says the account has no tier, a 401 the refresh didn't cure,
//! and the same error objects inside a stream.

use crate::codex::describe_reset;
use crate::common;
use ferrule_core::error::CoreError;
use ferrule_core::failure::{Failure, Kind};
use serde_json::Value;
use std::time::Duration;

/// Google's reply refused the sign-in even after a refresh.
pub const SIGN_IN_AGAIN: &str =
    "HTTP 401: your Google sign-in for the Gemini plan was refused even after a refresh: \
     run `ferrule login gemini` or sign in again on the dashboard's Models page";

/// A wait longer than this is the plan's limit, not a rate limit: no
/// retry would sit it out (the agent's retry budget is 2 minutes).
const LIMIT_WAIT: Duration = Duration::from_secs(120);

/// With no reset time in the reply, how long a usage limit is assumed to last.
const UNKNOWN_RESET: u64 = 300;

/// The [`SIGN_IN_AGAIN`] failure.
pub fn sign_in_again() -> CoreError {
    Failure::new(Kind::GeminiSignin, SIGN_IN_AGAIN).into()
}

/// The `error` object of a Google error reply: `{"error": {...}}`, or the
/// first element of a JSON array of them (the non-SSE stream shape).
fn error_object(body: &Value) -> &Value {
    let body = match body {
        Value::Array(items) => items.first().unwrap_or(&Value::Null),
        other => other,
    };
    body.get("error").unwrap_or(body)
}

fn details(err: &Value) -> impl Iterator<Item = &Value> {
    err["details"].as_array().into_iter().flatten()
}

fn detail_type(d: &Value) -> &str {
    d["@type"].as_str().unwrap_or("")
}

/// Google's own message, cut short, with nothing but printable text.
fn google_words(err: &Value) -> String {
    let message = err["message"].as_str().unwrap_or("").trim();
    let status = err["status"].as_str().unwrap_or("").trim();
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect();
    match (status.is_empty(), message.is_empty()) {
        (true, true) => "no message".into(),
        (false, true) => status.to_string(),
        (true, false) => message,
        (false, false) => format!("{status}: {message}"),
    }
}

/// `3.5s` (protobuf `Duration`) or `1h2m3.5s` / `500ms` (Go's), the shapes
/// Google's `RetryInfo.retryDelay` and `quotaResetDelay` take.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let s = text.trim().trim_end_matches(['.', ',', ';']);
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0f64;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_len == 0 {
            return None;
        }
        let value: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            "ns" => 0.000_000_001,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += value * scale;
    }
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total))
}

/// "…reset after 2h3m4s." or "…retry in 34.07s." in Google's message.
fn delay_in_message(message: &str) -> Option<Duration> {
    let lower = message.to_ascii_lowercase();
    ["reset after ", "retry in ", "retry after ", "resets in "]
        .iter()
        .find_map(|lead| {
            let at = lower.find(lead)? + lead.len();
            parse_duration(lower[at..].split_whitespace().next()?)
        })
}

/// What a 429's error object says about the limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Limit {
    /// The plan's quota is used up (not a short rate limit).
    pub exhausted: bool,
    /// When it resets (unix seconds), if the reply said.
    pub resets_at: Option<u64>,
    /// The model the quota is for, if the reply named it.
    pub model: Option<String>,
}

pub(crate) fn read_limit(err: &Value, header_wait: Option<Duration>, now: u64) -> Limit {
    let mut reason = None;
    let mut model = None;
    let mut reset_stamp = None;
    let mut reset_delay = None;
    let mut retry_delay = None;
    let mut quota_failure = false;
    for d in details(err) {
        let ty = detail_type(d);
        if ty.ends_with("google.rpc.ErrorInfo") {
            reason = d["reason"].as_str().map(str::to_string);
            let meta = &d["metadata"];
            model = meta["model"].as_str().map(str::to_string);
            reset_stamp = meta["quotaResetTimeStamp"]
                .as_str()
                .and_then(super::parse_rfc3339_unix);
            reset_delay = meta["quotaResetDelay"].as_str().and_then(parse_duration);
        } else if ty.ends_with("google.rpc.RetryInfo") {
            retry_delay = d["retryDelay"].as_str().and_then(parse_duration);
        } else if ty.ends_with("google.rpc.QuotaFailure") {
            quota_failure = true;
        }
    }
    let message = err["message"].as_str().unwrap_or("");
    let wait = reset_delay
        .or(retry_delay)
        .or_else(|| delay_in_message(message))
        .or(header_wait);
    let resets_at = reset_stamp.or_else(|| wait.map(|w| now + w.as_secs_f64().ceil() as u64));
    let lower = message.to_ascii_lowercase();
    let exhausted = match reason.as_deref() {
        Some("QUOTA_EXHAUSTED") => true,
        Some("RATE_LIMIT_EXCEEDED" | "MODEL_CAPACITY_EXHAUSTED") => {
            wait.is_some_and(|w| w > LIMIT_WAIT)
        }
        _ => {
            quota_failure
                || reset_stamp.is_some()
                || reset_delay.is_some()
                || wait.is_some_and(|w| w > LIMIT_WAIT)
                || lower.contains("exhausted your capacity")
                || lower.contains("quota will reset")
                || lower.contains("quota exceeded")
        }
    };
    Limit {
        exhausted,
        resets_at,
        model,
    }
}

/// A 429's error. The plan's usage limit says so and when it resets (so
/// the agent's retry budget gives up at once and M21 falls back, as the
/// ChatGPT plan's does); a short rate limit is retried after Google's wait.
pub(crate) fn too_many(
    text: &str,
    header_wait: Option<Duration>,
    model: &str,
    now: u64,
) -> CoreError {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    limit_error(error_object(&body), header_wait, model, now)
}

fn limit_error(err: &Value, header_wait: Option<Duration>, model: &str, now: u64) -> CoreError {
    let limit = read_limit(err, header_wait, now);
    if !limit.exhausted {
        let wait = limit
            .resets_at
            .map(|at| Duration::from_secs(at.saturating_sub(now).max(1)));
        return common::transient(format!("HTTP 429: {}", google_words(err)), wait);
    }
    let model = limit.model.as_deref().unwrap_or(model);
    let when = match limit.resets_at {
        Some(at) => format!("; it resets {}", describe_reset(at, now)),
        None => "; Google didn't say when it resets".into(),
    };
    let left = limit
        .resets_at
        .map_or(UNKNOWN_RESET, |at| at.saturating_sub(now).max(1));
    CoreError::Transient {
        message: format!(
            "HTTP 429 usage_limit_reached: the Gemini plan's usage limit is reached for {model}{when}"
        ),
        retry_after: Some(Duration::from_secs(left)),
    }
}

/// When a usage-limit error from [`too_many`] says the plan resets.
pub(crate) fn limited_until(e: &CoreError, now: u64) -> Option<u64> {
    match e {
        CoreError::Transient {
            message,
            retry_after: Some(wait),
        } if message.contains("usage_limit_reached") => Some(now + wait.as_secs()),
        _ => None,
    }
}

/// A 403 from Cloud Code. The token is good (a refused one is a 401), so
/// it is the account: no Gemini Code Assist / Antigravity tier, no Cloud
/// Code project, or not onboarded. A token missing a scope is a sign-in
/// to do again.
pub(crate) fn forbidden(text: &str, project: Option<&str>) -> CoreError {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let err = error_object(&body);
    let words = if body.is_null() {
        let start: String = text
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        if start.is_empty() {
            "no message".to_string()
        } else {
            start
        }
    } else {
        google_words(err)
    };
    let scope = details(err)
        .any(|d| d["reason"].as_str() == Some("ACCESS_TOKEN_SCOPE_INSUFFICIENT"))
        || words
            .to_ascii_lowercase()
            .contains("insufficient authentication scopes");
    if scope {
        return Failure::new(
            Kind::GeminiSignin,
            format!(
                "HTTP 403: your Google sign-in for the Gemini plan lacks a permission Cloud Code needs ({words}): \
                 run `ferrule login gemini` or sign in again on the dashboard's Models page"
            ),
        )
        .into();
    }
    let project = match project {
        Some(p) => format!("project {p}"),
        None => "Google gave it no Cloud Code project".into(),
    };
    Failure::new(
        Kind::GeminiNoTier,
        format!(
            "HTTP 403: this Google account has no Gemini Code Assist / Antigravity tier the Gemini plan can use \
             ({project}; Google said {words}). Sign in with an account that has one: \
             `ferrule login gemini` or the dashboard's Models page"
        ),
    )
    .into()
}

/// An error object inside a 200 stream: the same failure its status
/// would be as a reply.
pub(crate) fn in_stream(err: &Value, model: &str, now: u64) -> CoreError {
    match err["code"].as_u64() {
        Some(429) => limit_error(err, None, model, now),
        Some(403) => forbidden(&serde_json::json!({ "error": err }).to_string(), None),
        Some(401) => sign_in_again(),
        _ => common::error_in_body(err, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::failure::{classify, retry_after};
    use serde_json::json;

    const NOW: u64 = 1_793_700_000;

    #[test]
    fn durations_read_in_google_and_go_shapes() {
        let s = |t: &str| parse_duration(t).map(|d| d.as_secs_f64());
        assert_eq!(s("3.5s"), Some(3.5));
        assert_eq!(s("7384s"), Some(7384.0));
        assert_eq!(s("1h2m3s"), Some(3723.0));
        assert_eq!(s("2h"), Some(7200.0));
        assert_eq!(s("500ms"), Some(0.5));
        assert_eq!(s("34.07s."), Some(34.07));
        assert_eq!(s("soon"), None);
        assert_eq!(s(""), None);
        assert_eq!(
            delay_in_message(
                "You have exhausted your capacity. Your quota will reset after 2h3m4s."
            ),
            Some(Duration::from_secs(7384))
        );
    }

    #[test]
    fn a_quota_exhausted_429_is_the_plans_limit_with_its_reset_time() {
        let body = json!({"error": {
            "code": 429,
            "message": "You have exhausted your capacity on this model. Your quota will reset after 2h3m4s.",
            "status": "RESOURCE_EXHAUSTED",
            "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED",
                 "domain": "cloudcode-pa.googleapis.com",
                 "metadata": {"model": "gemini-3.8-flash-tiered", "quotaResetDelay": "2h3m4s"}},
                {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "7384s"}
            ]
        }});
        let e = too_many(&body.to_string(), None, "fallback-model", NOW);
        let text = e.to_string();
        assert!(
            text.contains("usage_limit_reached: the Gemini plan's usage limit is reached for gemini-3.8-flash-tiered; it resets in 2 h 3 min"),
            "{text}"
        );
        assert_eq!(classify(&e), Kind::UsageLimit);
        assert_eq!(retry_after(&e), Some(Duration::from_secs(7384)));
        assert_eq!(limited_until(&e, NOW), Some(NOW + 7384));
        let words = e.plain_words().unwrap();
        assert!(
            words.starts_with("the Gemini plan's usage limit is reached for gemini-3.8-flash-tiered; it resets in 2 h 3 min"),
            "{words}"
        );
    }

    #[test]
    fn a_reset_timestamp_wins_and_an_unknown_reset_says_so() {
        let stamp = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED",
                "metadata": {"quotaResetTimeStamp": "2026-11-03T22:39:16Z"}}]}});
        let limit = read_limit(error_object(&stamp), None, NOW);
        assert!(limit.exhausted);
        assert_eq!(
            limit.resets_at,
            super::super::parse_rfc3339_unix("2026-11-03T22:39:16Z")
        );

        let bare = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
            "details": [{"@type": "type.googleapis.com/google.rpc.QuotaFailure",
                "violations": [{"quotaId": "PerDay"}]}]}});
        let e = too_many(&bare.to_string(), None, "gemini-4-argon", NOW);
        assert!(
            e.to_string().contains("Google didn't say when it resets"),
            "{e}"
        );
        assert_eq!(classify(&e), Kind::UsageLimit);
        assert_eq!(retry_after(&e), Some(Duration::from_secs(UNKNOWN_RESET)));
    }

    #[test]
    fn a_short_rate_limit_is_retried_after_googles_wait() {
        let body = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
        "message": "Too many requests, please retry in 3.2s.",
        "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"},
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "3.2s"}
        ]}});
        let e = too_many(&body.to_string(), None, "m", NOW);
        assert!(e.is_transient());
        assert_eq!(classify(&e), Kind::RateLimited);
        assert_eq!(retry_after(&e), Some(Duration::from_secs(4)));
        assert_eq!(limited_until(&e, NOW), None);
        assert!(
            e.to_string()
                .contains("HTTP 429: RESOURCE_EXHAUSTED: Too many requests"),
            "{e}"
        );

        // Not JSON at all: still a rate limit, with the header's wait.
        let e = too_many(
            "<html>slow down</html>",
            Some(Duration::from_secs(9)),
            "m",
            NOW,
        );
        assert_eq!(classify(&e), Kind::RateLimited);
        assert_eq!(retry_after(&e), Some(Duration::from_secs(9)));
    }

    #[test]
    fn a_403_says_the_account_has_no_tier_and_a_scope_403_says_sign_in() {
        let body = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "The caller does not have permission"}});
        let e = forbidden(&body.to_string(), None);
        assert_eq!(classify(&e), Kind::GeminiNoTier);
        let text = e.to_string();
        assert!(
            text.contains("no Gemini Code Assist / Antigravity tier"),
            "{text}"
        );
        assert!(
            text.contains("Google gave it no Cloud Code project"),
            "{text}"
        );
        assert!(
            text.contains("PERMISSION_DENIED: The caller does not have permission"),
            "{text}"
        );
        assert!(!text.contains("API key"), "{text}");

        let e = forbidden(&body.to_string(), Some("companion-1"));
        assert!(e.to_string().contains("project companion-1"), "{e}");

        let scope = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "Request had insufficient authentication scopes.",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}});
        assert_eq!(
            classify(&forbidden(&scope.to_string(), None)),
            Kind::GeminiSignin
        );
        assert_eq!(classify(&forbidden("", None)), Kind::GeminiNoTier);
    }

    #[test]
    fn a_refused_refresh_is_the_google_sign_in_not_an_api_key() {
        let e = sign_in_again();
        assert_eq!(classify(&e), Kind::GeminiSignin);
        assert!(!e.is_transient());
        // Read back from its text alone (a ledger line, a relayed error).
        assert_eq!(
            classify(&CoreError::Provider(SIGN_IN_AGAIN.into())),
            Kind::GeminiSignin
        );
    }

    #[test]
    fn an_error_object_in_the_stream_classes_like_its_status() {
        let quota = json!({"code": 429, "status": "RESOURCE_EXHAUSTED",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED",
                "metadata": {"quotaResetDelay": "90m"}}]});
        assert_eq!(classify(&in_stream(&quota, "m", NOW)), Kind::UsageLimit);
        let denied = json!({"code": 403, "status": "PERMISSION_DENIED", "message": "no"});
        assert_eq!(classify(&in_stream(&denied, "m", NOW)), Kind::GeminiNoTier);
        let server = json!({"code": 500, "status": "INTERNAL", "message": "oops"});
        let e = in_stream(&server, "m", NOW);
        assert!(e.is_transient(), "{e}");
    }
}
