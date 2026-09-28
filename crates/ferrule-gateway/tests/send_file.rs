//! M39 §2: `send_file` sends a workspace file to the session's own chat,
//! and only there; a path out of the workspace or under a hidden one, and
//! a channel without attachments, are refused in words the model reads.

use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolCall, ToolRegistry, Usage,
};
use ferrule_gateway::tools::{FileOut, SendFileTool};
use ferrule_gateway::{
    AgentFactory, Channel, ChannelCapabilities, GatewayError, InboundMessage, OutboundMessage,
    Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Keeps what it's sent; `files` says whether it takes attachments.
struct Recorder {
    files: bool,
    sent: Mutex<Vec<OutboundMessage>>,
}

#[async_trait]
impl Channel for Recorder {
    fn name(&self) -> &str {
        "rec"
    }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            attachments: self.files,
            ..Default::default()
        }
    }
    async fn run(&self, _tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.sent.lock().unwrap().push(msg);
        Ok(())
    }
}

/// Calls `send_file` with `args` once, then answers with what the tool
/// said.
struct Caller {
    args: Value,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for Caller {
    fn name(&self) -> &str {
        "caller"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let message = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Message::assistant(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "send_file".into(),
                    arguments: self.args.clone(),
                }],
                None,
            )
        } else {
            let last = serde_json::to_string(req.messages.last().unwrap()).unwrap();
            Message::assistant(Some(last), vec![], None)
        };
        Ok(CompletionResponse {
            message,
            usage: Usage::default(),
        })
    }
}

fn factory(args: Value, ws: PathBuf, out: Arc<FileOut>) -> AgentFactory {
    Arc::new(move |sid, transcript| {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(SendFileTool::new(
            out.clone(),
            sid,
            vec![ws.join(".hidden")],
        )));
        Ok(Agent::new(
            Arc::new(Caller {
                args: args.clone(),
                calls: AtomicUsize::new(0),
            }),
            tools,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext {
                workspace: ws.clone(),
                max_output_chars: 30_000,
            },
            Some(transcript),
        )
        .with_system_prompt("test"))
    })
}

/// One turn in chat `c7` on a `Recorder`; what the model was told, and
/// what the chat got.
async fn turn(ws: &Path, files: bool, args: Value) -> (String, Vec<OutboundMessage>) {
    let sessions = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder {
        files,
        sent: Mutex::new(vec![]),
    });
    let channels = HashMap::from([("rec".to_string(), rec.clone() as Arc<dyn Channel>)]);
    let out = Arc::new(FileOut::default());
    let router = Arc::new(Router::new(
        sessions.path(),
        factory(args, ws.to_path_buf(), out.clone()),
        channels,
    ));
    out.bind(&router);
    let reply = router
        .dispatch_and_wait(InboundMessage {
            channel: "rec".into(),
            chat_id: "c7".into(),
            sender: "max".into(),
            sender_id: None,
            message_id: "1".into(),
            text: "send me the report".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        })
        .await
        .unwrap();
    let sent = rec.sent.lock().unwrap().clone();
    (reply.text, sent)
}

fn workspace() -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("report.pdf"), b"%PDF-1.4 hi").unwrap();
    std::fs::create_dir(ws.path().join(".hidden")).unwrap();
    std::fs::write(ws.path().join(".hidden/key"), b"secret").unwrap();
    ws
}

#[tokio::test]
async fn a_workspace_file_goes_to_the_sessions_own_chat() {
    let ws = workspace();
    let (told, sent) = turn(
        ws.path(),
        true,
        json!({"path": "report.pdf", "caption": "here it is"}),
    )
    .await;
    assert!(told.contains("Sent report.pdf"), "{told}");
    let files: Vec<_> = sent.iter().filter(|m| !m.attachments.is_empty()).collect();
    assert_eq!(files.len(), 1, "{sent:?}");
    let m = files[0];
    assert_eq!((m.chat_id.as_str(), m.text.as_str()), ("c7", "here it is"));
    assert_eq!(m.attachments[0].kind, "application/pdf");
    assert_eq!(m.attachments[0].name.as_deref(), Some("report.pdf"));
    assert!(Path::new(&m.attachments[0].url).ends_with("report.pdf"));
}

#[tokio::test]
async fn nothing_outside_the_workspace_or_hidden_leaves() {
    let ws = workspace();
    let outside = tempfile::NamedTempFile::new().unwrap();
    for path in [
        outside.path().to_string_lossy().into_owned(),
        "../x".into(),
        ".hidden/key".into(),
    ] {
        let (told, sent) = turn(ws.path(), true, json!({ "path": path })).await;
        assert!(
            sent.iter().all(|m| m.attachments.is_empty()),
            "{path}: {sent:?}"
        );
        assert!(!told.contains("Sent "), "{path}: {told}");
    }
}

#[tokio::test]
async fn a_channel_without_files_is_said_so() {
    let ws = workspace();
    let (told, sent) = turn(ws.path(), false, json!({"path": "report.pdf"})).await;
    assert!(told.contains("can't send files"), "{told}");
    assert!(sent.iter().all(|m| m.attachments.is_empty()));
}
