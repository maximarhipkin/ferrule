//! No network, tokens or real messages: recording adapter + real router.
use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, Tool, ToolContext,
    ToolRegistry, Usage,
};
use ferrule_gateway::tools::{FileOut, SendMessageTool};
use ferrule_gateway::{
    AgentFactory, Channel, GatewayError, InboundMessage, OutboundMessage, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Default)]
struct Recorder {
    admitted: Mutex<Vec<String>>,
    sent: Mutex<Vec<OutboundMessage>>,
    fail: bool,
}
#[async_trait]
impl Channel for Recorder {
    fn name(&self) -> &str {
        "telegram"
    }
    fn allows_cross_chat_send(&self, id: &str) -> bool {
        self.admitted.lock().unwrap().iter().any(|s| s == id)
    }
    async fn run(&self, _: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        if self.fail {
            return Err(GatewayError::Channel("test delivery failure".into()));
        }
        self.sent.lock().unwrap().push(msg);
        Ok(())
    }
    async fn send_with_id(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.send(msg).await?;
        Ok(Some("42".into()))
    }
}
struct Quiet;
#[async_trait]
impl Provider for Quiet {
    fn name(&self) -> &str {
        "quiet"
    }
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        Ok(CompletionResponse {
            message: Message::assistant(Some("NO_REPLY".into()), vec![], None),
            usage: Usage::default(),
            stop_reason: None,
        })
    }
}
async fn fixture(
    origin: &str,
    fail: bool,
) -> (tempfile::TempDir, Arc<Router>, Arc<FileOut>, Arc<Recorder>) {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder {
        admitted: Mutex::new(vec!["-5293598802".into()]),
        fail,
        ..Default::default()
    });
    let out = Arc::new(FileOut::default());
    let factory: AgentFactory = Arc::new(|_, transcript| {
        Ok(Agent::new(
            Arc::new(Quiet),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            Some(transcript),
        ))
    });
    let router = Arc::new(Router::new(
        dir.path(),
        factory,
        HashMap::from([("telegram".into(), rec.clone() as Arc<dyn Channel>)]),
    ));
    out.bind(&router);
    router
        .dispatch_and_wait(InboundMessage {
            channel: "telegram".into(),
            chat_id: origin.into(),
            sender: "owner".into(),
            sender_id: Some(origin.into()),
            message_id: "1".into(),
            text: "prepare".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        })
        .await
        .unwrap();
    // Ignore the initial inbound response that established the owner lane.
    rec.sent.lock().unwrap().clear();
    (dir, router, out, rec)
}
fn args() -> Value {
    json!({"chat_id": "-5293598802", "text": "האקדמיה פעילה!"})
}

#[test]
fn only_exact_owner_private_session_is_offered_the_tool() {
    let out = Arc::new(FileOut::default());
    for sid in [
        "telegram__-123",
        "telegram__2",
        "telegram__1__child",
        "scheduler__1",
        "discord__1",
        "telegram__01",
    ] {
        assert!(
            SendMessageTool::for_owner(out.clone(), sid, 1).is_none(),
            "{sid}"
        );
    }
    assert!(SendMessageTool::for_owner(out.clone(), "telegram__-1", -1).is_none());
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    assert!(tool.needs_approval());
    assert!(!tool.read_only());
}

#[tokio::test]
async fn sends_exact_text_to_admitted_group_with_delivery_id() {
    let (_dir, _router, out, rec) = fixture("1", false).await;
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    let result = tool.call(args(), &ToolContext::default()).await.unwrap();
    let result: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(result["message_id"], "42");
    assert_eq!(result["chat_id"], "-5293598802");
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text, "האקדמיה פעילה!");
    assert_eq!(sent[0].chat_id, "-5293598802");
    assert!(sent[0].reply_to.is_none());
    assert!(sent[0].attachments.is_empty());
}

#[tokio::test]
async fn refuses_invalid_and_unadmitted_destinations_without_delivery() {
    let (_dir, _router, out, rec) = fixture("1", false).await;
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    for id in [
        "-2",
        "@public",
        "0",
        "01",
        "+1",
        " -5293598802",
        "9223372036854775808",
    ] {
        assert!(tool
            .call(
                json!({"chat_id":id,"text":"hello"}),
                &ToolContext::default()
            )
            .await
            .is_err());
    }
    for value in [
        json!({"chat_id":-5293598802_i64,"text":"hi"}),
        json!({"chat_id":"-5293598802","text":" "}),
        json!({"chat_id":"-5293598802","text":"😀".repeat(2049)}),
        json!({"chat_id":"-5293598802","text":"hi","token":"unused"}),
    ] {
        assert!(tool.call(value, &ToolContext::default()).await.is_err());
    }
    rec.admitted.lock().unwrap().clear();
    assert!(
        tool.call(args(), &ToolContext::default()).await.is_err(),
        "live admission must be rechecked"
    );
    assert!(rec.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refuses_without_owner_lane_or_gateway() {
    let out = Arc::new(FileOut::default());
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    assert!(tool.call(args(), &ToolContext::default()).await.is_err());
    let (_dir, _router, out, rec) = fixture("2", false).await;
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    assert!(tool.call(args(), &ToolContext::default()).await.is_err());
    assert!(rec.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reports_delivery_failure_without_claiming_sent_or_retrying() {
    let (_dir, _router, out, rec) = fixture("1", true).await;
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    let error = tool
        .call(args(), &ToolContext::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous"));
    assert!(rec.sent.lock().unwrap().is_empty());
}

struct DefaultDenied;
#[async_trait]
impl Channel for DefaultDenied {
    fn name(&self) -> &str {
        "other"
    }
    async fn run(&self, _: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn send(&self, _: OutboundMessage) -> Result<(), GatewayError> {
        panic!("default-denied channel must never send")
    }
}

#[test]
fn admission_is_denied_by_default_and_for_an_empty_telegram_allowlist() {
    use ferrule_gateway::channels::telegram::TelegramChannel;
    let other = DefaultDenied;
    let empty = TelegramChannel::with_base_url("NOT_A_TOKEN", "http://127.0.0.1:1");
    for id in ["1", "-5293598802", "@name", ""] {
        assert!(!other.allows_cross_chat_send(id));
        assert!(!empty.allows_cross_chat_send(id));
    }
}

#[tokio::test]
async fn accepts_the_utf16_boundary_without_rewriting_content() {
    let (_dir, _router, out, rec) = fixture("1", false).await;
    let tool = SendMessageTool::for_owner(out, "telegram__1", 1).unwrap();
    let text = "😀".repeat(2048);
    tool.call(
        json!({"chat_id": "-5293598802", "text": text}),
        &ToolContext::default(),
    )
    .await
    .unwrap();
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text, text);
}

#[test]
fn real_telegram_admission_is_numeric_canonical_and_allowlisted() {
    use ferrule_gateway::channels::telegram::TelegramChannel;
    let tg = TelegramChannel::with_base_url("NOT_A_TOKEN", "http://127.0.0.1:1")
        .with_allowed_chats(vec![1, -5293598802]);
    assert!(tg.allows_cross_chat_send("-5293598802"));
    assert!(tg.allows_cross_chat_send("1"));
    for id in ["-2", "01", "+1", "@name", "0", ""] {
        assert!(!tg.allows_cross_chat_send(id));
    }
}
