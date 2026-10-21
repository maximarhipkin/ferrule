//! M51: `generate_image` saves and sends a generated image to the session's
//! own chat and counts it against the daily cap; `generate_video` answers
//! at once with a job id and the built-in poller delivers the clip minutes
//! later — or says why it failed. All against FakeMedia; no real network.

use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::ToolContext;
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, LedgerRecord, LedgerSink, Message, Provider,
    ToolCall, ToolRegistry, Usage,
};
use ferrule_gateway::scheduler::{BuiltinJob, NewTask, TaskKind, TaskStore};
use ferrule_gateway::tools::{GenerateImageTool, GenerateVideoTool, MediaPoller, MediaShared};
use ferrule_gateway::{
    AgentFactory, Channel, ChannelCapabilities, GatewayError, InboundMessage, OutboundMessage,
    Router,
};
use ferrule_media::fake::FakeMedia;
use ferrule_media::JobStore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

struct Recorder {
    sent: Mutex<Vec<OutboundMessage>>,
}

#[async_trait]
impl Channel for Recorder {
    fn name(&self) -> &str {
        "rec"
    }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            attachments: true,
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

struct Rows(Mutex<Vec<LedgerRecord>>);
impl LedgerSink for Rows {
    fn record(&self, r: LedgerRecord) {
        self.0.lock().unwrap().push(r);
    }
}

/// Calls `tool` with `args` once, then answers with what the tool said.
struct Caller {
    tool: &'static str,
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
                    name: self.tool.into(),
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

struct Rig {
    _sessions: tempfile::TempDir,
    router: Arc<Router>,
    rec: Arc<Recorder>,
    rows: Arc<Rows>,
    shared: Arc<MediaShared>,
}

fn rig(
    ws: PathBuf,
    tool: &'static str,
    args: Value,
    backend: Arc<FakeMedia>,
    max_per_day: u64,
) -> Rig {
    let sessions = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder {
        sent: Mutex::new(vec![]),
    });
    let rows = Arc::new(Rows(Mutex::default()));
    let store = Arc::new(JobStore::open(&sessions.path().join("media.db")).unwrap());
    let shared = MediaShared::new(
        backend,
        store,
        "img-default".into(),
        "vid-default".into(),
        max_per_day,
        Some(rows.clone()),
    );
    let channels = HashMap::from([("rec".to_string(), rec.clone() as Arc<dyn Channel>)]);
    let factory: AgentFactory = {
        let shared = shared.clone();
        Arc::new(move |sid, transcript| {
            let mut tools = ToolRegistry::new();
            let hidden = vec![ws.join(".hidden")];
            match tool {
                "generate_image" => tools.register(Arc::new(GenerateImageTool::new(
                    shared.clone(),
                    sid,
                    hidden,
                ))),
                _ => tools.register(Arc::new(GenerateVideoTool::new(
                    shared.clone(),
                    sid,
                    hidden,
                ))),
            }
            Ok(Agent::new(
                Arc::new(Caller {
                    tool,
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
    };
    let router = Arc::new(Router::new(sessions.path(), factory, channels));
    shared.bind(&router);
    Rig {
        _sessions: sessions,
        router,
        rec,
        rows,
        shared,
    }
}

async fn turn(rig: &Rig, text: &str) -> String {
    turn_in(rig, "c7", text).await
}

/// A turn in a named chat (a fresh chat is a fresh agent, so the Caller
/// calls its tool again).
async fn turn_in(rig: &Rig, chat: &str, text: &str) -> String {
    rig.router
        .dispatch_and_wait(InboundMessage {
            channel: "rec".into(),
            chat_id: chat.into(),
            sender: "max".into(),
            sender_id: None,
            message_id: "1".into(),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        })
        .await
        .unwrap()
        .text
}

fn poll_task(store_dir: &std::path::Path) -> ferrule_gateway::scheduler::Task {
    let tasks = TaskStore::open(store_dir.join("tasks.db")).unwrap();
    tasks
        .add(
            NewTask {
                name: "ferrule-media".into(),
                kind: TaskKind::Cron,
                schedule: "* * * * *".into(),
                timezone: "UTC".into(),
                channel: "rec".into(),
                chat_id: "c7".into(),
                prompt: String::new(),
                gate: None,
                model: None,
            },
            "ferrule-media".into(),
            0,
            None,
        )
        .unwrap()
}

#[tokio::test]
async fn an_image_is_saved_sent_and_counted() {
    let ws = tempfile::tempdir().unwrap();
    let r = rig(
        ws.path().to_path_buf(),
        "generate_image",
        json!({"prompt": "a copper logo"}),
        Arc::new(FakeMedia::default()),
        20,
    );
    let told = turn(&r, "draw me a logo").await;
    assert!(told.contains("and sent it to this chat"), "{told}");
    let sent = r.rec.sent.lock().unwrap().clone();
    let with_file: Vec<_> = sent.iter().filter(|m| !m.attachments.is_empty()).collect();
    assert_eq!(with_file.len(), 1, "{sent:?}");
    assert!(
        with_file[0].attachments[0]
            .url
            .replace('\\', "/")
            .contains("media/"),
        "{}",
        with_file[0].attachments[0].url
    );
    let saved = std::fs::read_dir(ws.path().join("media")).unwrap();
    assert_eq!(saved.count(), 1);
    // Counted for the cap and ledgered as media.
    assert_eq!(r.shared.store.count_since(0).unwrap(), 1);
    let rows = r.rows.0.lock().unwrap();
    assert!(
        rows.iter()
            .any(|row| row.call_kind == "media" && row.outcome == "ok"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn the_daily_cap_refuses_in_words() {
    let ws = tempfile::tempdir().unwrap();
    let r = rig(
        ws.path().to_path_buf(),
        "generate_image",
        json!({"prompt": "one"}),
        Arc::new(FakeMedia::default()),
        1,
    );
    let first = turn_in(&r, "c7", "one").await;
    assert!(first.contains("sent it to this chat"), "{first}");
    let second = turn_in(&r, "c8", "two").await;
    assert!(second.contains("daily media cap"), "{second}");
    assert!(!second.contains("sent it to this chat"), "{second}");
    assert_eq!(r.shared.store.count_since(0).unwrap(), 1);
}

#[tokio::test]
async fn a_video_is_answered_now_and_delivered_by_the_poller() {
    let ws = tempfile::tempdir().unwrap();
    let r = rig(
        ws.path().to_path_buf(),
        "generate_video",
        json!({"prompt": "the logo waves"}),
        Arc::new(FakeMedia::slow(1)),
        20,
    );
    let told = turn(&r, "make a clip").await;
    assert!(told.contains("job fake-1"), "{told}");
    // Nothing yet: the render takes minutes.
    assert!(r
        .rec
        .sent
        .lock()
        .unwrap()
        .iter()
        .all(|m| m.attachments.is_empty()));

    let poller = MediaPoller {
        shared: r.shared.clone(),
    };
    let task = poll_task(r.router_sessions_dir());
    // First poll: still pending.
    poller.run(&task).await.unwrap();
    assert!(r
        .rec
        .sent
        .lock()
        .unwrap()
        .iter()
        .all(|m| m.attachments.is_empty()));
    // Second poll: done — downloaded and delivered.
    poller.run(&task).await.unwrap();
    let sent = r.rec.sent.lock().unwrap().clone();
    let with_file: Vec<_> = sent.iter().filter(|m| !m.attachments.is_empty()).collect();
    assert_eq!(with_file.len(), 1, "{sent:?}");
    assert!(with_file[0].attachments[0].url.ends_with(".mp4"));
    assert_eq!(r.shared.store.pending_videos().unwrap().len(), 0);
}

#[tokio::test]
async fn a_failed_video_is_said_in_the_chat() {
    let ws = tempfile::tempdir().unwrap();
    let r = rig(
        ws.path().to_path_buf(),
        "generate_video",
        json!({"prompt": "a clip"}),
        Arc::new(FakeMedia::failing("the provider ran out of GPUs")),
        20,
    );
    let told = turn(&r, "make a clip").await;
    assert!(told.contains("job fake-1"), "{told}");
    let poller = MediaPoller {
        shared: r.shared.clone(),
    };
    let task = poll_task(r.router_sessions_dir());
    poller.run(&task).await.unwrap();
    let sent = r.rec.sent.lock().unwrap().clone();
    assert!(
        sent.iter()
            .any(|m| m.text.contains("didn't render") && m.text.contains("GPUs")),
        "{sent:?}"
    );
    assert_eq!(r.shared.store.pending_videos().unwrap().len(), 0);
    let rows = r.rows.0.lock().unwrap();
    assert!(rows.iter().any(|row| row.outcome == "error"), "{rows:?}");
}

impl Rig {
    fn router_sessions_dir(&self) -> &std::path::Path {
        self._sessions.path()
    }
}
