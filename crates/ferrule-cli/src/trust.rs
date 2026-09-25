//! M19 in the binary: the process's trust hub, who can approve a gated
//! command in each run tree, and the guard and ledger sink every agent
//! `build_agent_from` makes gets (docs/m19-trust-cost.md).

use crate::config::{self, Config};
use crate::ledger::{self, LedgerTag};
use anyhow::{anyhow, Result};
use ferrule_core::{LedgerRecord, LedgerSink, Transcript};
use ferrule_proxy::HostPattern;
use ferrule_trust::{Hub, Prompter, Route, SystemClock, TrustGuard, TrustSink};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal as _, Write as _};
use std::sync::{Arc, Mutex};

static HUB: Mutex<Option<Arc<Hub>>> = Mutex::new(None);
static ROUTES: Mutex<Option<HashMap<String, Route>>> = Mutex::new(None);
static PLANNING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The one hub of this process, built from the first config it's asked
/// with. A `[trust]` that doesn't validate is an error, not "no caps".
pub fn hub(cfg: &Config) -> Result<Arc<Hub>> {
    let mut slot = HUB.lock().unwrap();
    if let Some(h) = slot.as_ref() {
        return Ok(h.clone());
    }
    let data = config::data_dir()?;
    let hub = Arc::new(
        Hub::new(
            cfg.trust.clone(),
            &data,
            &ledger::ledger_path()?,
            Arc::new(SystemClock),
            bound_hosts(cfg),
        )
        .map_err(|e| anyhow!(e))?,
    );
    hub.set_owner(owner_chat(cfg));
    *slot = Some(hub.clone());
    Ok(hub)
}

/// The hosts a configured secret may reach: a DELETE to one of them is
/// gated, like one to a host the classifier can't read.
pub fn bound_hosts(cfg: &Config) -> Vec<HostPattern> {
    cfg.secrets
        .values()
        .flat_map(|spec| ferrule_proxy::SecretRule::from(spec).hosts)
        .filter_map(|h| HostPattern::parse(&h).ok())
        .collect()
}

/// `[trust] owner_chat`, else the first private chat (a positive id) the
/// gateway allows.
/// The hub, if something in this process made it already.
pub fn existing_hub() -> Option<Arc<Hub>> {
    HUB.lock().unwrap().clone()
}

pub fn owner_chat(cfg: &Config) -> Option<i64> {
    cfg.trust.owner_chat.or_else(|| {
        cfg.gateway
            .telegram_allowed_chats
            .iter()
            .copied()
            .find(|c| *c > 0)
    })
}

/// Who answers for `tree` from now on (`ferrule run` and `chat` seat the
/// terminal before building their root).
pub fn seat(tree: &str, route: Route) {
    ROUTES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(tree.to_string(), route);
}

/// Plan mode for `tree`: every agent built for it from now on (the root
/// and its sub-agents, which share the tree) explores read-only.
pub fn set_planning(tree: &str, on: bool) {
    let mut set = PLANNING.lock().unwrap();
    let set = set.get_or_insert_with(HashSet::new);
    if on {
        set.insert(tree.to_string());
    } else {
        set.remove(tree);
    }
}

pub fn is_planning(tree: &str) -> bool {
    PLANNING
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|s| s.contains(tree))
}

/// Who can approve a gated command in `tree`: what was seated for it, else
/// what its session id says. A scheduled task and anything unknown ask
/// nobody.
pub fn route_for(tree: &str) -> Route {
    if let Some(r) = ROUTES.lock().unwrap().as_ref().and_then(|m| m.get(tree)) {
        return r.clone();
    }
    if let Some(task) = tree.strip_prefix(ferrule_trust::meter::TASK_PREFIX) {
        return Route::Unattended(format!(
            "this is scheduled task `{task}`, which runs unattended"
        ));
    }
    if let Some(chat) = tree.strip_prefix("telegram__") {
        return Route::Owner {
            chat_label: format!("Telegram chat {chat}"),
        };
    }
    Route::Unattended(format!("session `{tree}` has nobody to ask"))
}

/// The terminal when stdin is one; a run piped or started by a script
/// asks nobody.
pub fn terminal_route() -> Route {
    if std::io::stdin().is_terminal() {
        Route::Terminal(Arc::new(StdinPrompter))
    } else {
        Route::Unattended("this run has no terminal to ask (stdin isn't a TTY)".into())
    }
}

struct StdinPrompter;

impl Prompter for StdinPrompter {
    fn ask(&self, text: &str) -> Option<String> {
        eprint!("\n\x1b[1;33m{text}\x1b[0m");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

/// The run tree an agent belongs to: a sub-agent's root session, else its
/// own session (its transcript's name), else a tree of its own.
pub fn tree_of(
    child: Option<&ferrule_agents::ChildSpec>,
    transcript: Option<&Transcript>,
) -> String {
    if let Some(spec) = child {
        return spec.tree.clone();
    }
    transcript
        .and_then(|t| t.path().file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// What `build_agent_from` hands the agent: its ledger tag with the sink
/// wrapped so every call is stamped with the tree, priced and charged to
/// the hub (with no ledger, the hub is still charged), and its guard.
pub fn equip(
    cfg: &Config,
    tree: &str,
    child: bool,
    ledger: Option<LedgerTag>,
) -> Result<(LedgerTag, Arc<TrustGuard>)> {
    let hub = hub(cfg)?;
    let (inner, shape, origin) = match ledger {
        Some(t) => (t.sink, t.task_shape, t.origin),
        None => (
            Arc::new(NoLedger) as Arc<dyn LedgerSink>,
            "run".to_string(),
            None,
        ),
    };
    // Priced by the model that ran (M21), so a fallback counts at its own.
    let prices = crate::models::prices(cfg);
    let price: ferrule_trust::Pricer =
        Arc::new(move |r: &LedgerRecord| prices(&r.provider, &r.model).map(|p| p.cost_usd(r)));
    let sink = Arc::new(TrustSink::new(inner, hub.clone(), tree, Some(price)));
    let root = TrustGuard::root(hub, tree, route_for(tree)).planning(is_planning(tree));
    let guard = if child { root.child() } else { root };
    Ok((
        LedgerTag {
            sink,
            task_shape: shape,
            origin,
        },
        Arc::new(guard),
    ))
}

struct NoLedger;

impl LedgerSink for NoLedger {
    fn record(&self, _: LedgerRecord) {}
}

/// Sends the owner's warnings and questions through the gateway's Telegram
/// channel.
pub struct ChannelNotifier(pub Arc<dyn ferrule_gateway::Channel>);

#[async_trait::async_trait]
impl ferrule_trust::Notifier for ChannelNotifier {
    async fn send(&self, chat: i64, text: &str) -> Result<(), String> {
        self.0
            .send(ferrule_gateway::OutboundMessage {
                channel: self.0.name().to_string(),
                chat_id: chat.to_string(),
                text: text.to_string(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// The owner's commands in Telegram, before any chat turn: `/stop`,
/// `/resume`, `/plan` and the answers to open approvals. Other channels
/// pass straight through.
pub struct OwnerDoor {
    pub hub: Arc<Hub>,
    /// Runs `/plan <task>` for a chat; its reply is the acknowledgement.
    pub plan: Option<Arc<dyn Fn(i64, String) -> String + Send + Sync>>,
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for OwnerDoor {
    async fn intercept(&self, msg: &ferrule_gateway::InboundMessage) -> Option<String> {
        if msg.channel != "telegram" {
            return None;
        }
        let chat = msg.chat_id.parse::<i64>().ok()?;
        match self.hub.intercept(chat, &msg.text) {
            ferrule_trust::Intercept::Pass => None,
            ferrule_trust::Intercept::Reply(r) => Some(r),
            ferrule_trust::Intercept::Plan(task) => Some(match &self.plan {
                Some(plan) => plan(chat, task),
                None => "Plan mode isn't available in this gateway.".into(),
            }),
        }
    }
}

/// Holds the scheduler's tick while the kill switch is on.
pub fn scheduler_hold(hub: Arc<Hub>) -> ferrule_gateway::Hold {
    Arc::new(move || {
        hub.stopped()
            .map(|info| ferrule_trust::kill::stop_message(&info))
    })
}

#[derive(clap::Subcommand)]
pub enum TrustCmd {
    /// The caps, today's spend against them, the kill switch and who approves
    Status,
    /// What the guard did: stops, warnings, approvals, refusals, plans
    Audit {
        /// `7d`, `12h`, `30m`, or an RFC 3339 instant
        #[arg(long)]
        since: Option<String>,
    },
}

/// `ferrule stop`: every run in every ferrule process halts at its next
/// step and nothing new starts, until `--clear`.
pub fn stop_cmd(reason: Option<String>, clear: bool, status: bool) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let hub = hub(&cfg)?;
    if status {
        match hub.stopped() {
            Some(info) => println!("{}", ferrule_trust::kill::stop_message(&info)),
            None => println!("not stopped"),
        }
        return Ok(());
    }
    if clear {
        if hub.clear("ferrule stop --clear")? {
            println!("cleared: runs can start again");
        } else {
            println!("ferrule wasn't stopped");
        }
        return Ok(());
    }
    let info = hub.engage("ferrule stop", reason)?;
    println!(
        "stopped at {}: running agents halt at their next step, and nothing new starts until `ferrule stop --clear` (or /resume from the owner chat)",
        info.at
    );
    println!("stop file: {}", hub.kill_switch().path().display());
    Ok(())
}

pub fn cmd(op: TrustCmd) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let hub = hub(&cfg)?;
    match op {
        TrustCmd::Status => {
            for line in status_lines(&hub) {
                println!("{line}");
            }
        }
        TrustCmd::Audit { since } => {
            let since = since
                .map(|s| ledger::parse_since(&s, chrono::Utc::now()))
                .transpose()?;
            let events = hub.audit().read(since)?;
            if events.is_empty() {
                println!("no trust events in {}", hub.audit().path().display());
            }
            for e in events {
                let tree = e.tree.map(|t| format!(" [{t}]")).unwrap_or_default();
                println!("{} {}{} {}", e.at, e.event, tree, e.detail);
            }
        }
    }
    Ok(())
}

/// What `ferrule trust status` prints.
pub fn status_lines(hub: &Hub) -> Vec<String> {
    let c = hub.config();
    let tokens = |n: u64| {
        if n == 0 {
            "off".to_string()
        } else {
            ferrule_trust::hub::thousands(n)
        }
    };
    let usd = |v: f64| {
        if v == 0.0 {
            "off".to_string()
        } else {
            format!("${v:.2}")
        }
    };
    let mut out = vec![match hub.stopped() {
        Some(info) => format!(
            "kill switch: ON — {}",
            ferrule_trust::kill::stop_message(&info)
        ),
        None => "kill switch: off".into(),
    }];
    out.push(format!(
        "per run:  {} tokens, {}",
        tokens(c.max_tokens_per_run),
        usd(c.max_usd_per_run)
    ));
    out.push(format!(
        "per day:  {} tokens, {} ({})",
        tokens(c.max_tokens_per_day),
        usd(c.max_usd_per_day),
        c.timezone
    ));
    out.push(format!(
        "per task: {} tokens, {}",
        tokens(c.max_tokens_per_task),
        usd(c.max_usd_per_task)
    ));
    out.push(match hub.today(None) {
        Ok((today, _)) => format!(
            "today:    {} tokens, ${:.2}",
            ferrule_trust::hub::thousands(today.tokens),
            today.usd
        ),
        Err(e) => format!("today:    unknown — {e}"),
    });
    out.push(format!(
        "gates:    {}",
        if c.gates {
            "on (rm -r, force push, DELETE to a bound host)"
        } else {
            "off"
        }
    ));
    out.push(match hub.owner() {
        Some(chat) => format!("approvals: Telegram chat {chat} (gateway), or the terminal"),
        None => "approvals: the terminal only (no owner chat); unattended runs refuse".into(),
    });
    out
}
