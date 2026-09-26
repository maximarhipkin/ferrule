//! M27: the agent streams each answer call's text to its reply stream, a
//! reset before every call and every retry, and times the first byte and
//! the first visible text into the ledger.

use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, CoreError, Delta, DeltaSink, HarnessProfile, LedgerRecord, LedgerSink,
    Message, Provider, RetryPolicy, ToolRegistry, Usage,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Streams "Let me look." with a tool call, then "All " "done."; with
/// `flaky`, the first attempt streams "half" and then fails.
struct Streamer {
    flaky: bool,
    calls: AtomicUsize,
    streamed: Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl Provider for Streamer {
    fn name(&self) -> &str {
        "streamer"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        self.streamed.lock().unwrap().push(req.stream.is_some());
        let send = |t: &str| {
            if let Some(s) = &req.stream {
                s.send(Delta::Progress);
                s.send(Delta::Text(t.into()));
            }
        };
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let n = if self.flaky {
            if n == 0 {
                send("half");
                return Err(CoreError::Transient {
                    message: "stream broke after 2 events: reset".into(),
                    retry_after: None,
                });
            }
            n - 1
        } else {
            n
        };
        let message = if n == 0 {
            send("Let me look.");
            Message::assistant(
                Some("Let me look.".into()),
                vec![ToolCall {
                    id: "c0".into(),
                    name: "look".into(),
                    arguments: json!({}),
                }],
                None,
            )
        } else {
            send("All ");
            send("done.");
            Message::assistant(Some("All done.".into()), vec![], None)
        };
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Usage::default()
            },
        })
    }
}

struct Look;

#[async_trait::async_trait]
impl Tool for Look {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "look".into(),
            description: "looks".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok("seen"))
    }
}

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, record: LedgerRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn agent(provider: Arc<Streamer>, rows: Arc<Rows>) -> Agent {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Look));
    Agent::new(
        provider,
        tools,
        HarnessProfile::generic(),
        AgentConfig {
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(1),
                budget: Duration::from_secs(5),
            },
            ..AgentConfig::default()
        },
        ToolContext::default(),
        None,
    )
    .with_system_prompt("test")
    .with_ledger(rows, "chat", None, "m")
}

fn streamer(flaky: bool) -> Arc<Streamer> {
    Arc::new(Streamer {
        flaky,
        calls: AtomicUsize::new(0),
        streamed: Mutex::default(),
    })
}

fn collect() -> (DeltaSink, Arc<Mutex<Vec<Delta>>>) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let keep = got.clone();
    (
        DeltaSink::new(move |d| {
            if d != Delta::Progress {
                keep.lock().unwrap().push(d)
            }
        }),
        got,
    )
}

fn text(t: &str) -> Delta {
    Delta::Text(t.into())
}

#[tokio::test]
async fn each_call_resets_the_stream_and_the_last_one_is_the_answer() {
    let rows = Arc::new(Rows::default());
    let mut agent = agent(streamer(false), rows.clone());
    let (sink, got) = collect();
    agent.set_reply_stream(Some(sink));
    let (tx, _rx) = mpsc::channel(256);
    assert_eq!(agent.run("go", tx).await.unwrap(), "All done.");
    assert_eq!(
        *got.lock().unwrap(),
        [
            Delta::Reset,
            text("Let me look."),
            Delta::Reset,
            text("All "),
            text("done.")
        ]
    );

    // The first byte of each call is timed; the first visible text once a run.
    let rows = rows.0.lock().unwrap();
    assert_eq!(rows.len(), 2);
    let speed: Vec<_> = rows.iter().map(|r| r.speed.clone().unwrap()).collect();
    assert!(speed.iter().all(|s| s.first_token_ms.is_some()));
    assert!(speed[0].first_visible_ms.is_some());
    assert!(speed[1].first_visible_ms.is_none());
}

#[tokio::test]
async fn a_retry_replaces_what_the_failed_attempt_showed() {
    let rows = Arc::new(Rows::default());
    let provider = streamer(true);
    let mut agent = agent(provider.clone(), rows);
    let (sink, got) = collect();
    agent.set_reply_stream(Some(sink));
    let (tx, _rx) = mpsc::channel(256);
    agent.run("go", tx).await.unwrap();
    let got = got.lock().unwrap();
    assert_eq!(
        got[..4],
        [
            Delta::Reset,
            text("half"),
            Delta::Reset,
            text("Let me look.")
        ]
    );
    assert_eq!(*provider.streamed.lock().unwrap(), [true, true, true]);
}

#[tokio::test]
async fn without_a_reply_stream_no_call_streams() {
    let rows = Arc::new(Rows::default());
    let provider = streamer(false);
    let mut agent = agent(provider.clone(), rows.clone());
    let (tx, _rx) = mpsc::channel(256);
    agent.run("go", tx).await.unwrap();
    assert_eq!(*provider.streamed.lock().unwrap(), [false, false]);
    // And the ledger rows are today's: no speed block for a plain turn.
    assert!(rows.0.lock().unwrap().iter().all(|r| r.speed.is_none()));
}
