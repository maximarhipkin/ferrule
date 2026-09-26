//! The owner's trust state for one process: the caps and what's been spent
//! against them, the kill switch, the approvals waiting for an answer, and
//! the audit log. Every agent's guard in the process shares one hub.

use crate::approval::{Answer, Approvals};
use crate::audit::Audit;
use crate::chat::ChatRef;
use crate::clock::Clock;
use crate::config::TrustConfig;
use crate::kill::{stop_message, KillSwitch, StopInfo};
use crate::meter::{Meter, Spend};
use async_trait::async_trait;
use chrono_tz::Tz;
use ferrule_core::LedgerRecord;
use ferrule_proxy::HostPattern;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::watch;

/// Sends the owner a message (through the gateway's channels).
#[async_trait]
pub trait Notifier: Send + Sync {
    /// A Telegram chat: the notifier before M31.
    async fn send(&self, chat: i64, text: &str) -> Result<(), String>;

    /// Any owner chat (M31). By default only a Telegram one, through `send`.
    async fn send_to(&self, chat: &ChatRef, text: &str) -> Result<(), String> {
        match chat.telegram_id() {
            Some(id) => self.send(id, text).await,
            None => Err(format!("no way to reach a {} chat", chat.channel)),
        }
    }

    /// A question with answer buttons, `(label, the reply it sends)`, where
    /// the channel has them. By default the text alone, which already says
    /// what to reply.
    async fn send_choices(
        &self,
        chat: &ChatRef,
        text: &str,
        _choices: &[(String, String)],
    ) -> Result<(), String> {
        self.send_to(chat, text).await
    }
}

/// Asks a question at a terminal and reads one line; `None` at end of input.
pub trait Prompter: Send + Sync {
    fn ask(&self, text: &str) -> Option<String>;
}

/// What the gateway does with a message the hub read first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intercept {
    /// Not for the hub: goes to the chat's session as usual.
    Pass,
    /// Handled: send this back, and nothing else.
    Reply(String),
    /// `/plan <task>`: run it in plan mode.
    Plan(String),
}

/// Which cap, for messages and window keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Run,
    Day,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Tokens,
    Usd,
}

struct Over {
    key: &'static str,
    window: Window,
    unit: Unit,
    cap: f64,
    spent: f64,
}

#[derive(Clone)]
struct RunState {
    id: String,
    spend: Spend,
}

pub struct Hub {
    /// The caps can change while the process runs (M24's page and CLI).
    cfg: RwLock<TrustConfig>,
    tz: Tz,
    clock: Arc<dyn Clock>,
    meter: Meter,
    audit: Audit,
    stop: KillSwitch,
    notifier: RwLock<Option<Arc<dyn Notifier>>>,
    /// The owner's chats, at most one per channel; the first is primary:
    /// it gets the questions and the warnings (M31).
    owners: RwLock<Vec<ChatRef>>,
    approvals: Approvals,
    runs: Mutex<HashMap<String, RunState>>,
    warned: Mutex<HashSet<String>>,
    changed: watch::Sender<u64>,
    poll: Duration,
    bound: Vec<HostPattern>,
    /// M20: the connected services, whose changing tools are gated.
    connected: RwLock<Vec<String>>,
}

impl Hub {
    /// A hub over `<data>`: the ledger at `ledger`, the switch and the
    /// audit log under `<data>/trust/`.
    pub fn new(
        cfg: TrustConfig,
        data: &Path,
        ledger: &Path,
        clock: Arc<dyn Clock>,
        bound: Vec<HostPattern>,
    ) -> Result<Self, String> {
        cfg.validate()?;
        let tz = cfg.tz()?;
        let hub = Self {
            meter: Meter::new(ledger.to_path_buf(), tz),
            audit: Audit::new(data.join("trust").join("audit.jsonl")),
            stop: KillSwitch::new(data.join("trust").join("stop")),
            owners: RwLock::new(crate::config::order_owners(
                cfg.owner_chat
                    .map(ChatRef::from)
                    .into_iter()
                    .chain(
                        cfg.discord_owner
                            .clone()
                            .map(|u| ChatRef::new("discord", u)),
                    )
                    .chain(cfg.slack_owner.clone().map(|u| ChatRef::new("slack", u)))
                    .collect(),
                cfg.owner_channel.as_deref(),
            )),
            cfg: RwLock::new(cfg),
            tz,
            clock,
            notifier: RwLock::new(None),
            approvals: Approvals::default(),
            runs: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
            changed: watch::channel(0).0,
            poll: Duration::from_secs(1),
            bound,
            connected: RwLock::new(Vec::new()),
        };
        // A restart doesn't warn twice for today's windows.
        let today = hub.meter.day(hub.clock.now()).to_string();
        if let Ok(events) = hub.audit.read(None) {
            let mut warned = hub.warned.lock().unwrap();
            for e in events.iter().filter(|e| e.event == "cap_warning") {
                if let Some(key) = e.detail.get("window").and_then(|k| k.as_str()) {
                    if key.contains(&today) {
                        warned.insert(key.to_string());
                    }
                }
            }
        }
        Ok(hub)
    }

    /// How often the kill switch file is looked at (tests go faster).
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// The settings as they are now.
    pub fn config(&self) -> TrustConfig {
        self.cfg.read().unwrap().clone()
    }

    /// New caps, live from the next check (M24). Only the six caps and
    /// `warn_at` change; the zone, the owner and the gates keep what the
    /// process started with. Refused, changing nothing, when they don't
    /// validate. `true` when a value changed.
    pub fn set_caps(&self, new: &TrustConfig) -> Result<bool, String> {
        let mut cfg = self.cfg.write().unwrap();
        let next = TrustConfig {
            max_tokens_per_run: new.max_tokens_per_run,
            max_usd_per_run: new.max_usd_per_run,
            max_tokens_per_day: new.max_tokens_per_day,
            max_usd_per_day: new.max_usd_per_day,
            max_tokens_per_task: new.max_tokens_per_task,
            max_usd_per_task: new.max_usd_per_task,
            warn_at: new.warn_at,
            ..cfg.clone()
        };
        next.validate()?;
        let changed = *cfg != next;
        *cfg = next;
        Ok(changed)
    }

    pub fn bound_hosts(&self) -> &[HostPattern] {
        &self.bound
    }

    /// M20: the services connected now (their MCP server names).
    pub fn set_connected(&self, names: Vec<String>) {
        *self.connected.write().unwrap() = names;
    }

    pub fn connected(&self) -> Vec<String> {
        self.connected.read().unwrap().clone()
    }

    pub fn audit(&self) -> &Audit {
        &self.audit
    }

    pub fn kill_switch(&self) -> &KillSwitch {
        &self.stop
    }

    pub fn approvals(&self) -> &Approvals {
        &self.approvals
    }

    pub fn set_notifier(&self, n: Option<Arc<dyn Notifier>>) {
        *self.notifier.write().unwrap() = n;
    }

    /// The Telegram owner chat: set, replaced where it stood, or removed.
    /// A new one comes first, as it did when it was the only owner.
    pub fn set_owner(&self, chat: Option<i64>) {
        let mut owners = self.owners.write().unwrap();
        let at = owners.iter().position(|o| o.channel == "telegram");
        match (chat.map(ChatRef::from), at) {
            (Some(c), Some(i)) => owners[i] = c,
            (Some(c), None) => owners.insert(0, c),
            (None, Some(i)) => {
                owners.remove(i);
            }
            (None, None) => {}
        }
    }

    /// The Telegram owner chat, if there is one.
    pub fn owner(&self) -> Option<i64> {
        self.owners
            .read()
            .unwrap()
            .iter()
            .find_map(ChatRef::telegram_id)
    }

    /// Every owner chat (M31), the primary first; one per channel is kept.
    pub fn set_owners(&self, chats: Vec<ChatRef>) {
        let mut seen = HashSet::new();
        *self.owners.write().unwrap() = chats
            .into_iter()
            .filter(|c| seen.insert(c.channel.clone()))
            .collect();
    }

    pub fn owners(&self) -> Vec<ChatRef> {
        self.owners.read().unwrap().clone()
    }

    /// The chat that gets approvals and warnings.
    pub fn primary(&self) -> Option<ChatRef> {
        self.owners.read().unwrap().first().cloned()
    }

    /// Whether `chat` is one of the owner's chats.
    pub fn is_owner(&self, chat: &ChatRef) -> bool {
        self.owners.read().unwrap().contains(chat)
    }

    /// The owner's chat on `channel`.
    pub fn owner_on(&self, channel: &str) -> Option<ChatRef> {
        self.owners
            .read()
            .unwrap()
            .iter()
            .find(|o| o.channel == channel)
            .cloned()
    }

    fn notifier(&self) -> Option<Arc<dyn Notifier>> {
        self.notifier.read().unwrap().clone()
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// A root agent's run starts: its spend starts from nothing.
    pub fn begin_run(&self, tree: &str) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        self.runs.lock().unwrap().insert(
            tree.to_string(),
            RunState {
                id: id.clone(),
                spend: Spend::default(),
            },
        );
        id
    }

    pub fn run_id(&self, tree: &str) -> Option<String> {
        self.runs.lock().unwrap().get(tree).map(|r| r.id.clone())
    }

    /// One provider call's row, as it goes to the ledger.
    pub fn charge(&self, tree: &str, r: &LedgerRecord) {
        {
            let mut runs = self.runs.lock().unwrap();
            let run = runs.entry(tree.to_string()).or_insert_with(|| RunState {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                spend: Spend::default(),
            });
            run.spend.add(Spend::of(r));
        }
        self.bump();
    }

    pub fn run_spend(&self, tree: &str) -> Spend {
        self.runs
            .lock()
            .unwrap()
            .get(tree)
            .map(|r| r.spend)
            .unwrap_or_default()
    }

    /// Today's spend and `task`'s, from the ledger.
    pub fn today(&self, task: Option<&str>) -> Result<(Spend, Spend), String> {
        self.meter.read(self.clock.now(), task)
    }

    /// Asked before every model call. `Some(message)` ends the run.
    /// `notify`: also send the stop to the owner (an unattended run).
    pub fn check(&self, tree: &str, task: Option<&str>, notify: bool) -> Option<String> {
        if let Some(info) = self.stop.status() {
            return Some(stop_message(&info));
        }
        let run = self.run_spend(tree);
        let run_id = self.run_id(tree);
        let today = match self.today(task) {
            Ok(t) => Some(t),
            Err(e) if self.config().needs_ledger() => {
                let msg = format!(
                    "Stopped: {e}, so the day caps can't be checked. Nothing else was sent to the model."
                );
                self.audit.record(
                    self.clock.now(),
                    "cap_stop",
                    Some(tree),
                    run_id.as_deref(),
                    json!({"cap": "ledger", "error": e}),
                );
                return Some(msg);
            }
            Err(e) => {
                tracing::warn!("trust: {e}");
                None
            }
        };
        let (overs, near) = self.measure(run, today, task);
        if let Some(o) = overs.first() {
            let msg = self.stop_text(o, run, today, task);
            self.audit.record(
                self.clock.now(),
                "cap_stop",
                Some(tree),
                run_id.as_deref(),
                json!({"cap": o.key, "limit": o.cap, "spent": o.spent, "message": msg}),
            );
            if notify {
                self.tell_owner(msg.clone());
            }
            return Some(msg);
        }
        for o in near {
            let window = self.window_key(&o, run_id.as_deref(), task);
            if !self.warned.lock().unwrap().insert(window.clone()) {
                continue;
            }
            let msg = self.warn_text(&o, task);
            self.audit.record(
                self.clock.now(),
                "cap_warning",
                Some(tree),
                run_id.as_deref(),
                json!({"cap": o.key, "limit": o.cap, "spent": o.spent, "window": window, "message": msg}),
            );
            tracing::warn!("{msg}");
            self.tell_owner(msg);
        }
        None
    }

    /// Resolves once `tree` must stop now: the kill switch, or a cap that
    /// wasn't crossed yet when the wait began (a call already over a cap
    /// goes on; the next check stops the run).
    pub async fn halted(&self, tree: &str, task: Option<&str>) -> String {
        let mut changed = self.changed.subscribe();
        let over_at_start = self.over(tree, task).is_some();
        loop {
            if let Some(info) = self.stop.status() {
                return stop_message(&info);
            }
            if !over_at_start {
                if let Some(msg) = self.over(tree, task) {
                    return msg;
                }
            }
            tokio::select! {
                r = changed.changed() => {
                    if r.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                _ = tokio::time::sleep(self.poll) => {}
            }
        }
    }

    /// The stop text of the first cap `tree` is at or over, without side
    /// effects. A ledger that can't be read counts as nothing here.
    fn over(&self, tree: &str, task: Option<&str>) -> Option<String> {
        let run = self.run_spend(tree);
        let today = self.today(task).ok();
        let (overs, _) = self.measure(run, today, task);
        overs.first().map(|o| self.stop_text(o, run, today, task))
    }

    fn measure(
        &self,
        run: Spend,
        today: Option<(Spend, Spend)>,
        task: Option<&str>,
    ) -> (Vec<Over>, Vec<Over>) {
        let c = &self.config();
        let mut caps = vec![
            (
                "max_tokens_per_run",
                Window::Run,
                Unit::Tokens,
                c.max_tokens_per_run as f64,
                run.tokens as f64,
            ),
            (
                "max_usd_per_run",
                Window::Run,
                Unit::Usd,
                c.max_usd_per_run,
                run.usd,
            ),
        ];
        if let Some((day, t)) = today {
            caps.push((
                "max_tokens_per_day",
                Window::Day,
                Unit::Tokens,
                c.max_tokens_per_day as f64,
                day.tokens as f64,
            ));
            caps.push((
                "max_usd_per_day",
                Window::Day,
                Unit::Usd,
                c.max_usd_per_day,
                day.usd,
            ));
            if task.is_some() {
                caps.push((
                    "max_tokens_per_task",
                    Window::Task,
                    Unit::Tokens,
                    c.max_tokens_per_task as f64,
                    t.tokens as f64,
                ));
                caps.push((
                    "max_usd_per_task",
                    Window::Task,
                    Unit::Usd,
                    c.max_usd_per_task,
                    t.usd,
                ));
            }
        }
        let mut overs = Vec::new();
        let mut near = Vec::new();
        for (key, window, unit, cap, spent) in caps {
            if cap <= 0.0 {
                continue;
            }
            let o = Over {
                key,
                window,
                unit,
                cap,
                spent,
            };
            if spent >= cap {
                overs.push(o);
            } else if spent >= cap * c.warn_at {
                near.push(o);
            }
        }
        (overs, near)
    }

    fn window_key(&self, o: &Over, run: Option<&str>, task: Option<&str>) -> String {
        let day = self.meter.day(self.clock.now());
        match o.window {
            Window::Run => format!("run:{}:{}", run.unwrap_or("-"), o.key),
            Window::Day => format!("day:{day}:{}", o.key),
            Window::Task => format!("task:{}:{day}:{}", task.unwrap_or("-"), o.key),
        }
    }

    fn stop_text(
        &self,
        o: &Over,
        run: Spend,
        today: Option<(Spend, Spend)>,
        task: Option<&str>,
    ) -> String {
        let what = match o.window {
            Window::Run => "this run".to_string(),
            Window::Day => "today's spend".to_string(),
            Window::Task => format!("scheduled task `{}` today", task.unwrap_or("?")),
        };
        let unit = match o.unit {
            Unit::Tokens => "token",
            Unit::Usd => "dollar",
        };
        let limit = amount(o.unit, o.cap);
        let mut spent = format!(
            "{} tokens and ${:.2} this run",
            thousands(run.tokens),
            run.usd
        );
        if let Some((day, t)) = today {
            spent.push_str(&format!(
                "; {} tokens and ${:.2} today ({})",
                thousands(day.tokens),
                day.usd,
                self.tz
            ));
            if let (Some(task), Window::Task) = (task, o.window) {
                spent.push_str(&format!(
                    "; {} tokens and ${:.2} for `{task}` today",
                    thousands(t.tokens),
                    t.usd
                ));
            }
        }
        format!(
            "Stopped: {what} reached its {unit} cap ({} = {limit}). Spent: {spent}. \
             Nothing else was sent to the model. Raise the cap in [trust], or start again tomorrow for a day cap.",
            o.key
        )
    }

    fn warn_text(&self, o: &Over, task: Option<&str>) -> String {
        let share = (self.config().warn_at * 100.0).round();
        let unit = match o.unit {
            Unit::Tokens => "token",
            Unit::Usd => "dollar",
        };
        let whose = match o.window {
            Window::Run => "this run's".to_string(),
            Window::Day => format!("today's ({} day)", self.tz),
            Window::Task => format!("scheduled task `{}`'s daily", task.unwrap_or("?")),
        };
        format!(
            "ferrule: {share}% of {whose} {unit} cap is spent ({} of {}). Runs stop at {}.",
            amount(o.unit, o.spent),
            amount(o.unit, o.cap),
            amount(o.unit, o.cap)
        )
    }

    /// Sends the owner a message and doesn't wait; a failure is logged.
    pub fn tell_owner(&self, text: String) {
        let (Some(n), Some(chat)) = (self.notifier(), self.primary()) else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.spawn(async move {
            if let Err(e) = n.send_to(&chat, &text).await {
                tracing::warn!("trust: couldn't tell the owner ({e})");
            }
        });
    }

    /// The kill switch on. `by`: who (`ferrule stop`, `telegram chat 42`,
    /// `discord chat 1234`).
    pub fn engage(&self, by: &str, reason: Option<String>) -> std::io::Result<StopInfo> {
        let info = StopInfo {
            at: self.clock.now().to_rfc3339(),
            by: by.into(),
            reason,
        };
        self.stop.engage(&info)?;
        self.audit.record(
            self.clock.now(),
            "stop_engaged",
            None,
            None,
            json!({"by": info.by, "reason": info.reason}),
        );
        self.bump();
        Ok(info)
    }

    /// The kill switch off; `true` when it was on.
    pub fn clear(&self, by: &str) -> std::io::Result<bool> {
        let was = self.stop.clear()?;
        if was {
            self.audit.record(
                self.clock.now(),
                "stop_cleared",
                None,
                None,
                json!({"by": by}),
            );
        }
        self.bump();
        Ok(was)
    }

    pub fn stopped(&self) -> Option<StopInfo> {
        self.stop.status()
    }

    /// Asks the owner chat `question` and waits up to `timeout`. `Ok` only
    /// on a yes; `Err` says why not. Dropping the future withdraws it.
    pub async fn ask_owner(
        &self,
        tree: &str,
        subject: &str,
        question: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let run = self.run_id(tree);
        let Some(chat) = self.primary() else {
            self.answered(
                tree,
                &run,
                json!({"subject": subject, "route": "telegram", "answer": "unreachable"}),
            );
            return Err("no owner chat is set to ask ([trust] owner_chat, or a private chat in [gateway] telegram_allowed_chats)".into());
        };
        let route = chat.channel.clone();
        let Some(notifier) = self.notifier() else {
            self.answered(
                tree,
                &run,
                json!({"subject": subject, "route": route, "answer": "unreachable"}),
            );
            return Err(format!(
                "couldn't reach the owner: {} isn't running in this process",
                chat.channel_title()
            ));
        };
        let (code, rx) = self.approvals.open(&chat, subject);
        let mut waiting = Waiting {
            hub: self,
            code: code.clone(),
            route: route.clone(),
            tree,
            run: run.clone(),
            subject,
            done: false,
        };
        let text = format!("{question}\nReply `yes` to allow it. Anything else, or no answer in {}, refuses it. (code {code})", minutes(timeout));
        self.audit.record(
            self.clock.now(),
            "approval_asked",
            Some(tree),
            run.as_deref(),
            json!({"subject": subject, "route": route, "chat": chat.audit_value(), "code": code}),
        );
        let choices = [
            ("Allow".to_string(), format!("yes {code}")),
            ("Refuse".to_string(), format!("no {code}")),
        ];
        if let Err(e) = notifier.send_choices(&chat, &text, &choices).await {
            waiting.finish("unreachable");
            return Err(format!("couldn't reach the owner ({e})"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Answer::Yes)) => {
                waiting.finish("yes");
                Ok(())
            }
            Ok(Ok(Answer::No(said))) => {
                waiting.finish("no");
                Err(format!("the owner refused it (\"{said}\")"))
            }
            Ok(Err(_)) => {
                waiting.finish("withdrawn");
                Err("the question was withdrawn".into())
            }
            Err(_) => {
                waiting.finish("timeout");
                Err(format!("no answer in {}", minutes(timeout)))
            }
        }
    }

    /// Asks at the terminal: one line, `yes` or nothing.
    pub async fn ask_terminal(
        &self,
        tree: &str,
        subject: &str,
        question: &str,
        prompter: Arc<dyn Prompter>,
    ) -> Result<(), String> {
        let run = self.run_id(tree);
        self.audit.record(
            self.clock.now(),
            "approval_asked",
            Some(tree),
            run.as_deref(),
            json!({"subject": subject, "route": "terminal"}),
        );
        let text = format!("{question}\nType yes to allow it; anything else refuses it: ");
        let line = tokio::task::spawn_blocking(move || prompter.ask(&text))
            .await
            .ok()
            .flatten();
        let yes = line
            .as_deref()
            .map(|l| {
                l.trim()
                    .trim_end_matches(['.', '!'])
                    .eq_ignore_ascii_case("yes")
            })
            .unwrap_or(false);
        self.answered(
            tree,
            &run,
            json!({"subject": subject, "route": "terminal", "answer": if yes { "yes" } else { "no" }}),
        );
        if yes {
            Ok(())
        } else {
            Err("refused at the terminal".into())
        }
    }

    /// Records a refusal that asked nobody (an unattended run).
    pub fn refuse_unattended(&self, tree: &str, subject: &str, why: &str) {
        let run = self.run_id(tree);
        self.answered(
            tree,
            &run,
            json!({"subject": subject, "route": "none", "answer": "unattended", "why": why}),
        );
    }

    fn answered(&self, tree: &str, run: &Option<String>, detail: serde_json::Value) {
        self.audit.record(
            self.clock.now(),
            "approval_answered",
            Some(tree),
            run.as_deref(),
            detail,
        );
    }

    /// Reads a message from an allowed chat before it reaches a session:
    /// `/stop`, `/resume`, `/plan`, and replies to pending approvals.
    pub fn intercept(&self, chat: impl Into<ChatRef>, text: &str) -> Intercept {
        let chat = chat.into();
        let t = text.trim();
        let (cmd, rest) = match t.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (t, ""),
        };
        let cmd = cmd.split('@').next().unwrap_or(cmd).to_lowercase();
        match cmd.as_str() {
            "/stop" => {
                let reason = (!rest.is_empty()).then(|| rest.to_string());
                return Intercept::Reply(match self.engage(&chat.to_string(), reason) {
                    Ok(_) => "Stopped: every run halts now, and nothing new starts until /resume (or `ferrule stop --clear` on the machine).".into(),
                    Err(e) => format!("Couldn't write the stop file ({e}). Run `ferrule stop` on the machine."),
                });
            }
            "/resume" => {
                if !self.is_owner(&chat) {
                    return Intercept::Reply("Only the owner chat can resume ferrule.".into());
                }
                return Intercept::Reply(match self.clear(&chat.to_string()) {
                    Ok(true) => "Resumed: runs can start again.".into(),
                    Ok(false) => "ferrule wasn't stopped.".into(),
                    Err(e) => format!("Couldn't remove the stop file ({e})."),
                });
            }
            "/plan" if rest.is_empty() => {
                return Intercept::Reply(
                    "Usage: /plan <task> — explores read-only, then asks before running the plan."
                        .into(),
                );
            }
            "/plan" => return Intercept::Plan(rest.to_string()),
            _ => {}
        }
        match self.approvals.answer(&chat, text) {
            Some(reply) => Intercept::Reply(reply),
            None => Intercept::Pass,
        }
    }
}

/// An open approval: dropped before it finished (the run halted while
/// waiting), it withdraws the question and records that.
struct Waiting<'a> {
    hub: &'a Hub,
    code: String,
    /// The owner's channel the question went to.
    route: String,
    tree: &'a str,
    run: Option<String>,
    subject: &'a str,
    done: bool,
}

impl Waiting<'_> {
    fn finish(&mut self, answer: &str) {
        self.done = true;
        self.hub.approvals.withdraw(&self.code);
        self.hub.answered(
            self.tree,
            &self.run,
            json!({"subject": self.subject, "route": self.route, "code": self.code, "answer": answer}),
        );
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.finish("halted");
        }
    }
}

fn amount(unit: Unit, v: f64) -> String {
    match unit {
        Unit::Tokens => thousands(v as u64),
        Unit::Usd => format!("${v:.2}"),
    }
}

fn minutes(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 120 {
        format!("{} minutes", s / 60)
    } else {
        format!("{s} seconds")
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
