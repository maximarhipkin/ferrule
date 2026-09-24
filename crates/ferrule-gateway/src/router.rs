use crate::channel::Channel;
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use crate::session;
use ferrule_core::{Agent, CoreError, Role, Transcript};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Builds a ready-to-run `Agent` for a session: system prompt, provider,
/// tools and harness profile already applied. Receives the freshly created
/// (or reopened) `Transcript` so the agent logs into the same file the
/// router just read history from. Provider/tool wiring is a product
/// decision that belongs to the binary embedding this crate (`ferrule-cli`),
/// not to the gateway itself — this is the seam.
pub type AgentFactory = Arc<dyn Fn(&str, Transcript) -> Result<Agent, GatewayError> + Send + Sync>;

/// A message queued onto a lane, plus an optional oneshot the caller can use
/// to observe the *result* of the agent turn it produces. Plain `dispatch()`
/// (used by channel adapters via `Gateway::run`) leaves this `None` — fire-
/// and-forget, matching the original M1/M2 design. The scheduler (M3) needs
/// more than that: it must know whether a run truly succeeded or errored (a
/// NanoClaw-style run that silently logs `success` on a provider error is
/// exactly the bug this crate is meant not to reproduce), so
/// `dispatch_and_wait` fills this in and awaits it.
struct LaneJob {
    msg: InboundMessage,
    reply: Option<oneshot::Sender<Result<Reply, String>>>,
}

/// What an agent turn answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// Set when the run stopped before finishing (see `Agent::incomplete`):
    /// why. `text` is then a status of where the work stands.
    pub incomplete: Option<String>,
}

/// Routes inbound messages to one agent session per (channel, chat), each
/// with its own FIFO queue ("lane") so a chat's messages are processed in
/// order while different chats run fully concurrently as separate tokio
/// tasks. Session state is the existing JSONL transcript: a lane started
/// after a restart replays it into the new `Agent` before serving new
/// messages, so resume is "just" re-reading a file NanoClaw-style, not a
/// bespoke session store.
pub struct Router {
    sessions_dir: PathBuf,
    agent_factory: AgentFactory,
    channels: HashMap<String, Arc<dyn Channel>>,
    lanes: Mutex<HashMap<String, mpsc::Sender<LaneJob>>>,
    lane_queue_capacity: usize,
}

impl Router {
    pub fn new(sessions_dir: impl Into<PathBuf>, agent_factory: AgentFactory, channels: HashMap<String, Arc<dyn Channel>>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            agent_factory,
            channels,
            lanes: Mutex::new(HashMap::new()),
            lane_queue_capacity: 64,
        }
    }

    /// Enqueue an inbound message onto its session's lane, spawning the lane
    /// if this is the first message seen for that (channel, chat) pair since
    /// this router started. Fire-and-forget: the caller only learns the
    /// message was *queued*, not that the agent turn succeeded.
    pub async fn dispatch(&self, msg: InboundMessage) -> Result<(), GatewayError> {
        let sid = session::session_id(&msg.channel, &msg.chat_id);
        let tx = self.lane_for(&sid, &msg.channel).await?;
        tx.send(LaneJob { msg, reply: None }).await.map_err(|_| GatewayError::SessionClosed(sid))
    }

    /// Enqueue an inbound message and await the agent turn's own result:
    /// `Ok(reply)` when the run answered (a run that stopped early answers
    /// with a status, marked in `Reply::incomplete`), `Err` (carrying the
    /// error text) when `Agent::run` itself returned an error. Used by the
    /// scheduler, which must be able to tell a real failure apart from a
    /// normal reply rather than relying on the best-effort failure text
    /// `run_lane` also puts in chat.
    pub async fn dispatch_and_wait(&self, msg: InboundMessage) -> Result<Reply, GatewayError> {
        let sid = session::session_id(&msg.channel, &msg.chat_id);
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = self.lane_for(&sid, &msg.channel).await?;
        tx.send(LaneJob { msg, reply: Some(reply_tx) }).await.map_err(|_| GatewayError::SessionClosed(sid.clone()))?;
        match reply_rx.await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(err_text)) => Err(GatewayError::Channel(err_text)),
            Err(_) => Err(GatewayError::LaneClosed(sid)),
        }
    }

    async fn lane_for(&self, session_id: &str, channel_name: &str) -> Result<mpsc::Sender<LaneJob>, GatewayError> {
        let mut lanes = self.lanes.lock().await;
        if let Some(tx) = lanes.get(session_id) {
            if !tx.is_closed() {
                return Ok(tx.clone());
            }
        }
        let tx = self.spawn_lane(session_id, channel_name)?;
        lanes.insert(session_id.to_string(), tx.clone());
        Ok(tx)
    }

    fn spawn_lane(&self, session_id: &str, channel_name: &str) -> Result<mpsc::Sender<LaneJob>, GatewayError> {
        let transcript = Transcript::create(&self.sessions_dir, session_id)?;
        let history = transcript.read_messages().unwrap_or_default();
        let mut agent = (self.agent_factory)(session_id, transcript)?;
        // The factory already pushed a fresh system prompt (it may embed
        // live memory recall, workspace path, etc.) — replay only the prior
        // conversation on top of it, not the old system message.
        for m in history.into_iter().filter(|m| m.role != Role::System) {
            agent.messages.push(m);
        }
        let channel = self.channels.get(channel_name).cloned();
        let (tx, rx) = mpsc::channel(self.lane_queue_capacity);
        tokio::spawn(run_lane(agent, rx, channel, session_id.to_string()));
        Ok(tx)
    }
}

/// One session's serialized worker loop: pull the next message, run one
/// agent turn, deliver the reply. Errors never crash the lane — they are
/// logged and, when possible, reported back to the chat so the user isn't
/// left staring at silence (see NanoClaw's task-silent-death lesson: an
/// error that's only logged and never surfaced is worse than a visible one).
/// The job's own oneshot (if any) always gets the *true* `Result`, separate
/// from the best-effort chat text.
async fn run_lane(mut agent: Agent, mut rx: mpsc::Receiver<LaneJob>, channel: Option<Arc<dyn Channel>>, session_id: String) {
    while let Some(job) = rx.recv().await {
        let LaneJob { msg: inbound, reply } = job;
        // The agent loop wants a live event sender; the gateway doesn't
        // stream token-by-token to channels (yet), so the receiver is
        // dropped right away. It has to be: `Agent::emit` waits while a live
        // receiver's buffer is full, so one merely kept alive (`_erx`)
        // stalled the lane for good once a run passed 64 events.
        let (etx, erx) = mpsc::channel(1);
        drop(erx);
        let run_result = agent.run(&inbound.text, etx).await;
        let reply_text = match &run_result {
            Ok(answer) => answer.clone(),
            Err(e) => {
                tracing::error!(session = %session_id, error = %e, "agent run failed");
                failure_text(e)
            }
        };
        if let Some(ch) = &channel {
            let out = OutboundMessage {
                channel: inbound.channel.clone(),
                chat_id: inbound.chat_id.clone(),
                text: reply_text,
                reply_to: Some(inbound.message_id.clone()),
                attachments: vec![],
            };
            if let Err(e) = ch.send(out).await {
                tracing::error!(session = %session_id, error = %e, "failed to deliver reply");
            }
        }
        if let Some(reply_tx) = reply {
            let outcome = run_result.map(|text| Reply { text, incomplete: agent.incomplete.clone() }).map_err(|e| e.to_string());
            let _ = reply_tx.send(outcome); // receiver may have given up (e.g. caller timed out)
        }
    }
    tracing::debug!(session = %session_id, "session lane closed");
}

/// What the chat sees when a run fails outright, instead of silence.
fn failure_text(e: &CoreError) -> String {
    if e.is_transient() {
        format!("The model provider isn't answering right now, so I couldn't reply ({e}). Please try again in a few minutes.")
    } else {
        format!("Something went wrong and I couldn't reply: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse};
    use ferrule_core::tool::ToolContext;
    use ferrule_core::{AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{sleep, Duration, Instant};

    /// Echoes the incoming text; used to verify per-lane FIFO ordering.
    struct EchoProvider;
    #[async_trait]
    impl Provider for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let last_user = req.messages.iter().rev().find(|m| m.role == Role::User).and_then(|m| m.content.clone()).unwrap_or_default();
            Ok(CompletionResponse { message: Message::assistant(Some(format!("echo: {last_user}")), vec![], None), usage: Usage::default() })
        }
    }

    /// Reports how many *user* messages are in context — used to prove that
    /// a new Router instance pointed at the same sessions_dir resumes prior
    /// history instead of starting cold.
    struct CountingProvider;
    #[async_trait]
    impl Provider for CountingProvider {
        fn name(&self) -> &str {
            "counter"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let n = req.messages.iter().filter(|m| m.role == Role::User).count();
            Ok(CompletionResponse { message: Message::assistant(Some(format!("count: {n}")), vec![], None), usage: Usage::default() })
        }
    }

    /// Always fails — used to prove a provider error surfaces as a true
    /// `Err` through `dispatch_and_wait`, never a disguised `Ok`.
    struct FailingProvider;
    #[async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            Err(CoreError::Provider("simulated provider outage".into()))
        }
    }

    /// Calls a different missing tool each turn for `turns` turns, then
    /// answers: a long run, far past 64 events, that never looks stuck.
    struct LongRunProvider {
        turns: usize,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for LongRunProvider {
        fn name(&self) -> &str {
            "long"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let message = if n < self.turns {
                let call = ferrule_core::ToolCall { id: format!("c{n}"), name: "missing".into(), arguments: serde_json::json!({ "n": n }) };
                Message::assistant(None, vec![call], None)
            } else {
                Message::assistant(Some("finally".into()), vec![], None)
            };
            Ok(CompletionResponse { message, usage: Usage::default() })
        }
    }

    struct RecordingChannel {
        sent: std::sync::Mutex<Vec<OutboundMessage>>,
    }
    impl RecordingChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self { sent: std::sync::Mutex::new(Vec::new()) })
        }
        fn texts(&self) -> Vec<String> {
            self.sent.lock().unwrap().iter().map(|m| m.text.clone()).collect()
        }
    }
    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "test"
        }
        async fn run(&self, _tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            Ok(())
        }
        async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn inbound(chat_id: &str, text: &str) -> InboundMessage {
        static N: AtomicUsize = AtomicUsize::new(0);
        InboundMessage {
            channel: "test".into(),
            chat_id: chat_id.into(),
            sender: "user".into(),
            message_id: format!("m{}", N.fetch_add(1, Ordering::SeqCst)),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        }
    }

    fn echo_factory() -> AgentFactory {
        Arc::new(|_sid, transcript| {
            Ok(Agent::new(Arc::new(EchoProvider), ToolRegistry::new(), HarnessProfile::generic(), AgentConfig::default(), ToolContext::default(), Some(transcript))
                .with_system_prompt("test"))
        })
    }

    fn counting_factory() -> AgentFactory {
        Arc::new(|_sid, transcript| {
            Ok(Agent::new(Arc::new(CountingProvider), ToolRegistry::new(), HarnessProfile::generic(), AgentConfig::default(), ToolContext::default(), Some(transcript))
                .with_system_prompt("test"))
        })
    }

    fn failing_factory() -> AgentFactory {
        Arc::new(|_sid, transcript| {
            Ok(Agent::new(Arc::new(FailingProvider), ToolRegistry::new(), HarnessProfile::generic(), AgentConfig::default(), ToolContext::default(), Some(transcript))
                .with_system_prompt("test"))
        })
    }

    /// Polls until `f()` is true or the deadline passes — replies land
    /// asynchronously on a spawned lane, so tests can't assert immediately
    /// after `dispatch` returns (dispatch only guarantees the message was
    /// queued, not processed).
    async fn wait_until(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !f() {
            assert!(Instant::now() < deadline, "condition never became true");
            sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn same_chat_messages_are_processed_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let router = Router::new(dir.path(), echo_factory(), channels);

        router.dispatch(inbound("chat-1", "one")).await.unwrap();
        router.dispatch(inbound("chat-1", "two")).await.unwrap();
        router.dispatch(inbound("chat-1", "three")).await.unwrap();

        wait_until(|| recorder.texts().len() == 3).await;
        assert_eq!(recorder.texts(), vec!["echo: one", "echo: two", "echo: three"]);
    }

    #[tokio::test]
    async fn different_chats_get_independent_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let router = Router::new(dir.path(), echo_factory(), channels);

        router.dispatch(inbound("chat-a", "hi a")).await.unwrap();
        router.dispatch(inbound("chat-b", "hi b")).await.unwrap();

        wait_until(|| recorder.texts().len() == 2).await;
        let mut texts = recorder.texts();
        texts.sort();
        assert_eq!(texts, vec!["echo: hi a", "echo: hi b"]);
    }

    #[tokio::test]
    async fn resumes_history_from_transcript_across_router_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());

        // First "process": one message, then the router is dropped —
        // simulating a gateway restart. Only the JSONL transcript survives.
        {
            let router = Router::new(dir.path(), counting_factory(), channels.clone());
            router.dispatch(inbound("chat-1", "first")).await.unwrap();
            wait_until(|| recorder.texts().len() == 1).await;
            assert_eq!(recorder.texts(), vec!["count: 1"]);
        }

        // Second "process": brand new Router, same sessions_dir. The lane it
        // spawns must replay the earlier user turn before this new one.
        {
            let router = Router::new(dir.path(), counting_factory(), channels);
            router.dispatch(inbound("chat-1", "second")).await.unwrap();
            wait_until(|| recorder.texts().len() == 2).await;
            assert_eq!(recorder.texts()[1], "count: 2");
        }
    }

    #[tokio::test]
    async fn dispatch_and_wait_returns_the_true_answer() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new(dir.path(), echo_factory(), HashMap::new());
        let answer = router.dispatch_and_wait(inbound("chat-1", "ping")).await.unwrap();
        assert_eq!(answer, Reply { text: "echo: ping".into(), incomplete: None });
    }

    /// The exact bug class this method exists to prevent: a provider error
    /// must never be observable as `Ok(..)` through `dispatch_and_wait` —
    /// unlike the best-effort "internal error: …" text `run_lane` puts in
    /// chat, this is the caller's one source of truth for pass/fail.
    #[tokio::test]
    async fn dispatch_and_wait_surfaces_a_provider_error_as_err_never_ok() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new(dir.path(), failing_factory(), HashMap::new());
        let result = router.dispatch_and_wait(inbound("chat-1", "ping")).await;
        assert!(result.is_err(), "provider error must not be reported as success");
        assert!(result.unwrap_err().to_string().contains("simulated provider outage"));
    }

    #[tokio::test]
    async fn a_long_run_does_not_stall_the_lane() {
        let dir = tempfile::tempdir().unwrap();
        let factory: AgentFactory = Arc::new(|_sid, transcript| {
            let provider = Arc::new(LongRunProvider { turns: 40, calls: AtomicUsize::new(0) });
            Ok(Agent::new(provider, ToolRegistry::new(), HarnessProfile::generic(), AgentConfig::default(), ToolContext::default(), Some(transcript)))
        });
        let router = Router::new(dir.path(), factory, HashMap::new());
        let reply = tokio::time::timeout(Duration::from_secs(10), router.dispatch_and_wait(inbound("chat-1", "go"))).await;
        assert_eq!(reply.expect("the lane stalled").unwrap().text, "finally");
    }

    #[tokio::test]
    async fn a_failed_run_tells_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let router = Router::new(dir.path(), failing_factory(), channels);
        router.dispatch(inbound("chat-1", "ping")).await.unwrap();
        wait_until(|| recorder.texts().len() == 1).await;
        assert!(recorder.texts()[0].starts_with("Something went wrong and I couldn't reply"), "{:?}", recorder.texts());
    }

    #[tokio::test]
    async fn dispatch_and_wait_does_not_double_deliver_through_the_channel() {
        // Scheduler-triggered sessions use a pseudo-channel name that is
        // deliberately never registered in the router's channel map, so the
        // lane's own "reply to inbound.channel" path naturally no-ops —
        // delivery to the task's real destination is the scheduler's job,
        // done once, after `dispatch_and_wait` returns the true answer.
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let router = Router::new(dir.path(), echo_factory(), channels);

        let mut msg = inbound("chat-1", "ping");
        msg.channel = "scheduler".into(); // not registered above
        let answer = router.dispatch_and_wait(msg).await.unwrap();

        assert_eq!(answer.text, "echo: ping");
        assert!(recorder.texts().is_empty(), "unregistered pseudo-channel must not receive a delivery");
    }
}
