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
        let mut tasks = Vec::new();
        {
            let (health, router, channels) = (health.clone(), router.clone(), channels.clone());
            let changed = router.changed();
            let every = health.settings().status_every;
            tasks.push(tokio::spawn(async move {
                loop {
                    let lanes = router.snapshot();
                    health.write_running(&lanes);
                    health.write_status(&health.report(&lanes, &channels));
                    let _ = tokio::time::timeout(every, changed.notified()).await;
                }
            }));
        }
        if let Some(notice) = health.take_startup_notice() {
            let target = health.settings().owner.clone().or(notice.fallback);
            let channel = target
                .as_ref()
                .and_then(|(name, _)| channels.iter().find(|c| c.name() == name).cloned());
            if let (Some((name, chat)), Some(channel)) = (target, channel) {
                let out = OutboundMessage {
                    channel: name,
                    chat_id: chat,
                    text: health.redactor().redact(&notice.text),
                    reply_to: None,
                    attachments: vec![],
                };
                tasks.push(tokio::spawn(send_with_retries(channel, out)));
            }
        }
        if let Some(sd) = health.systemd().cloned() {
            let (health, channels) = (health.clone(), channels.clone());
            tasks.push(tokio::spawn(async move {
                ping_systemd(sd, health, channels).await
            }));
        }
        if let Some(beat) = health.settings().heartbeat.clone() {
            let (health, router, channels) = (health.clone(), router.clone(), channels.clone());
            tasks.push(tokio::spawn(heartbeat(health, router, channels, beat)));
        }
        if let Some(after) = health.settings().watchdog_after {
            tasks.push(tokio::spawn(watchdog(health, router, channels, after)));
        }
        tasks
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
            let text = busy_text(
                &self.redactor.redact(&lane.activity),
                lane.busy_for.unwrap_or_default(),
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

/// The startup notice: the network may not be up yet after a crash, so a
/// failed send is tried again, for about two minutes.
async fn send_with_retries(channel: Arc<dyn Channel>, out: OutboundMessage) {
    let mut wait = Duration::from_secs(1);
    for attempt in 1..=8 {
        match channel.send(out.clone()).await {
            Ok(()) => return,
            Err(e) => tracing::warn!(attempt, error = %e, "couldn't send the startup notice"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// `WATCHDOG=1` every third of systemd's timeout, withheld while the
/// gateway can't hear the owner so that systemd restarts it.
async fn ping_systemd(
    sd: crate::sdnotify::SystemdWatchdog,
    health: Arc<Health>,
    channels: Vec<Arc<dyn Channel>>,
) {
    let mut withheld = false;
    loop {
        match health.watchdog_ok(&channels) {
            Ok(()) => {
                if withheld {
                    tracing::info!("pinging systemd's watchdog again");
                    withheld = false;
                }
                if let Err(e) = sd.notify("WATCHDOG=1") {
                    tracing::warn!(error = %e, "couldn't ping systemd's watchdog");
                }
            }
            Err(why) => {
                if !withheld {
                    tracing::warn!(
                        "{why}: not pinging systemd's watchdog, so it restarts the service"
                    );
                    withheld = true;
                }
            }
        }
        tokio::time::sleep(sd.every).await;
    }
}

/// POSTs [`Health::heartbeat`] to `beat.url` now and every `beat.every`.
/// A failure is a warning (once, until a ping gets through again), never
/// fatal; the URL itself isn't logged, in case it holds a token.
async fn heartbeat(
    health: Arc<Health>,
    router: Arc<Router>,
    channels: Vec<Arc<dyn Channel>>,
    beat: crate::health::Heartbeat,
) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "no heartbeat: couldn't build an HTTP client");
            return;
        }
    };
    let mut failing = false;
    loop {
        let body = health.heartbeat(&router.snapshot(), &channels);
        let sent = client
            .post(&beat.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        let result = match sent {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(format!("HTTP {}", r.status())),
            Err(e) => Err(e.without_url().to_string()),
        };
        match result {
            Ok(()) if failing => {
                tracing::info!("the heartbeat gets through again");
                failing = false;
            }
            Ok(()) => {}
            Err(e) if !failing => {
                tracing::warn!(error = %e, "the heartbeat failed; retrying every interval");
                failing = true;
            }
            Err(_) => {}
        }
        tokio::time::sleep(beat.every).await;
    }
}

/// M19b's watchdog: a turn that has made no progress for `after` gets
/// one message to the owner (or its own chat), once per stall — progress
/// re-arms it. The turn itself is left alone; `/stop` or
/// `max_turn_minutes` ends it.
async fn watchdog(
    health: Arc<Health>,
    router: Arc<Router>,
    channels: Vec<Arc<dyn Channel>>,
    after: Duration,
) {
    let tick = (after / 4).clamp(Duration::from_millis(20), Duration::from_secs(15));
    loop {
        tokio::time::sleep(tick).await;
        for lane in router.claim_stalled(after) {
            tracing::warn!(chat = %lane.place(), activity = %lane.activity, "a turn has made no progress");
            let (to_channel, to_chat) = health
                .settings()
                .owner
                .clone()
                .unwrap_or((lane.channel.clone(), lane.chat_id.clone()));
            let Some(channel) = channels.iter().find(|c| c.name() == to_channel).cloned() else {
                continue;
            };
            let out = OutboundMessage {
                channel: to_channel,
                chat_id: to_chat,
                text: health.stall_notice(&lane),
                reply_to: None,
                attachments: vec![],
            };
            tokio::spawn(async move {
                if let Err(e) = channel.send(out).await {
                    tracing::warn!(error = %e, "couldn't send the watchdog's message");
                }
            });
        }
    }
}

/// The notice a message queued behind a busy turn gets.
fn busy_text(activity: &str, busy_for: Duration) -> String {
    let busy_for = human(busy_for);
    if activity.starts_with("waiting") {
        // The model is being retried (M19c): say what's waited for.
        format!("Busy for {busy_for}, {activity}; your message is queued — /stop to cancel it.")
    } else {
        format!("Busy with {activity} for {busy_for}; your message is queued — /stop to cancel it.")
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
            sender_id: None,
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
        /// How long the channel stays open after its script.
        linger: Duration,
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
            tokio::time::sleep(self.linger).await;
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
        held_gateway_open(dir, script, log, release, Duration::ZERO)
    }

    fn held_gateway_open(
        dir: &std::path::Path,
        script: Vec<InboundMessage>,
        log: &Log,
        release: &Arc<tokio::sync::Semaphore>,
        linger: Duration,
    ) -> (Arc<ReactingChannel>, Arc<Router>) {
        let channel = Arc::new(ReactingChannel {
            script,
            log: log.clone(),
            linger,
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

    #[test]
    fn a_queued_message_hears_about_a_rate_limit_wait() {
        assert_eq!(
            busy_text(
                "waiting out the model's rate limit, retry 2 in 25 s",
                Duration::from_secs(40)
            ),
            "Busy for 40 s, waiting out the model's rate limit, retry 2 in 25 s; your message is queued — /stop to cancel it."
        );
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

    #[tokio::test]
    async fn the_watchdog_tells_the_owner_once_per_stall() {
        let dir = tempfile::tempdir().unwrap();
        let log: Log = Arc::default();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let script = numbered(&["first"]);
        let (channel, router) = held_gateway_open(
            dir.path(),
            script,
            &log,
            &release,
            Duration::from_millis(700),
        );
        let health = Arc::new(Health::new(
            "9.9.9",
            crate::health::HealthSettings {
                watchdog_after: Some(Duration::from_millis(100)),
                owner: Some(("scripted".into(), "owner".into())),
                ..Default::default()
            },
        ));
        let mut gateway = Gateway::new(router).with_health(health);
        gateway.add_channel(channel);
        tokio::time::timeout(Duration::from_secs(2), gateway.run())
            .await
            .unwrap()
            .unwrap();
        let stalls: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("Stuck on"))
            .cloned()
            .collect();
        // Held for ~700 ms against a 100 ms watchdog: one message, not six.
        assert_eq!(
            stalls,
            ["send to : Stuck on a model call for 0 s in scripted chat c1, handling: 'first' — /stop to cancel it."]
        );
        release.add_permits(1);
    }

    #[tokio::test]
    async fn no_watchdog_when_it_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let log: Log = Arc::default();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (channel, router) = held_gateway_open(
            dir.path(),
            numbered(&["first"]),
            &log,
            &release,
            Duration::from_millis(300),
        );
        let health = Arc::new(Health::new(
            "9.9.9",
            crate::health::HealthSettings {
                watchdog_after: None,
                ..Default::default()
            },
        ));
        let mut gateway = Gateway::new(router).with_health(health);
        gateway.add_channel(channel);
        gateway.run().await.unwrap();
        assert!(log.lock().unwrap().iter().all(|l| !l.contains("Stuck on")));
        release.add_permits(1);
    }

    /// Polls "successfully" while `fresh`; runs until `stop`.
    struct PollingChannel {
        last_ok: std::sync::Mutex<std::time::SystemTime>,
        fresh: std::sync::atomic::AtomicBool,
        stop: tokio::sync::Notify,
    }
    #[async_trait]
    impl Channel for PollingChannel {
        fn name(&self) -> &str {
            "polling"
        }
        fn polls(&self) -> bool {
            true
        }
        fn last_ok_poll(&self) -> Option<std::time::SystemTime> {
            let mut last = self.last_ok.lock().unwrap();
            if self.fresh.load(std::sync::atomic::Ordering::SeqCst) {
                *last = std::time::SystemTime::now();
            }
            Some(*last)
        }
        async fn run(&self, _tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            self.stop.notified().await;
            Ok(())
        }
        async fn send(&self, _msg: OutboundMessage) -> Result<(), GatewayError> {
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_is_pinged_only_while_polling_works() {
        use std::os::unix::net::UnixDatagram;
        use std::sync::atomic::Ordering::SeqCst;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("notify");
        let rx = UnixDatagram::bind(&socket).unwrap();
        rx.set_nonblocking(true).unwrap();
        let drain = || {
            let mut buf = [0u8; 64];
            let mut pings = 0;
            while let Ok(n) = rx.recv(&mut buf) {
                assert_eq!(&buf[..n], b"WATCHDOG=1");
                pings += 1;
            }
            pings
        };
        let channel = Arc::new(PollingChannel {
            last_ok: std::sync::Mutex::new(std::time::SystemTime::now()),
            fresh: true.into(),
            stop: tokio::sync::Notify::new(),
        });
        let health = Arc::new(
            Health::new(
                "9.9.9",
                crate::health::HealthSettings {
                    poll_stale: Duration::from_millis(200),
                    watchdog_after: None,
                    ..Default::default()
                },
            )
            .with_systemd(Some(crate::sdnotify::SystemdWatchdog {
                socket: socket.to_string_lossy().into(),
                every: Duration::from_millis(20),
            })),
        );
        let router = Arc::new(Router::new(
            dir.path(),
            Arc::new(|_, _| unreachable!()),
            HashMap::new(),
        ));
        let mut gateway = Gateway::new(router).with_health(health);
        gateway.add_channel(channel.clone());
        let run = tokio::spawn(gateway.run());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(drain() >= 3, "no pings while polling works");

        // Polling stops succeeding: after poll_stale the pings stop.
        channel.fresh.store(false, SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(drain(), 0, "pinged with polling stale");

        // A good poll again: so are the pings.
        channel.fresh.store(true, SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(drain() >= 3, "pings didn't come back");

        channel.stop.notify_one();
        run.await.unwrap().unwrap();
    }
}
