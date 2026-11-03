//! Antigravity CLI (`agy`) and `@google/gemini-cli` `--output-format stream-json`
//! events: `init`, `step_update` / `message` (delta text), `tool_use`,
//! `tool_result`, `error`, and `result`.

use ferrule_core::message::Usage;
use ferrule_core::CoreError;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Init {
        session_id: String,
        model: Option<String>,
    },
    Text(String),
    Progress,
    Error(String),
    Result(TurnResult),
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub is_error: bool,
    pub text: String,
    pub session_id: Option<String>,
    pub usage: Usage,
    pub model: Option<String>,
    pub errors: Vec<String>,
}

pub fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if let Some(ev) = v["event"].as_str() {
        return parse_agy(ev, &v);
    }
    Some(match v["type"].as_str()? {
        "init" => Event::Init {
            session_id: v["session_id"].as_str()?.to_string(),
            model: v["model"].as_str().map(str::to_string),
        },
        "message" => {
            let role = v["role"].as_str().unwrap_or("");
            if role == "assistant" || role == "model" {
                let text = v["content"].as_str().unwrap_or("");
                if text.is_empty() {
                    Event::Progress
                } else {
                    Event::Text(text.to_string())
                }
            } else {
                Event::Other
            }
        }
        "thought" | "tool_use" | "tool_result" => Event::Progress,
        "error" => {
            let msg = v["message"]
                .as_str()
                .or_else(|| v["error"]["message"].as_str())
                .unwrap_or("unknown gemini error");
            Event::Error(msg.to_string())
        }
        "result" => {
            let status = v["status"].as_str().unwrap_or("success");
            let is_error = status.eq_ignore_ascii_case("error")
                || v["is_error"].as_bool().unwrap_or(false)
                || v.get("error").is_some_and(|e| !e.is_null());
            let stats = &v["stats"];
            let n = |k: &str| {
                stats[k]
                    .as_u64()
                    .or_else(|| v["usage"][k].as_u64())
                    .unwrap_or(0)
            };
            let input_tokens = n("input_tokens").max(n("input"));
            let output_tokens = n("output_tokens").max(n("output"));
            let cached_tokens = n("cached")
                .max(n("cached_input_tokens"))
                .max(n("cache_read_tokens"));
            let mut errors = Vec::new();
            if let Some(msg) = v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .filter(|s| !s.is_empty())
            {
                errors.push(msg.to_string());
            }
            let text = v["response"]
                .as_str()
                .or_else(|| v["result"].as_str())
                .unwrap_or("")
                .to_string();
            Event::Result(TurnResult {
                is_error,
                text,
                session_id: v["session_id"].as_str().map(str::to_string),
                usage: Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens: cached_tokens,
                    cache_write_input_tokens: 0,
                    cache_write_1h_input_tokens: 0,
                    notional_usd: None,
                },
                model: v["model"].as_str().map(str::to_string),
                errors,
            })
        }
        _ => Event::Other,
    })
}

fn parse_agy(ev: &str, v: &Value) -> Option<Event> {
    Some(match ev {
        "init" => {
            let session_id = v["conversation_id"]
                .as_str()
                .or_else(|| v["init"]["conversation_id"].as_str())?
                .to_string();
            let model = v["init"]["model"]
                .as_str()
                .or_else(|| v["model"].as_str())
                .map(str::to_string);
            Event::Init { session_id, model }
        }
        "step_update" => {
            let su = &v["step_update"];
            if su["step_type"].as_str() == Some("agent_response") {
                let text = su["text_delta"].as_str().unwrap_or("");
                if text.is_empty() {
                    Event::Progress
                } else {
                    Event::Text(text.to_string())
                }
            } else {
                Event::Progress
            }
        }
        "error" => {
            let msg = v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
                .unwrap_or("unknown antigravity error");
            Event::Error(msg.to_string())
        }
        "result" => {
            let r = if v["result"].is_object() {
                &v["result"]
            } else {
                v
            };
            let status = r["status"].as_str().unwrap_or("SUCCESS");
            let is_error = status.eq_ignore_ascii_case("error")
                || r["is_error"].as_bool().unwrap_or(false)
                || r.get("error")
                    .is_some_and(|e| !e.is_null() && e.as_str() != Some(""));
            let usage = &r["usage"];
            let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
            let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0)
                + usage["thinking_tokens"].as_u64().unwrap_or(0);
            let cached_tokens = usage["cache_read_tokens"]
                .as_u64()
                .or_else(|| usage["cached_input_tokens"].as_u64())
                .unwrap_or(0);
            let mut errors = Vec::new();
            if let Some(msg) = r["error"]["message"]
                .as_str()
                .or_else(|| r["error"].as_str())
                .filter(|s| !s.is_empty())
            {
                errors.push(msg.to_string());
            }
            let text = r["response"].as_str().unwrap_or("").to_string();
            let session_id = r["conversation_id"]
                .as_str()
                .or_else(|| r["session_id"].as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Event::Result(TurnResult {
                is_error,
                text,
                session_id,
                usage: Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens: cached_tokens,
                    cache_write_input_tokens: 0,
                    cache_write_1h_input_tokens: 0,
                    notional_usd: None,
                },
                model: r["model"].as_str().map(str::to_string),
                errors,
            })
        }
        _ => Event::Other,
    })
}

pub const NOT_SIGNED_IN: &str =
    "Gemini CLI isn't signed in: run `ferrule login gemini` on the server";

fn not_signed_in(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "not logged in",
        "not signed in",
        "not authenticated",
        "please set an auth method",
        "unauthenticated",
        "invalid_grant",
        "oauth_creds.json",
        "antigravity-oauth-token",
        "login required",
    ]
    .iter()
    .any(|w| t.contains(w))
}

fn rate_limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "resource_exhausted",
        "quota exceeded",
        "ratelimitexceeded",
        "rate limit",
        "429",
    ]
    .iter()
    .any(|w| t.contains(w))
}

pub fn resume_missing(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("no session found")
        || t.contains("session not found")
        || t.contains("no conversation found")
        || (t.contains("conversation") && t.contains("not found"))
}

pub fn result_error(r: &TurnResult) -> Option<CoreError> {
    if !r.is_error {
        return None;
    }
    let text = if !r.errors.is_empty() {
        r.errors.join("; ")
    } else {
        r.text.clone()
    };
    if rate_limited(&text) {
        return Some(CoreError::Transient {
            message: "usage_limit_reached: the Gemini plan's rate or quota limit is reached".into(),
            retry_after: Some(Duration::from_secs(60)),
        });
    }
    if not_signed_in(&text) {
        return Some(CoreError::Provider(NOT_SIGNED_IN.into()));
    }
    let short: String = text.chars().take(400).collect();
    Some(CoreError::Provider(format!("Gemini CLI: {short}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gemini_and_agy_stream_json_events() {
        let init = r#"{"type":"init","session_id":"gem-1","model":"gemini-2.5-pro"}"#;
        assert_eq!(
            parse(init),
            Some(Event::Init {
                session_id: "gem-1".into(),
                model: Some("gemini-2.5-pro".into()),
            })
        );
        let msg = r#"{"type":"message","role":"assistant","content":"Hello world","delta":true}"#;
        assert_eq!(parse(msg), Some(Event::Text("Hello world".into())));
        let res = r#"{"type":"result","status":"success","stats":{"input_tokens":120,"output_tokens":35,"cached":40}}"#;
        let Some(Event::Result(r)) = parse(res) else {
            panic!("expected result");
        };
        assert!(!r.is_error);
        assert_eq!(r.usage.input_tokens, 120);
        assert_eq!(r.usage.output_tokens, 35);
        assert_eq!(r.usage.cached_input_tokens, 40);
        assert!(result_error(&r).is_none());

        // Antigravity (`agy`) event format:
        let agy_init = r#"{"event":"init","conversation_id":"conv-99","init":{"model":"gemini-4-argon-medium"}}"#;
        assert_eq!(
            parse(agy_init),
            Some(Event::Init {
                session_id: "conv-99".into(),
                model: Some("gemini-4-argon-medium".into()),
            })
        );
        let agy_delta = r#"{"event":"step_update","step_update":{"conversation_id":"conv-99","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"PONG"}}"#;
        assert_eq!(parse(agy_delta), Some(Event::Text("PONG".into())));
        let agy_res = r#"{"event":"result","result":{"conversation_id":"conv-99","status":"SUCCESS","response":"PONG\n","usage":{"input_tokens":12000,"output_tokens":4,"thinking_tokens":2,"cache_read_tokens":500}}}"#;
        let Some(Event::Result(ar)) = parse(agy_res) else {
            panic!("expected agy result");
        };
        assert!(!ar.is_error);
        assert_eq!(ar.session_id.as_deref(), Some("conv-99"));
        assert_eq!(ar.text, "PONG\n");
        assert_eq!(ar.usage.input_tokens, 12000);
        assert_eq!(ar.usage.output_tokens, 6);
        assert_eq!(ar.usage.cached_input_tokens, 500);
    }
}
