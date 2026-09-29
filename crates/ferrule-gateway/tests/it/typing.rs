//! M41: "typing…" in a Telegram chat, against a mock Bot API: shown again
//! every 4 s while a long turn runs, and never after the reply; after a
//! 429, not again that turn.

use crate::support::http::{serve, Request, Response};
use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage,
};
use ferrule_gateway::{AgentFactory, Channel, InboundMessage, Router, TelegramChannel};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Answers "done" after `self.0`.
struct Slow(Duration);

#[async_trait]
impl Provider for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        tokio::time::sleep(self.0).await;
        Ok(CompletionResponse {
            message: Message::assistant(Some("done".into()), vec![], None),
            usage: Usage::default(),
        })
    }
}

fn factory(think: Duration) -> AgentFactory {
    Arc::new(move |_sid, transcript| {
        Ok(Agent::new(
            Arc::new(Slow(think)),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        )
        .with_system_prompt("test"))
    })
}

/// The Bot API methods called, with their payloads.
type Calls = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// A Bot API that logs each method called; `sendChatAction` answers 429
/// when `limited`.
fn bot(limited: bool) -> (String, Calls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let port = serve(Arc::new(move |req: Request| {
        let method = req
            .path
            .split('?')
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        log.lock().unwrap().push((method.clone(), req.json()));
        match method.as_str() {
            "sendChatAction" if limited => Response::json(json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 3",
                "parameters": {"retry_after": 3}
            }))
            .with_status(429),
            "sendMessage" => Response::json(json!({"ok": true, "result": {"message_id": 500}})),
            _ => Response::json(json!({"ok": true, "result": true})),
        }
    }));
    (format!("http://127.0.0.1:{port}"), calls)
}

async fn turn(bot_url: &str, think: Duration, typing: bool) -> String {
    let telegram = Arc::new(TelegramChannel::with_base_url("T", bot_url));
    let channels = HashMap::from([("telegram".to_string(), telegram as Arc<dyn Channel>)]);
    let sessions = tempfile::tempdir().unwrap();
    let router = Router::new(sessions.path(), factory(think), channels).with_typing(typing);
    let msg = InboundMessage {
        channel: "telegram".into(),
        chat_id: "9999".into(),
        sender: "max".into(),
        sender_id: None,
        message_id: "41".into(),
        text: "think hard".into(),
        attachments: vec![],
        reply_to: None,
        ts: 0,
    };
    tokio::time::timeout(Duration::from_secs(30), router.dispatch_and_wait(msg))
        .await
        .expect("the turn ends")
        .unwrap()
        .text
}

fn methods(calls: &Calls) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect()
}

#[tokio::test]
async fn typing_shows_through_a_long_turn_and_stops_with_the_reply() {
    let (url, calls) = bot(false);
    let answer = turn(&url, Duration::from_millis(5500), true).await;
    assert_eq!(answer, "done");

    let seen = methods(&calls);
    let reply = seen
        .iter()
        .position(|m| m == "sendMessage")
        .unwrap_or_else(|| panic!("the reply was sent: {seen:?}"));
    let typing = seen.iter().filter(|m| *m == "sendChatAction").count();
    assert_eq!(typing, 2, "at the start, then 4 s later: {seen:?}");
    assert!(
        seen[reply..].iter().all(|m| m != "sendChatAction"),
        "none after the reply: {seen:?}"
    );
    let (_, first) = calls
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| m == "sendChatAction")
        .cloned()
        .unwrap();
    assert_eq!(first["action"], "typing");
    assert_eq!(first["chat_id"], "9999");

    // Nothing more once the turn is over, not even a refresh due later.
    tokio::time::sleep(Duration::from_millis(4200)).await;
    assert_eq!(methods(&calls), seen);
}

#[tokio::test]
async fn after_a_429_typing_stops_for_the_turn_and_the_reply_still_goes() {
    let (url, calls) = bot(true);
    let answer = turn(&url, Duration::from_millis(1500), true).await;
    assert_eq!(answer, "done");
    let seen = methods(&calls);
    assert_eq!(
        seen.iter().filter(|m| *m == "sendChatAction").count(),
        1,
        "{seen:?}"
    );
    assert!(seen.iter().any(|m| m == "sendMessage"), "{seen:?}");
}

#[tokio::test]
async fn typing_off_sends_none() {
    let (url, calls) = bot(false);
    turn(&url, Duration::from_millis(50), false).await;
    let seen = methods(&calls);
    assert!(seen.iter().all(|m| m != "sendChatAction"), "{seen:?}");
    assert!(seen.iter().any(|m| m == "sendMessage"), "{seen:?}");
}
