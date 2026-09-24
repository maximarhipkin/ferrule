use crate::channel::Channel;
use crate::error::GatewayError;
use crate::health::{human, is_command, Health, Redactor};
use crate::message::{InboundMessage, OutboundMessage};
use crate::router::Router;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// The receipt: the gateway saw the message (M19b).
pub const ACK_EMOJI: &str = "👀";
/// How long the dispatcher waits for the receipt before moving on; the
/// reaction keeps trying in the background.
const ACK_WAIT: Duration = Duration::from_secs(2);
/// A turn that has run less than this doesn't earn a busy notice: it's
/// probably about to answer.
pub const BUSY_NOTICE_AFTER: Duration = Duration::from_secs(3);

/// Ties one or more channel adapters to a `Router`. Every adapter pushes
/// onto the same inbound funnel and runs concurrently as its own tokio task;
/// the gateway's only job is fan-in + dispatch, mirroring NanoClaw's
/// "host router" rather than a heavier broker.
pub struct Gateway {
    channels: Vec<Arc<dyn Channel>>,
    router: Arc<Router>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    busy_notice_after: Duration,
    redactor: Arc<Redactor>,
    health: Option<Arc<Health>>,
}

/// Looks at every inbound message before the router does (M19: the owner's
/// `/stop`, `/resume`, `/plan` and approval replies). `Some(reply)` takes
/// the message: the reply goes back to its chat and no agent turn runs.
#[async_trait::async_trait]
pub trait Interceptor: Send + Sync {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String>;
}

impl Gateway {
    /// Takes an `Arc<Router>` (rather than owning one outright) so the same
    /// router can also be handed to the scheduler (M3), which dispatches
    /// task-triggered turns onto the identical session lanes channel
    /// adapters use — one router, two front doors.
    pub fn new(router: Arc<Router>) -> Self {
        Self {
            channels: Vec::new(),
            router,
            interceptors: Vec::new(),
            busy_notice_after: BUSY_NOTICE_AFTER,
            redactor: Arc::new(Redactor::default()),
            health: None,
        }
    }

    /// M19b: `/status` answered here, from any chat, without the model or
    /// the lane; and the status file kept current while `run` runs.
    pub fn with_health(mut self, health: Arc<Health>) -> Self {
        self.health = Some(health);
        self
    }

    /// Adds an interceptor; they're asked in the order added, and the
    /// first to answer takes the message.
    pub fn with_interceptor(mut self, interceptor: Arc<dyn Interceptor>) -> Self {
        self.interceptors.push(interceptor);
        self
    }

    /// How long a turn must have run before a message queued behind it
    /// gets the busy notice.
    pub fn with_busy_notice_after(mut self, after: Duration) -> Self {
        self.busy_notice_after = after;
        self
    }

    /// Hides secrets in what the gateway itself says (the busy notice).
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn add_channel(&mut self, channel: Arc<dyn Channel>) -> &mut Self {
        self.channels.push(channel);
        self
    }

    /// Runs until every channel adapter's inbound loop returns. A channel
    /// that errors is logged and dropped; the others keep serving. Returns
    /// once all adapters have stopped (e.g. every source is exhausted), so a
    /// gateway with only file-backed or stdin-backed adapters terminates
    /// naturally instead of hanging forever.
    pub async fn run(self) -> Result<(), GatewayError> {
        let (tx, mut rx) = mpsc::channel::<InboundMessage>(256);
        let mut handles = Vec::with_capacity(self.channels.len());
        for channel in &self.channels {
            let channel = channel.clone();
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                let name = channel.name().to_string();
                if let Err(e) = channel.run(tx).await {
                    tracing::error!(channel = %name, error = %e, "channel adapter stopped");
                }
            }));
        }
        // Drop our own sender so `rx` closes once every adapter task above
        // has dropped its clone — i.e. once all channels are done.
        drop(tx);

        let background = self.spawn_health();
        while let Some(msg) = rx.recv().await {
            if let Some(h) = &self.health {
                h.dispatching(true);
            }
            self.handle(msg).await;
            if let Some(h) = &self.health {
                h.dispatching(false);
            }
        }
        for h in handles {
            let _ = h.await;
        }
        for task in background {
            task.abort();
        }
        Ok(())
    }

    /// The tasks behind `with_health`: the status file, rewritten every
    /// few seconds and whenever a turn starts or ends.
    fn spawn_health(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let Some(health) = self.health.clone() else {
            return Vec::new();
        };
        let (router, channels) = (self.router.clone(), self.channels.clone());
        let changed = router.changed();
        let every = health.settings().status_every;
        vec![tokio::spawn(async move {
            loop {
                health.write_status(&health.report(&router.snapshot(), &channels));
                let _ = tokio::time::timeout(every, changed.notified()).await;
            }
        })]
    }

    /// One inbound message: `/status`, an interceptor's, or the receipt,
    /// the busy notice if its chat is mid-turn, and its lane.
    async fn handle(&self, msg: InboundMessage) {
        if let Some(health) = &self.health {
            if is_command(&msg.text, "/status") {
                let report = health.report(&self.router.snapshot(), &self.channels);
                self.reply_directly(&msg, report).await;
                return;
            }
        }
        for i in &self.interceptors {
            if let Some(reply) = i.intercept(&msg).await {
                self.reply_directly(&msg, reply).await;
                return;
            }
        }
        self.acknowledge(&msg).await;
        if let Some(lane) = self.router.claim_busy_notice(&msg, self.busy_notice_after) {
            let text = format!(
                "Busy with {} for {}; your message is queued — /stop to cancel it.",
                self.redactor.redact(&lane.activity),
                human(lane.busy_for.unwrap_or_default())
            );
            if let Some(channel) = self.channel(&msg.channel) {
                let out = OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    text,
                    reply_to: Some(msg.message_id.clone()),
                    attachments: vec![],
                };
                tokio::spawn(async move {
                    if let Err(e) = channel.send(out).await {
                        tracing::warn!(error = %e, "couldn't send the busy notice");
                    }
                });
            }
        }
        let reply = msg.clone();
        match self.router.offer(msg) {
            Ok(()) => {}
            Err(GatewayError::QueueFull(_)) => {
                tracing::warn!(chat = %reply.chat_id, "a chat's queue is full; a message was refused");
                self.reply_directly(
                    &reply,
                    "Too many messages are waiting in this chat, so this one was dropped. /stop ends the turn in front; /status shows what it's doing.".into(),
                )
                .await;
            }
            Err(e) => tracing::error!(error = %e, "failed to dispatch inbound message"),
        }
    }

    /// Reacts 👀 to a message bound for a lane, before it's queued, so the
    /// sender knows it arrived even while the chat's turn runs. Channels
    /// without reactions skip it; a failure never holds the message up.
    async fn acknowledge(&self, msg: &InboundMessage) {
        let Some(channel) = self.channel(&msg.channel) else {
            return;
        };
        if !channel.capabilities().reactions || msg.message_id.is_empty() {
            return;
        }
        let (chat, id) = (msg.chat_id.clone(), msg.message_id.clone());
        let react = tokio::spawn(async move {
            if let Err(e) = channel.react(&chat, &id, ACK_EMOJI).await {
                tracing::warn!(error = %e, "couldn't react to a message");
            }
        });
        let _ = tokio::time::timeout(ACK_WAIT, react).await;
    }

    fn channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels.iter().find(|c| c.name() == name).cloned()
    }

    async fn reply_directly(&self, msg: &InboundMessage, text: String) {
        let Some(channel) = self.channel(&msg.channel) else {
            tracing::error!(channel = %msg.channel, "no channel to send an intercepted reply on");
            return;
        };
        let out = OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            text,
            reply_to: Some(msg.message_id.clone()),
            attachments: vec![],
        };
        if let Err(e) = channel.send(out).await {
            tracing::error!(error = %e, "failed to send an intercepted reply");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::OutboundMessage;
    use async_trait::async_trait;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse};
    use ferrule_core::tool::ToolContext;
    use ferrule_core::{
        Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage,
    };
    use std::collections::HashMap;

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
                .find(|m| m.role == ferrule_core::Role::User)
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            Ok(CompletionResponse {
                message: Message::assistant(Some(format!("echo: {last_user}")), vec![], None),
                usage: Usage::default(),
            })
        }
    }

    /// A channel with a fixed, canned inbound script — stands in for a real
    /// adapter (stdin, HTTP polling) so `Gateway::run` can be tested without
    /// any external process or network.
    struct ScriptedChannel {
        script: Vec<InboundMessage>,
        sent: std::sync::Mutex<Vec<OutboundMessage>>,
    }
    #[async_trait]
    impl Channel for ScriptedChannel {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            for m in self.script.clone() {
                tx.send(m)
                    .await
                    .map_err(|_| GatewayError::Channel("closed".into()))?;
            }
            Ok(())
        }
        async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn msg(chat_id: &str, text: &str) -> InboundMessage {
        InboundMessage {
            channel: "scripted".into(),
            chat_id: chat_id.into(),
            sender: "u".into(),
            message_id: "1".into(),
            text: text.into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        }
    }

    #[tokio::test]
    async fn run_terminates_once_all_channels_are_exhausted_and_replies_are_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let scripted = Arc::new(ScriptedChannel {
            script: vec![msg("c1", "hello")],
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("scripted".into(), scripted.clone());

        let factory: crate::router::AgentFactory = Arc::new(|_sid, transcript| {
            Ok(Agent::new(
                Arc::new(EchoProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        });
        let router = Arc::new(Router::new(dir.path(), factory, channels));
        let mut gateway = Gateway::new(router);
        gateway.add_channel(scripted.clone());

        // The channel's `run` finishes after sending one message, which
        // closes the funnel and lets `Gateway::run` return on its own —
        // no manual shutdown signal needed for a source that terminates.
        // Note: `run` returning only means every inbound message has been
        // *dispatched* onto its session lane, not that the lane's spawned
        // task has finished the agent turn and delivered the reply yet —
        // so the assertion below polls with a bounded timeout rather than
        // checking immediately.
        tokio::time::timeout(std::time::Duration::from_secs(2), gateway.run())
            .await
            .unwrap()
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while scripted.sent.lock().unwrap().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "reply was never delivered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(scripted.sent.lock().unwrap().len(), 1);
        assert_eq!(scripted.sent.lock().unwrap()[0].text, "echo: hello");
    }

    struct StopWord;
    #[async_trait]
    impl Interceptor for StopWord {
        async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
            (msg.text == "/stop").then(|| "stopped".to_string())
        }
    }

    #[tokio::test]
    async fn an_intercepted_message_is_answered_without_an_agent_turn() {
        let dir = tempfile::tempdir().unwrap();
        let scripted = Arc::new(ScriptedChannel {
            script: vec![msg("c1", "/stop"), msg("c1", "hello")],
            sent: std::sync::Mutex::new(Vec::new()),
        });
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("scripted".into(), scripted.clone());
        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = turns.clone();
        let factory: crate::router::AgentFactory = Arc::new(move |_sid, transcript| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Agent::new(
                Arc::new(EchoProvider),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        });
        let router = Arc::new(Router::new(dir.path(), factory, channels));
        let mut gateway = Gateway::new(router).with_interceptor(Arc::new(StopWord));
        gateway.add_channel(scripted.clone());
        tokio::time::timeout(std::time::Duration::from_secs(2), gateway.run())
            .await
            .unwrap()
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while scripted.sent.lock().unwrap().len() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "replies never came");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let sent = scripted.sent.lock().unwrap();
        assert_eq!(sent[0].text, "stopped");
        assert_eq!(sent[0].chat_id, "c1");
        assert_eq!(sent[1].text, "echo: hello");
        assert!(sent.iter().all(|m| !m.text.contains("/stop")));
    }

    /// Logs reactions and replies, in order, into a log a provider shares.
    struct ReactingChannel {
        script: Vec<InboundMessage>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Channel for ReactingChannel {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> crate::channel::ChannelCapabilities {
            crate::channel::ChannelCapabilities {
                reactions: true,
                ..Default::default()
            }
        }
        async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            for m in self.script.clone() {
                tx.send(m)
                    .await
                    .map_err(|_| GatewayError::Channel("closed".into()))?;
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            Ok(())
        }
        async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
            self.log.lock().unwrap().push(format!(
                "send to {}: {}",
                msg.reply_to.unwrap_or_default(),
                msg.text
            ));
            Ok(())
        }
        async fn react(
            &self,
            _chat_id: &str,
            message_id: &str,
            emoji: &str,
        ) -> Result<(), GatewayError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("react {emoji} to {message_id}"));
            Ok(())
        }
    }

    /// Logs each turn; the turn for "first" waits until released.
    struct HeldProvider {
        log: Arc<std::sync::Mutex<Vec<String>>>,
        release: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait]
    impl Provider for HeldProvider {
        fn name(&self) -> &str {
            "held"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let text = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == ferrule_core::Role::User)
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            self.log.lock().unwrap().push(format!("turn: {text}"));
            if text == "first" {
                let _ = self.release.acquire().await.unwrap();
            }
            Ok(CompletionResponse {
                message: Message::assistant(Some(format!("echo: {text}")), vec![], None),
                usage: Usage::default(),
            })
        }
    }

    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    /// A reacting channel with `script`, and a router whose turns run on
    /// a `HeldProvider`.
    fn held_gateway(
        dir: &std::path::Path,
        script: Vec<InboundMessage>,
        log: &Log,
        release: &Arc<tokio::sync::Semaphore>,
    ) -> (Arc<ReactingChannel>, Arc<Router>) {
        let channel = Arc::new(ReactingChannel {
            script,
            log: log.clone(),
        });
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("scripted".into(), channel.clone());
        let factory: crate::router::AgentFactory = {
            let (log, release) = (log.clone(), release.clone());
            Arc::new(move |_sid, transcript| {
                Ok(Agent::new(
                    Arc::new(HeldProvider {
                        log: log.clone(),
                        release: release.clone(),
                    }),
                    ToolRegistry::new(),
                    HarnessProfile::generic(),
                    AgentConfig::default(),
                    ToolContext::default(),
                    Some(transcript),
                )
                .with_system_prompt("test"))
            })
        };
        (channel, Arc::new(Router::new(dir, factory, channels)))
    }

    async fn wait_for_log(log: &Log, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while log.lock().unwrap().len() < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{:?}",
                log.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn numbered(texts: &[&str]) -> Vec<InboundMessage> {
        texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let mut m = msg("c1", text);
                m.message_id = (i + 1).to_string();
                m
            })
            .collect()
    }

    #[tokio::test]
    async fn status_is_answered_while_the_chats_turn_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let log: Log = Arc::default();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let script = numbered(&["first", "/status", "/status@ferrule_bot"]);
        let (channel, router) = held_gateway(dir.path(), script, &log, &release);
        let status_dir = dir.path().join("gateway");
        let health = Arc::new(
            Health::new(
                "9.9.9",
                crate::health::HealthSettings {
                    dir: Some(status_dir.clone()),
                    ..Default::default()
                },
            )
            .with_recent(Arc::new(crate::health::RecentLog::new(5)))
            .with_redactor(Arc::new(Redactor::new(["sk-abcdef123".to_string()])))
            .with_section(
                "spend",
                Arc::new(|| vec!["today: 12 tokens, sk-abcdef123".into()]),
            ),
        );
        let mut gateway = Gateway::new(router).with_health(health.clone());
        gateway.add_channel(channel);
        tokio::time::timeout(Duration::from_secs(2), gateway.run())
            .await
            .unwrap()
            .unwrap();
        wait_for_log(&log, 4).await;
        let log_now = log.lock().unwrap().clone();
        // No receipt for /status: its answer is the receipt.
        assert_eq!(log_now[..2], ["react 👀 to 1", "turn: first"]);
        for (i, id) in ["2", "3"].iter().enumerate() {
            let report = &log_now[2 + i];
            assert!(
                report.starts_with(&format!("send to {id}: ferrule 9.9.9 — up ")),
                "{report}"
            );
            assert!(
                report.contains("scripted chat c1: a model call for 0 s, last progress 0 s ago"),
                "{report}"
            );
            assert!(
                report.contains("spend:\n  today: 12 tokens, [redacted]"),
                "{report}"
            );
            assert!(report.contains("scripted: doesn't poll"), "{report}");
            assert!(
                report.contains("recent warnings and errors:\n  none"),
                "{report}"
            );
        }
        // The status file was written; a clean shutdown removes it.
        let file = status_dir.join(crate::health::STATUS_FILE);
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .starts_with("ferrule 9.9.9"));
        health.shutdown();
        assert!(!file.exists());
        release.add_permits(1);
        wait_for_log(&log, 5).await;
        assert_eq!(log.lock().unwrap()[4], "send to 1: echo: first");
    }

    #[tokio::test]
    async fn every_message_is_seen_at_once_and_a_queued_one_is_told_once() {
        let dir = tempfile::tempdir().unwrap();
        let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let script = numbered(&["first", "second", "third"]);
        let (channel, router) = held_gateway(dir.path(), script, &log, &release);
        let mut gateway = Gateway::new(router).with_busy_notice_after(Duration::ZERO);
        gateway.add_channel(channel);
        tokio::time::timeout(Duration::from_secs(2), gateway.run())
            .await
            .unwrap()
            .unwrap();
        // All three are acknowledged and queued while "first" is held.
        wait_for_log(&log, 5).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "react 👀 to 1",
                "turn: first",
                "react 👀 to 2",
                "send to 2: Busy with a model call for 0 s; your message is queued — /stop to cancel it.",
                "react 👀 to 3",
            ]
        );
        release.add_permits(1);
        wait_for_log(&log, 8).await;
        let log = log.lock().unwrap();
        assert_eq!(
            log[5..8],
            [
                "send to 1: echo: first",
                "turn: second",
                "send to 2: echo: second"
            ]
        );
    }
}
