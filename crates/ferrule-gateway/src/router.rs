use crate::channel::Channel;
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use crate::scheduler::SCHEDULER_PSEUDO_CHANNEL;
use crate::session;
use ferrule_core::{Agent, AgentEvent, CoreError, Guard, GuardedCall, Role, Transcript, Verdict};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, oneshot, Notify};

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
    lanes: Mutex<HashMap<String, Lane>>,
    lane_queue_capacity: usize,
    /// M19b: a turn running longer than this is ended the way `/stop` ends
    /// one, and the lane takes its next message.
    max_turn: Option<Duration>,
    /// Woken whenever a lane starts or ends a turn (the running marker).
    changed: Arc<Notify>,
}

struct Lane {
    tx: mpsc::Sender<LaneJob>,
    channel: String,
    chat_id: String,
    state: Arc<Mutex<LaneState>>,
}

/// What a lane is doing, for `/status`, the busy notice and the watchdog
/// (M19b). Fed by the lane itself and by its agent's events.
#[derive(Default)]
struct LaneState {
    /// When the current turn started; `None` while idle.
    busy_since: Option<(Instant, SystemTime)>,
    /// The message the turn is handling.
    text: String,
    /// "a model call", or a tool and a short summary of its arguments.
    activity: String,
    /// The last agent event (a model call or a tool starting or finishing).
    last_progress: Option<Instant>,
    /// Messages waiting behind the current turn.
    queued: usize,
    /// The chat was told it's queued, this busy period.
    busy_notice_sent: bool,
    /// The owner was told this turn is stuck, since its last progress.
    stall_reported: bool,
}

/// One lane as `/status` and the watchdog see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub session_id: String,
    pub channel: String,
    pub chat_id: String,
    /// `None` while idle.
    pub busy_for: Option<Duration>,
    pub started_at: Option<SystemTime>,
    pub activity: String,
    pub since_progress: Option<Duration>,
    pub queued: usize,
    /// The message the turn is handling (unredacted).
    pub text: String,
}

impl LaneSnapshot {
    fn of(session_id: &str, lane: &Lane, now: Instant) -> Self {
        let st = lane.state.lock().unwrap();
        Self {
            session_id: session_id.to_string(),
            channel: lane.channel.clone(),
            chat_id: lane.chat_id.clone(),
            busy_for: st
                .busy_since
                .map(|(at, _)| now.saturating_duration_since(at)),
            started_at: st.busy_since.map(|(_, at)| at),
            activity: st.activity.clone(),
            since_progress: st.last_progress.map(|at| now.saturating_duration_since(at)),
            queued: st.queued,
            text: st.text.clone(),
        }
    }

    /// Where the turn came from: "telegram chat 42", "scheduled task x".
    pub fn place(&self) -> String {
        if self.channel == SCHEDULER_PSEUDO_CHANNEL {
            format!("scheduled task {}", self.chat_id)
        } else {
            format!("{} chat {}", self.channel, self.chat_id)
        }
    }
}

impl Router {
    pub fn new(
        sessions_dir: impl Into<PathBuf>,
        agent_factory: AgentFactory,
        channels: HashMap<String, Arc<dyn Channel>>,
    ) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
            agent_factory,
            channels,
            lanes: Mutex::new(HashMap::new()),
            lane_queue_capacity: 64,
            max_turn: None,
            changed: Arc::new(Notify::new()),
        }
    }

    /// Ends any turn that runs longer than `limit` (M19b's
    /// `max_turn_minutes`), through the agent's guard, as `/stop` would.
    pub fn with_max_turn(mut self, limit: Option<Duration>) -> Self {
        self.max_turn = limit.filter(|d| !d.is_zero());
        self
    }

    /// Notified whenever a lane starts or finishes a turn.
    pub fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    /// Every lane that is running a turn or has messages waiting.
    pub fn snapshot(&self) -> Vec<LaneSnapshot> {
        let now = Instant::now();
        let lanes = self.lanes.lock().unwrap();
        let mut out: Vec<LaneSnapshot> = lanes
            .iter()
            .map(|(sid, lane)| LaneSnapshot::of(sid, lane, now))
            .filter(|l| l.busy_for.is_some() || l.queued > 0)
            .collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }

    /// If `msg`'s chat is in the middle of a turn and hasn't been told so
    /// this busy period, marks it told and returns the lane: the caller
    /// sends the busy notice. Only turns running at least `after` count.
    pub fn claim_busy_notice(&self, msg: &InboundMessage, after: Duration) -> Option<LaneSnapshot> {
        let sid = session::session_id(&msg.channel, &msg.chat_id);
        let lanes = self.lanes.lock().unwrap();
        let lane = lanes.get(&sid)?;
        let snap = LaneSnapshot::of(&sid, lane, Instant::now());
        let mut st = lane.state.lock().unwrap();
        if st.busy_notice_sent || snap.busy_for? < after {
            return None;
        }
        st.busy_notice_sent = true;
        Some(snap)
    }

    /// Lanes that have made no progress for `after` and haven't been
    /// reported since their last progress; marks them reported.
    pub fn claim_stalled(&self, after: Duration) -> Vec<LaneSnapshot> {
        let now = Instant::now();
        let lanes = self.lanes.lock().unwrap();
        let mut out = Vec::new();
        for (sid, lane) in lanes.iter() {
            let snap = LaneSnapshot::of(sid, lane, now);
            let mut st = lane.state.lock().unwrap();
            if st.busy_since.is_some()
                && !st.stall_reported
                && snap.since_progress.is_some_and(|d| d >= after)
            {
                st.stall_reported = true;
                out.push(snap);
            }
        }
        out
    }

    /// Enqueue an inbound message onto its session's lane, spawning the lane
    /// if this is the first message seen for that (channel, chat) pair since
    /// this router started. Fire-and-forget: the caller only learns the
    /// message was *queued*, not that the agent turn succeeded.
    pub async fn dispatch(&self, msg: InboundMessage) -> Result<(), GatewayError> {
        let sid = session::session_id(&msg.channel, &msg.chat_id);
        let (tx, state) = self.lane_for(&sid, &msg)?;
        state.lock().unwrap().queued += 1;
        let sent = tx.send(LaneJob { msg, reply: None }).await;
        if sent.is_err() {
            state.lock().unwrap().queued -= 1;
        }
        sent.map_err(|_| GatewayError::SessionClosed(sid))
    }

    /// Like [`Router::dispatch`], but never waits: a lane whose queue is
    /// full refuses the message (`QueueFull`), so the gateway's dispatcher
    /// stays free for `/status` and `/stop` while a turn hangs.
    pub fn offer(&self, msg: InboundMessage) -> Result<(), GatewayError> {
        let sid = session::session_id(&msg.channel, &msg.chat_id);
        let (tx, state) = self.lane_for(&sid, &msg)?;
        state.lock().unwrap().queued += 1;
        let sent = tx.try_send(LaneJob { msg, reply: None });
        if sent.is_err() {
            state.lock().unwrap().queued -= 1;
        }
        sent.map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => GatewayError::QueueFull(sid),
            mpsc::error::TrySendError::Closed(_) => GatewayError::SessionClosed(sid),
        })
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
        let (tx, state) = self.lane_for(&sid, &msg)?;
        state.lock().unwrap().queued += 1;
        let sent = tx
            .send(LaneJob {
                msg,
                reply: Some(reply_tx),
            })
            .await;
        if sent.is_err() {
            state.lock().unwrap().queued -= 1;
        }
        sent.map_err(|_| GatewayError::SessionClosed(sid.clone()))?;
        match reply_rx.await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(err_text)) => Err(GatewayError::Channel(err_text)),
            Err(_) => Err(GatewayError::LaneClosed(sid)),
        }
    }

    fn lane_for(
        &self,
        session_id: &str,
        msg: &InboundMessage,
    ) -> Result<(mpsc::Sender<LaneJob>, Arc<Mutex<LaneState>>), GatewayError> {
        let mut lanes = self.lanes.lock().unwrap();
        if let Some(lane) = lanes.get(session_id) {
            if !lane.tx.is_closed() {
                return Ok((lane.tx.clone(), lane.state.clone()));
            }
        }
        let state = Arc::new(Mutex::new(LaneState::default()));
        let tx = self.spawn_lane(session_id, &msg.channel, state.clone())?;
        lanes.insert(
            session_id.to_string(),
            Lane {
                tx: tx.clone(),
                channel: msg.channel.clone(),
                chat_id: msg.chat_id.clone(),
                state: state.clone(),
            },
        );
        Ok((tx, state))
    }

    /// Drops `session_id`'s lane once its queue drains: the next message
    /// starts a new agent that replays the transcript, with the tools the
    /// factory gives it then (M19: a plan's exploration is read-only, its
    /// execution isn't). False when there was no lane.
    pub fn retire(&self, session_id: &str) -> bool {
        self.lanes.lock().unwrap().remove(session_id).is_some()
    }

    /// Whether `session_id` is a chat that [`Router::wake`] can run: it has
    /// a lane, and a person on the other end (a scheduled task has none).
    pub fn can_wake(&self, session_id: &str) -> bool {
        self.lanes
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|l| !l.tx.is_closed() && l.channel != SCHEDULER_PSEUDO_CHANNEL)
    }

    /// Runs `session_id`'s agent on `text` as if it came from its chat,
    /// and sends the answer there (not as a reply to anything). For news
    /// the agent should act on while nobody is talking to it: its
    /// sub-agents finishing. False when there's no such chat or its queue
    /// is full.
    pub fn wake(&self, session_id: &str, text: String) -> bool {
        let lanes = self.lanes.lock().unwrap();
        let Some(lane) = lanes.get(session_id) else {
            return false;
        };
        if lane.channel == SCHEDULER_PSEUDO_CHANNEL {
            return false;
        }
        let msg = InboundMessage {
            channel: lane.channel.clone(),
            chat_id: lane.chat_id.clone(),
            sender: "ferrule".into(),
            message_id: String::new(),
            text,
            attachments: vec![],
            reply_to: None,
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        lane.state.lock().unwrap().queued += 1;
        let sent = lane.tx.try_send(LaneJob { msg, reply: None }).is_ok();
        if !sent {
            lane.state.lock().unwrap().queued -= 1;
        }
        sent
    }

    fn spawn_lane(
        &self,
        session_id: &str,
        channel_name: &str,
        state: Arc<Mutex<LaneState>>,
    ) -> Result<mpsc::Sender<LaneJob>, GatewayError> {
        let transcript = Transcript::create(&self.sessions_dir, session_id)?;
        let history = transcript.read_messages().unwrap_or_default();
        let mut agent = (self.agent_factory)(session_id, transcript)?;
        // The factory already pushed a fresh system prompt (it may embed
        // live memory recall, workspace path, etc.) — replay only the prior
        // conversation on top of it, not the old system message.
        for m in history.into_iter().filter(|m| m.role != Role::System) {
            agent.messages.push(m);
        }
        // The turn's deadline sits in front of whatever guard the factory
        // gave the agent (M19's), so it ends a turn the way `/stop` does.
        let deadline = self.max_turn.map(|limit| {
            let d = Arc::new(TurnDeadline::new(agent.guard(), limit));
            agent.set_guard(d.clone());
            d
        });
        let channel = self.channels.get(channel_name).cloned();
        let (tx, rx) = mpsc::channel(self.lane_queue_capacity);
        let watch = LaneWatch {
            state,
            changed: self.changed.clone(),
            deadline,
        };
        tokio::spawn(run_lane(agent, rx, channel, session_id.to_string(), watch));
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
async fn run_lane(
    mut agent: Agent,
    mut rx: mpsc::Receiver<LaneJob>,
    channel: Option<Arc<dyn Channel>>,
    session_id: String,
    watch: LaneWatch,
) {
    while let Some(job) = rx.recv().await {
        let LaneJob {
            msg: inbound,
            reply,
        } = job;
        watch.start(&inbound.text);
        // The agent's events only feed the lane's state (what it's doing,
        // when it last moved). They're drained by their own task that
        // never waits: `Agent::emit` blocks while a live receiver's buffer
        // is full, and a stalled drain once stalled the lane for good.
        let (etx, erx) = mpsc::channel(256);
        let drain = tokio::spawn(drain_events(
            erx,
            watch.state.clone(),
            watch.changed.clone(),
        ));
        if let Some(d) = &watch.deadline {
            d.arm();
        }
        let run_result = agent.run(&inbound.text, etx).await;
        if let Some(d) = &watch.deadline {
            d.disarm();
        }
        // Whatever still holds a sender (a sub-agent) now finds it closed
        // rather than feeding the next turn's state.
        drain.abort();
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
                // A wake-up has no message to reply to.
                reply_to: (!inbound.message_id.is_empty()).then(|| inbound.message_id.clone()),
                attachments: vec![],
            };
            if let Err(e) = ch.send(out).await {
                tracing::error!(session = %session_id, error = %e, "failed to deliver reply");
            }
        }
        if let Some(reply_tx) = reply {
            let outcome = run_result
                .map(|text| Reply {
                    text,
                    incomplete: agent.incomplete.clone(),
                })
                .map_err(|e| e.to_string());
            let _ = reply_tx.send(outcome); // receiver may have given up (e.g. caller timed out)
        }
        watch.finish();
    }
    tracing::debug!(session = %session_id, "session lane closed");
}

/// A lane's handles on its own state.
struct LaneWatch {
    state: Arc<Mutex<LaneState>>,
    changed: Arc<Notify>,
    deadline: Option<Arc<TurnDeadline>>,
}

impl LaneWatch {
    fn start(&self, text: &str) {
        {
            let mut st = self.state.lock().unwrap();
            st.queued = st.queued.saturating_sub(1);
            st.busy_since = Some((Instant::now(), SystemTime::now()));
            st.text = text.to_string();
            st.activity = "starting".into();
            st.last_progress = Some(Instant::now());
            st.stall_reported = false;
        }
        self.changed.notify_waiters();
    }

    fn finish(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.busy_since = None;
            st.text.clear();
            st.activity.clear();
            st.last_progress = None;
            if st.queued == 0 {
                st.busy_notice_sent = false;
            }
        }
        self.changed.notify_waiters();
    }
}

async fn drain_events(
    mut rx: mpsc::Receiver<AgentEvent>,
    state: Arc<Mutex<LaneState>>,
    changed: Arc<Notify>,
) {
    while let Some(ev) = rx.recv().await {
        let activity = match &ev {
            AgentEvent::RunStarted { .. }
            | AgentEvent::ToolCallFinished { .. }
            | AgentEvent::VerifyFinished { .. } => Some("a model call".to_string()),
            AgentEvent::ToolCallStarted {
                name, arguments, ..
            } => Some(tool_activity(name, arguments)),
            AgentEvent::VerifyStarted { check } => Some(format!("the check `{check}`")),
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                ..
            } => Some(format!("a model call (retry {attempt} of {max_attempts})")),
            _ => None,
        };
        let moved = {
            let mut st = state.lock().unwrap();
            st.last_progress = Some(Instant::now());
            st.stall_reported = false;
            match activity {
                Some(a) if a != st.activity => {
                    st.activity = a;
                    true
                }
                _ => false,
            }
        };
        // The status file shows what the lane is doing now, not 5 s ago.
        if moved {
            changed.notify_waiters();
        }
    }
}

/// "tool `shell` (sleep 100)": the tool and the one argument that says
/// what it's doing, cut short.
pub fn tool_activity(name: &str, args: &serde_json::Value) -> String {
    let key = [
        "command", "path", "url", "query", "pattern", "task", "name", "id",
    ]
    .into_iter()
    .find_map(|k| args.get(k).and_then(|v| v.as_str()));
    let detail = match key {
        Some(v) => v.to_string(),
        None if args.as_object().is_some_and(|o| o.is_empty()) || args.is_null() => String::new(),
        None => args.to_string(),
    };
    let detail: String = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.is_empty() {
        return format!("tool `{name}`");
    }
    format!("tool `{name}` ({})", crate::health::clip(&detail, 60))
}

/// M19b's `max_turn_minutes`: a guard in front of the agent's own that
/// halts the turn once it's past its deadline, the way the kill switch
/// does (the running model or tool call is dropped).
struct TurnDeadline {
    inner: Option<Arc<dyn Guard>>,
    limit: Duration,
    deadline: Mutex<Option<tokio::time::Instant>>,
}

impl TurnDeadline {
    fn new(inner: Option<Arc<dyn Guard>>, limit: Duration) -> Self {
        Self {
            inner,
            limit,
            deadline: Mutex::new(None),
        }
    }

    fn arm(&self) {
        *self.deadline.lock().unwrap() = Some(tokio::time::Instant::now() + self.limit);
    }

    fn disarm(&self) {
        *self.deadline.lock().unwrap() = None;
    }

    fn message(&self) -> String {
        format!(
            "Stopped: this turn ran for {} (max_turn_minutes), so I ended it to free the chat. Nothing after the last step was done; send a new message to continue.",
            crate::health::human(self.limit)
        )
    }
}

#[async_trait::async_trait]
impl Guard for TurnDeadline {
    fn begin(&self) {
        if let Some(g) = &self.inner {
            g.begin();
        }
    }

    fn before_model_call(&self) -> Option<String> {
        if let Some(why) = self.inner.as_ref().and_then(|g| g.before_model_call()) {
            return Some(why);
        }
        let deadline = *self.deadline.lock().unwrap();
        deadline
            .is_some_and(|d| tokio::time::Instant::now() >= d)
            .then(|| self.message())
    }

    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict {
        match &self.inner {
            Some(g) => g.before_tool_call(call).await,
            None => Verdict::Allow,
        }
    }

    async fn halted(&self) -> String {
        let deadline = *self.deadline.lock().unwrap();
        let expired = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        match &self.inner {
            Some(g) => tokio::select! {
                why = g.halted() => why,
                _ = expired => self.message(),
            },
            None => {
                expired.await;
                self.message()
            }
        }
    }
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
    use ferrule_core::{
        AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage,
    };
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
            let last_user = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            Ok(CompletionResponse {
                message: Message::assistant(Some(format!("echo: {last_user}")), vec![], None),
                usage: Usage::default(),
            })
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
            Ok(CompletionResponse {
                message: Message::assistant(Some(format!("count: {n}")), vec![], None),
                usage: Usage::default(),
            })
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
                let call = ferrule_core::ToolCall {
                    id: format!("c{n}"),
                    name: "missing".into(),
                    arguments: serde_json::json!({ "n": n }),
                };
                Message::assistant(None, vec![call], None)
            } else {
                Message::assistant(Some("finally".into()), vec![], None)
            };
            Ok(CompletionResponse {
                message,
                usage: Usage::default(),
            })
        }
    }

    struct RecordingChannel {
        sent: std::sync::Mutex<Vec<OutboundMessage>>,
    }
    impl RecordingChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect()
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
            Ok(Agent::new(
                Arc::new(EchoProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        })
    }

    fn counting_factory() -> AgentFactory {
        Arc::new(|_sid, transcript| {
            Ok(Agent::new(
                Arc::new(CountingProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        })
    }

    fn failing_factory() -> AgentFactory {
        Arc::new(|_sid, transcript| {
            Ok(Agent::new(
                Arc::new(FailingProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
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
    async fn a_chat_can_be_woken_and_the_answer_goes_to_it_unthreaded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let router = Router::new(dir.path(), echo_factory(), channels);
        let sid = session::session_id("test", "chat-1");

        // Nothing to wake before the chat has spoken.
        assert!(!router.can_wake(&sid));
        assert!(!router.wake(&sid, "news".into()));

        router.dispatch(inbound("chat-1", "hi")).await.unwrap();
        assert!(router.can_wake(&sid));
        assert!(router.wake(&sid, "news".into()));
        wait_until(|| recorder.texts().len() == 2).await;
        let sent = recorder.sent.lock().unwrap();
        assert_eq!(sent[1].text, "echo: news");
        assert_eq!(sent[1].chat_id, "chat-1");
        assert!(sent[0].reply_to.is_some());
        assert_eq!(sent[1].reply_to, None);
    }

    #[tokio::test]
    async fn a_scheduled_task_is_never_woken() {
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new(dir.path(), echo_factory(), HashMap::new());
        let mut msg = inbound("task-1", "run");
        msg.channel = SCHEDULER_PSEUDO_CHANNEL.into();
        router.dispatch_and_wait(msg).await.unwrap();
        let sid = session::session_id(SCHEDULER_PSEUDO_CHANNEL, "task-1");
        assert!(!router.can_wake(&sid));
        assert!(!router.wake(&sid, "news".into()));
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
        assert_eq!(
            recorder.texts(),
            vec!["echo: one", "echo: two", "echo: three"]
        );
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
    async fn a_retired_lane_is_rebuilt_from_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let built = Arc::new(AtomicUsize::new(0));
        let factory: AgentFactory = {
            let built = built.clone();
            let counting = counting_factory();
            Arc::new(move |sid, transcript| {
                built.fetch_add(1, Ordering::SeqCst);
                counting(sid, transcript)
            })
        };
        let router = Router::new(dir.path(), factory, HashMap::new());
        let first = router
            .dispatch_and_wait(inbound("chat-1", "look"))
            .await
            .unwrap();
        assert_eq!(first.text, "count: 1");
        assert!(router.retire(&session::session_id("test", "chat-1")));
        assert!(!router.retire(&session::session_id("test", "chat-1")));
        let second = router
            .dispatch_and_wait(inbound("chat-1", "do it"))
            .await
            .unwrap();
        assert_eq!(
            second.text, "count: 2",
            "the new agent replayed the first turn"
        );
        assert_eq!(built.load(Ordering::SeqCst), 2);
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
        let answer = router
            .dispatch_and_wait(inbound("chat-1", "ping"))
            .await
            .unwrap();
        assert_eq!(
            answer,
            Reply {
                text: "echo: ping".into(),
                incomplete: None
            }
        );
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
        assert!(
            result.is_err(),
            "provider error must not be reported as success"
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated provider outage"));
    }

    #[tokio::test]
    async fn a_long_run_does_not_stall_the_lane() {
        let dir = tempfile::tempdir().unwrap();
        let factory: AgentFactory = Arc::new(|_sid, transcript| {
            let provider = Arc::new(LongRunProvider {
                turns: 40,
                calls: AtomicUsize::new(0),
            });
            Ok(Agent::new(
                provider,
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            ))
        });
        let router = Router::new(dir.path(), factory, HashMap::new());
        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            router.dispatch_and_wait(inbound("chat-1", "go")),
        )
        .await;
        assert_eq!(reply.expect("the lane stalled").unwrap().text, "finally");
    }

    /// Hangs on "hang"; echoes anything else.
    struct HangingProvider;
    #[async_trait]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let last = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            if last == "hang" {
                std::future::pending::<()>().await;
            }
            Ok(CompletionResponse {
                message: Message::assistant(Some(format!("echo: {last}")), vec![], None),
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn max_turn_ends_a_hanging_turn_and_frees_the_lane() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());
        let factory: AgentFactory = Arc::new(|_sid, transcript| {
            Ok(Agent::new(
                Arc::new(HangingProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        });
        let router = Router::new(dir.path(), factory, channels)
            .with_max_turn(Some(Duration::from_millis(150)));
        router.dispatch(inbound("chat-1", "hang")).await.unwrap();
        router.dispatch(inbound("chat-1", "after")).await.unwrap();
        wait_until(|| recorder.texts().len() == 2).await;
        let texts = recorder.texts();
        assert!(
            texts[0].starts_with("Stopped: this turn ran for 0 s (max_turn_minutes)"),
            "{texts:?}"
        );
        // The deadline is per turn: the next one isn't born expired.
        assert_eq!(texts[1], "echo: after");
        wait_until(|| router.snapshot().is_empty()).await;
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
        assert!(
            recorder.texts()[0].starts_with("Something went wrong and I couldn't reply"),
            "{:?}",
            recorder.texts()
        );
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
        assert!(
            recorder.texts().is_empty(),
            "unregistered pseudo-channel must not receive a delivery"
        );
    }
}
