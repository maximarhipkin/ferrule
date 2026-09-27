//! Claude Code's `--output-format stream-json` events, as far as the
//! engine needs them: the session id, the streamed text, the rate-limit
//! reading and the final result. Unknown events and fields are ignored, so
//! a newer claude that adds some still works.

use crate::usage::{Reading, Window};
use ferrule_core::message::Usage;
use ferrule_core::CoreError;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `system`/`init`: the session this turn runs in.
    Init {
        session_id: String,
        model: Option<String>,
    },
    /// Visible answer text, streamed.
    Text(String),
    /// Thinking or tool arguments streaming: the model is alive.
    Progress,
    RateLimit(RateLimit),
    Result(TurnResult),
    Other,
}

/// A `rate_limit_event`'s `rate_limit_info`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimit {
    /// `allowed`, `allowed_warning` or `rejected`.
    pub status: String,
    pub resets_at: Option<u64>,
    /// `five_hour`, `seven_day`, …
    pub kind: Option<String>,
    /// `(name, utilization 0..1, resets_at)`.
    pub windows: Vec<(String, f64, Option<u64>)>,
}

impl RateLimit {
    pub fn rejected(&self) -> bool {
        self.status == "rejected"
    }

    /// The reading for the usage file.
    pub fn reading(&self, now: u64) -> Reading {
        Reading {
            at: now,
            plan: None,
            windows: self
                .windows
                .iter()
                .map(|(name, used, resets)| Window {
                    name: window_name(name),
                    used_percent: (used * 1000.0).round() / 10.0,
                    resets_at: *resets,
                })
                .collect(),
            limited_until: if self.rejected() {
                self.resets_at.or(Some(now + 300))
            } else {
                None
            },
            status: Some(self.status.clone()),
        }
    }
}

/// `five_hour` → `5h`, `seven_day` → `weekly`, `seven_day_opus` → `weekly
/// (opus)`.
pub fn window_name(kind: &str) -> String {
    match kind {
        "five_hour" => "5h".into(),
        "seven_day" => "weekly".into(),
        other => match other.strip_prefix("seven_day_") {
            Some(model) => format!("weekly ({model})"),
            None => other.replace('_', " "),
        },
    }
}

/// The `result` event.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub text: String,
    pub session_id: Option<String>,
    pub usage: Usage,
    /// Claude Code's own figure for what the turn would cost at API prices.
    pub total_cost_usd: Option<f64>,
    pub api_error_status: Option<u64>,
    /// The model that ran, when the result says (`modelUsage`'s key).
    pub model: Option<String>,
    /// `errors`, for the error subtypes.
    pub errors: Vec<String>,
}

pub fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    Some(match v["type"].as_str()? {
        "system" if v["subtype"] == "init" => Event::Init {
            session_id: v["session_id"].as_str()?.to_string(),
            model: v["model"].as_str().map(str::to_string),
        },
        "stream_event" => {
            let ev = &v["event"];
            if ev["type"] != "content_block_delta" {
                return Some(Event::Other);
            }
            match ev["delta"]["type"].as_str() {
                Some("text_delta") => Event::Text(ev["delta"]["text"].as_str()?.to_string()),
                Some("thinking_delta" | "input_json_delta" | "signature_delta") => Event::Progress,
                _ => Event::Other,
            }
        }
        "rate_limit_event" => {
            let info = &v["rate_limit_info"];
            let mut windows: Vec<(String, f64, Option<u64>)> = info["unifiedWindows"]
                .as_object()
                .map(|w| {
                    w.iter()
                        .map(|(k, w)| {
                            (
                                k.clone(),
                                w["utilization"].as_f64().unwrap_or(0.0),
                                w["resetsAt"].as_u64(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            windows.sort_by_key(|(k, _, _)| (k != "five_hour", k.clone()));
            Event::RateLimit(RateLimit {
                status: info["status"].as_str().unwrap_or("allowed").to_string(),
                resets_at: info["resetsAt"].as_u64(),
                kind: info["rateLimitType"].as_str().map(str::to_string),
                windows,
            })
        }
        "result" => {
            let u = &v["usage"];
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let (cache_write, cache_read) = (
                n("cache_creation_input_tokens"),
                n("cache_read_input_tokens"),
            );
            Event::Result(TurnResult {
                is_error: v["is_error"].as_bool().unwrap_or(false),
                subtype: v["subtype"].as_str().unwrap_or("").to_string(),
                text: v["result"].as_str().unwrap_or("").to_string(),
                session_id: v["session_id"].as_str().map(str::to_string),
                usage: Usage {
                    input_tokens: n("input_tokens") + cache_write + cache_read,
                    output_tokens: n("output_tokens"),
                    cached_input_tokens: cache_read,
                    cache_write_input_tokens: cache_write,
                    notional_usd: v["total_cost_usd"].as_f64(),
                },
                total_cost_usd: v["total_cost_usd"].as_f64(),
                api_error_status: v["api_error_status"].as_u64(),
                model: v["modelUsage"]
                    .as_object()
                    .and_then(|m| m.keys().next().cloned()),
                errors: v["errors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        }
        _ => Event::Other,
    })
}

/// Words that mean claude has no credential to use.
fn not_signed_in(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "not logged in",
        "please run /login",
        "invalid api key",
        "oauth token has expired",
        "oauth token revoked",
        "authentication_error",
        "invalid bearer token",
    ]
    .iter()
    .any(|w| t.contains(w))
}

/// Words that mean `--resume` found nothing to resume.
pub fn resume_missing(text: &str) -> bool {
    text.contains("No conversation found")
}

pub const NOT_SIGNED_IN: &str =
    "Claude Code isn't signed in: run `ferrule login claude` on the server";

/// The error for a turn that ended badly. `limit` is the turn's last
/// rate-limit reading. `None`: the result is a normal answer.
pub fn result_error(r: &TurnResult, limit: Option<&RateLimit>, now: u64) -> Option<CoreError> {
    let text = if r.text.is_empty() {
        r.errors.join("; ")
    } else {
        r.text.clone()
    };
    let rejected = limit.filter(|l| l.rejected());
    let limited = rejected.is_some()
        || (r.is_error
            && (r.api_error_status == Some(429)
                || text.to_ascii_lowercase().contains("usage limit")
                || text.contains("limit reached")));
    if limited && r.is_error {
        return Some(limit_error(rejected.or(limit), now));
    }
    if !r.is_error {
        return None;
    }
    if not_signed_in(&text) {
        return Some(CoreError::Provider(NOT_SIGNED_IN.into()));
    }
    let short: String = text.chars().take(400).collect();
    Some(match r.api_error_status {
        Some(s) if s == 408 || s == 429 || s >= 500 => CoreError::Transient {
            message: format!("Claude Code: HTTP {s}: {short}"),
            retry_after: None,
        },
        _ if r.subtype == "error_max_turns" => {
            CoreError::Provider(format!("Claude Code stopped at its turn limit: {short}"))
        }
        _ => CoreError::Provider(format!("Claude Code: {short}")),
    })
}

/// The plan's usage limit: a wait until the reset, so the agent's retries
/// give up at once and M21 falls back or tells the owner when it resets.
pub fn limit_error(limit: Option<&RateLimit>, now: u64) -> CoreError {
    let resets = limit.and_then(|l| l.resets_at);
    let kind = limit
        .and_then(|l| l.kind.as_deref())
        .map(|k| format!(" ({})", window_name(k)))
        .unwrap_or_default();
    let when = resets
        .map(|at| {
            format!(
                "; it resets {}",
                ferrule_providers::codex::describe_reset(at, now)
            )
        })
        .unwrap_or_default();
    CoreError::Transient {
        message: format!(
            "usage_limit_reached: the Claude plan's usage limit is reached{kind}{when}"
        ),
        retry_after: Some(Duration::from_secs(
            resets.map_or(300, |at| at.saturating_sub(now).max(1)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_events_a_real_turn_sends_parse() {
        // Shapes from a live 2.1.283 Haiku turn, trimmed.
        let init = r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"183a","tools":[],"model":"claude-haiku-4-5-20251001","permissionMode":"default"}"#;
        assert_eq!(
            parse(init),
            Some(Event::Init {
                session_id: "183a".into(),
                model: Some("claude-haiku-4-5-20251001".into())
            })
        );
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"pong"}}}"#;
        assert_eq!(parse(text), Some(Event::Text("pong".into())));
        let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}}"#;
        assert_eq!(parse(think), Some(Event::Progress));
        assert_eq!(
            parse(r#"{"type":"stream_event","event":{"type":"message_stop"}}"#),
            Some(Event::Other)
        );
        assert_eq!(parse("not json"), None);

        let rl = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1790506800,"rateLimitType":"five_hour","unifiedWindows":{"seven_day":{"utilization":0.63,"resetsAt":1790820000},"five_hour":{"utilization":0.18,"resetsAt":1790506800}}}}"#;
        let Some(Event::RateLimit(l)) = parse(rl) else {
            panic!()
        };
        assert!(!l.rejected());
        let r = l.reading(1790500000);
        assert_eq!(r.windows[0].name, "5h");
        assert_eq!(r.windows[0].used_percent, 18.0);
        assert_eq!(r.windows[1].name, "weekly");
        assert_eq!(r.limited_until, None);

        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","session_id":"183a","total_cost_usd":0.012568,"api_error_status":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":6174,"cache_read_input_tokens":100,"output_tokens":42},"modelUsage":{"claude-haiku-4-5-20251001":{"costUSD":0.012568}}}"#;
        let Some(Event::Result(r)) = parse(result) else {
            panic!()
        };
        assert_eq!(r.text, "pong");
        assert_eq!(r.usage.input_tokens, 6284);
        assert_eq!(r.usage.cached_input_tokens, 100);
        assert_eq!(r.usage.cache_write_input_tokens, 6174);
        assert_eq!(r.usage.output_tokens, 42);
        assert_eq!(r.model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert!(result_error(&r, Some(&l), 0).is_none());
    }

    #[test]
    fn a_rejected_limit_waits_for_the_reset_and_a_missing_login_says_what_to_run() {
        let limit = RateLimit {
            status: "rejected".into(),
            resets_at: Some(1000 + 2 * 3600),
            kind: Some("five_hour".into()),
            windows: vec![("five_hour".into(), 1.0, Some(1000 + 2 * 3600))],
        };
        assert_eq!(limit.reading(1000).limited_until, Some(1000 + 2 * 3600));
        let r = TurnResult {
            is_error: true,
            text: "Claude AI usage limit reached|1790506800".into(),
            api_error_status: Some(429),
            ..Default::default()
        };
        let e = result_error(&r, Some(&limit), 1000).unwrap();
        let CoreError::Transient {
            message,
            retry_after,
        } = &e
        else {
            panic!("{e}")
        };
        assert!(
            message.starts_with("usage_limit_reached: the Claude plan's usage limit is reached (5h); it resets in 2 h"),
            "{message}"
        );
        assert_eq!(*retry_after, Some(Duration::from_secs(7200)));

        let r = TurnResult {
            is_error: true,
            text: "Not logged in · Please run /login".into(),
            ..Default::default()
        };
        assert_eq!(
            result_error(&r, None, 0).unwrap().to_string(),
            format!("provider error: {NOT_SIGNED_IN}")
        );
        let r = TurnResult {
            is_error: true,
            text: "overloaded".into(),
            api_error_status: Some(529),
            ..Default::default()
        };
        assert!(result_error(&r, None, 0).unwrap().is_transient());
        assert_eq!(window_name("seven_day_opus"), "weekly (opus)");
        assert!(resume_missing("No conversation found with session ID: abc"));
    }
}
