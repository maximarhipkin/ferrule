//! M41: `/new` and its self-heal hint, through `Gateway::run` with a
//! scripted chat and a provider that rejects a poisoned history.

use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage,
};
use ferrule_gateway::{
    AgentFactory, Channel, Gateway, GatewayError, InboundMessage, OutboundMessage, Router, NEW_HINT,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Refuses any request whose history holds "POISON" the way a provider
/// refuses a malformed conversation (HTTP 400); answers "echo: <last>"
/// otherwise, after "slow" takes a long time.
struct Picky;

#[async_trait]
impl Provider for Picky {
    fn name(&self) -> &str {
        "picky"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let texts: Vec<&str> = req
            .messages
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect();
        if texts.iter().any(|t| t.contains("POISON")) {
            return Err(CoreError::Provider(
                r#"HTTP 400 Bad Request: {"error":{"type":"invalid_request_error","message":"messages.1.content.0.image.source: invalid image"}}"#
                    .into(),
            ));
        }
        let last = texts.last().copied().unwrap_or("").to_string();
        if last == "slow" {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Ok(CompletionResponse {
            message: Message::assistant(Some(format!("echo: {last}")), vec![], None),
            usage: Usage::default(),
        })
    }
}

fn factory() -> AgentFactory {
    Arc::new(|_sid, transcript| {
        Ok(Agent::new(
            Arc::new(Picky),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        )
        .with_system_prompt("test"))
    })
}

/// A chat that sends its script in order. After a line marked to wait, it
/// waits until that many replies in total have come back.
struct Chat {
    script: Vec<(&'static str, usize)>,
    sent: Mutex<Vec<String>>,
    got: Notify,
}

impl Chat {
    fn new(script: Vec<(&'static str, usize)>) -> Arc<Self> {
        Arc::new(Self {
            script,
            sent: Mutex::default(),
            got: Notify::new(),
        })
    }

    fn replies(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Channel for Chat {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        for (i, (text, wait_for)) in self.script.iter().enumerate() {
            let msg = InboundMessage {
                channel: "chat".into(),
                chat_id: "7".into(),
                sender: "max".into(),
                sender_id: None,
                message_id: format!("{}", i + 1),
                text: text.to_string(),
                attachments: vec![],
                reply_to: None,
                ts: 0,
            };
            tx.send(msg).await.unwrap();
            // A line that doesn't wait still gives the lane a moment to
            // pick it up, so what's running and what's queued is known.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while self.sent.lock().unwrap().len() < *wait_for {
                let notified = self.got.notified();
                if self.sent.lock().unwrap().len() >= *wait_for {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    panic!(
                        "no reply {wait_for} after {text:?}; got {:?}",
                        self.replies()
                    );
                }
            }
        }
        Ok(())
    }
    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.sent.lock().unwrap().push(msg.text);
        self.got.notify_waiters();
        Ok(())
    }
}

async fn run(chat: Arc<Chat>, sessions: &Path) {
    let channels: HashMap<String, Arc<dyn Channel>> =
        HashMap::from([("chat".to_string(), chat.clone() as Arc<dyn Channel>)]);
    let router = Arc::new(Router::new(sessions, factory(), channels));
    let mut gateway = Gateway::new(router);
    gateway.add_channel(chat);
    tokio::time::timeout(Duration::from_secs(60), gateway.run())
        .await
        .expect("the gateway ends once the chat's script is done")
        .unwrap();
}

fn transcripts(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_poisoned_conversation_is_escaped_with_new() {
    let dir = tempfile::tempdir().unwrap();
    let chat = Chat::new(vec![
        ("look at this POISON", 1),
        ("hello?", 2),
        ("/new", 3),
        ("hello again", 4),
    ]);
    run(chat.clone(), dir.path()).await;
    let replies = chat.replies();
    assert_eq!(replies.len(), 4, "{replies:?}");
    assert!(
        replies[0].starts_with("I couldn't reply:"),
        "{}",
        replies[0]
    );
    assert!(
        !replies[0].contains("/new"),
        "one 400 isn't a reason to throw a conversation away: {}",
        replies[0]
    );
    assert!(
        replies[1].starts_with("I couldn't reply:"),
        "{}",
        replies[1]
    );
    assert!(
        replies[1].ends_with("send /new to start a fresh conversation"),
        "the same 400 twice in a row: {}",
        replies[1]
    );
    assert!(replies[1].ends_with(NEW_HINT));
    assert_eq!(
        replies[2],
        "New conversation. The previous one (2 messages) is saved."
    );
    assert_eq!(replies[3], "echo: hello again");

    let names = transcripts(dir.path());
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"chat__7.jsonl".to_string()), "{names:?}");
    let saved = names.iter().find(|n| *n != "chat__7.jsonl").unwrap();
    assert!(
        saved.starts_with("chat__7.2") && saved.ends_with("Z.jsonl"),
        "{names:?}"
    );
    let old = std::fs::read_to_string(dir.path().join(saved)).unwrap();
    assert!(old.contains("POISON"), "the old conversation is kept");
    let new = std::fs::read_to_string(dir.path().join("chat__7.jsonl")).unwrap();
    assert!(!new.contains("POISON"), "{new}");
    assert!(new.contains("hello again"), "{new}");
}

#[tokio::test]
async fn new_stops_the_running_turn_and_drops_what_waited() {
    let dir = tempfile::tempdir().unwrap();
    let chat = Chat::new(vec![
        ("first", 1),
        ("slow", 1),
        ("queued behind it", 1),
        ("/reset", 3),
        ("after", 4),
    ]);
    let started = std::time::Instant::now();
    run(chat.clone(), dir.path()).await;
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the slow turn was stopped, not waited out"
    );
    let replies = chat.replies();
    assert_eq!(replies[0], "echo: first");
    assert!(
        replies[1].starts_with("Stopped from /new: I ended this turn."),
        "{replies:?}"
    );
    assert_eq!(
        replies[2],
        "New conversation. The previous one (4 messages) is saved. 1 message that was waiting was dropped; send it again if you still need it."
    );
    assert_eq!(replies[3], "echo: after");
    assert_eq!(
        replies.len(),
        4,
        "the dropped message got no turn: {replies:?}"
    );
}

#[tokio::test]
async fn new_on_a_chat_with_no_conversation_says_so_plainly() {
    let dir = tempfile::tempdir().unwrap();
    let chat = Chat::new(vec![("/new@ferrule_bot", 1), ("/help", 2)]);
    run(chat.clone(), dir.path()).await;
    let replies = chat.replies();
    assert_eq!(replies[0], "New conversation.");
    assert!(
        replies[1].starts_with("Commands:\n/new — "),
        "{}",
        replies[1]
    );
    assert!(replies[1].contains("/help — "), "{}", replies[1]);
}
