//! Self-service from the conversation (M48): one agent tool, `ferrule_admin`,
//! so the owner changes Ferrule from the chat and never logs in to the
//! machine.
//!
//! The tool is a typed list of ops, not a shell. A read-only op runs at
//! once; every change is shown to the owner as a card ("Switch the default
//! model to b (now a)"), is bound to that exact change, and runs only after
//! the owner's own tap or reply (`ferrule_trust::Approvals`: owner-only,
//! single-use, ten minutes). The change itself goes through the dashboard's
//! own handlers (`dashboard::api::act`), so there is one code path for it
//! whoever asks.

pub(crate) mod catalog;
mod describe;
pub(crate) mod doctor;
mod door;
pub(crate) mod keys;
mod locks;
pub mod menu;
mod packages;
pub mod promise;
mod quick;
pub(crate) mod restart;
mod run;
mod tool;
pub(crate) mod update;

#[cfg(test)]
mod tests;

pub use door::SelfServiceDoor;
pub use tool::AdminTool;

use crate::dashboard::{Ctx, Dashboard};
use crate::update::apply::Apply;
use ferrule_gateway::Router;
use ferrule_trust::{ChatRef, Hub};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

/// How long a card waits for the owner.
pub const ASK_FOR: Duration = Duration::from_secs(600);

/// Ops that only read: they run without asking.
pub const READ_OPS: &[&str] = &[
    "status",
    "doctor",
    "audit",
    "update_check",
    "models",
    "model_test",
    "settings",
    "tasks",
    "connections",
    "config_get",
    "secrets",
    "secret_request",
    "dashboard_link",
];

/// Ops that change something: each asks the owner first.
pub const CHANGE_OPS: &[&str] = &[
    "model_default",
    "model_here",
    "model_fallback",
    "config_set",
    "caps",
    "skill_on",
    "skill_off",
    "mcp_on",
    "mcp_off",
    "mcp_remove",
    "mcp_add",
    "hooks_trust",
    "hooks_untrust",
    "task_add",
    "task_schedule",
    "task_model",
    "task_pause",
    "task_resume",
    "task_delete",
    "task_run_now",
    "channel_restart",
    "config_restore",
    "backup",
    "disconnect",
    "package_install",
    "package_remove",
    "update",
    "restart",
];

/// One thing the tool can do. Closed: nothing here takes a command line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Status,
    Doctor,
    Audit {
        limit: usize,
    },
    UpdateCheck,
    Models,
    ModelTest {
        model: String,
    },
    Settings,
    Tasks,
    Connections,
    ConfigGet {
        key: Option<String>,
    },
    Secrets,
    /// The owner is sent a link to add the key (M54 §6.3, `keys::request`).
    SecretRequest {
        name: String,
        hosts: Vec<String>,
        why: String,
    },
    DashboardLink,
    ModelDefault {
        model: String,
    },
    ModelHere {
        model: Option<String>,
    },
    ModelFallback {
        models: Vec<String>,
    },
    ConfigSet {
        key: String,
        value: Value,
    },
    Caps {
        caps: BTreeMap<String, f64>,
    },
    SkillOn {
        name: String,
    },
    SkillOff {
        name: String,
    },
    McpOn {
        name: String,
    },
    McpOff {
        name: String,
    },
    McpRemove {
        name: String,
    },
    McpAdd {
        name: String,
        command: Vec<String>,
        url: Option<String>,
        env: Vec<String>,
        headers: Vec<String>,
        enabled_tools: Vec<String>,
        replace: bool,
    },
    /// M63.4: `mcp_add` with `catalog`: everything from the catalog entry,
    /// its key asked for with `secret_request` (`catalog::add`).
    McpAddCatalog {
        catalog: String,
        write: bool,
    },
    HooksTrust {
        sha: String,
    },
    HooksUntrust,
    TaskAdd {
        name: String,
        prompt: String,
        schedule: String,
        kind: Option<String>,
        timezone: Option<String>,
        model: Option<String>,
    },
    TaskSchedule {
        id: String,
        schedule: String,
        timezone: Option<String>,
    },
    TaskModel {
        id: String,
        model: Option<String>,
    },
    TaskPause {
        id: String,
    },
    TaskResume {
        id: String,
    },
    TaskDelete {
        id: String,
    },
    TaskRunNow {
        id: String,
    },
    ChannelRestart {
        name: String,
    },
    ConfigRestore,
    Backup,
    Disconnect {
        name: String,
    },
    PackageInstall {
        packages: Vec<String>,
    },
    PackageRemove {
        packages: Vec<String>,
    },
    Update,
    Restart,
}

/// Every argument any op takes, flat, as the model sends them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    op: String,
    limit: Option<usize>,
    model: Option<String>,
    models: Option<Vec<String>>,
    key: Option<String>,
    value: Option<Value>,
    caps: Option<BTreeMap<String, f64>>,
    name: Option<String>,
    sha: Option<String>,
    id: Option<String>,
    prompt: Option<String>,
    schedule: Option<String>,
    kind: Option<String>,
    timezone: Option<String>,
    packages: Option<Vec<String>>,
    command: Option<Vec<String>>,
    url: Option<String>,
    env: Option<Vec<String>>,
    headers: Option<Vec<String>>,
    enabled_tools: Option<Vec<String>>,
    replace: Option<bool>,
    hosts: Option<Vec<String>>,
    why: Option<String>,
    catalog: Option<String>,
    write: Option<bool>,
}

impl Op {
    /// Reads the tool's arguments. An unknown op, a missing field and an
    /// unknown field (`"approved": true` among them) are all refused.
    pub fn from_args(v: &Value) -> Result<Op, String> {
        let a: Args = serde_json::from_value(v.clone())
            .map_err(|e| format!("the arguments don't read: {e}"))?;
        let op = a.op.as_str();
        macro_rules! need {
            ($f:ident) => {
                a.$f.clone()
                    .ok_or_else(|| format!("`{op}` needs `{}`", stringify!($f)))?
            };
        }
        Ok(match op {
            "status" => Op::Status,
            "doctor" => Op::Doctor,
            "audit" => Op::Audit {
                limit: a.limit.unwrap_or(20).clamp(1, 100),
            },
            "update_check" => Op::UpdateCheck,
            "models" => Op::Models,
            "model_test" => Op::ModelTest {
                model: need!(model),
            },
            "settings" => Op::Settings,
            "tasks" => Op::Tasks,
            "connections" => Op::Connections,
            "config_get" => Op::ConfigGet { key: a.key.clone() },
            "secrets" => Op::Secrets,
            "secret_request" => Op::SecretRequest {
                name: need!(name),
                hosts: need!(hosts),
                why: need!(why),
            },
            "dashboard_link" => Op::DashboardLink,
            "model_default" => Op::ModelDefault {
                model: need!(model),
            },
            "model_here" => Op::ModelHere {
                model: a.model.clone(),
            },
            "model_fallback" => Op::ModelFallback {
                models: a.models.clone().unwrap_or_default(),
            },
            "config_set" => Op::ConfigSet {
                key: need!(key),
                value: need!(value),
            },
            "caps" => Op::Caps { caps: need!(caps) },
            "skill_on" => Op::SkillOn { name: need!(name) },
            "skill_off" => Op::SkillOff { name: need!(name) },
            "mcp_on" => Op::McpOn { name: need!(name) },
            "mcp_off" => Op::McpOff { name: need!(name) },
            "mcp_remove" => Op::McpRemove { name: need!(name) },
            "mcp_add" if a.catalog.is_some() => {
                // Everything comes from the catalog: nothing the model
                // names may change where the server runs or its key goes.
                if a.command.is_some()
                    || a.url.is_some()
                    || a.env.is_some()
                    || a.headers.is_some()
                    || a.enabled_tools.is_some()
                    || a.hosts.is_some()
                {
                    return Err(
                        "`mcp_add` with `catalog` takes only `catalog` and `write`: the rest comes from the catalog"
                            .into(),
                    );
                }
                Op::McpAddCatalog {
                    catalog: need!(catalog).trim().to_ascii_lowercase(),
                    write: a.write.unwrap_or(false),
                }
            }
            "mcp_add" => Op::McpAdd {
                name: need!(name),
                command: a.command.clone().unwrap_or_default(),
                url: a.url.clone(),
                env: a.env.clone().unwrap_or_default(),
                headers: a.headers.clone().unwrap_or_default(),
                enabled_tools: a.enabled_tools.clone().unwrap_or_default(),
                replace: a.replace.unwrap_or(false),
            },
            "hooks_trust" => Op::HooksTrust { sha: need!(sha) },
            "hooks_untrust" => Op::HooksUntrust,
            "task_add" => Op::TaskAdd {
                name: need!(name),
                prompt: need!(prompt),
                schedule: need!(schedule),
                kind: a.kind.clone(),
                timezone: a.timezone.clone(),
                model: a.model.clone(),
            },
            "task_schedule" => Op::TaskSchedule {
                id: need!(id),
                schedule: need!(schedule),
                timezone: a.timezone.clone(),
            },
            "task_model" => Op::TaskModel {
                id: need!(id),
                model: a.model.clone(),
            },
            "task_pause" => Op::TaskPause { id: need!(id) },
            "task_resume" => Op::TaskResume { id: need!(id) },
            "task_delete" => Op::TaskDelete { id: need!(id) },
            "task_run_now" => Op::TaskRunNow { id: need!(id) },
            "channel_restart" => Op::ChannelRestart { name: need!(name) },
            "config_restore" => Op::ConfigRestore,
            "backup" => Op::Backup,
            "disconnect" => Op::Disconnect { name: need!(name) },
            "package_install" => Op::PackageInstall {
                packages: need!(packages),
            },
            "package_remove" => Op::PackageRemove {
                packages: need!(packages),
            },
            "update" => Op::Update,
            "restart" => Op::Restart,
            other => {
                return Err(format!(
                    "`{other}` isn't an op; the ops are: {}",
                    READ_OPS
                        .iter()
                        .chain(CHANGE_OPS)
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        })
    }

    /// The op's name, as the tool and the audit log say it.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Status => "status",
            Op::Doctor => "doctor",
            Op::Audit { .. } => "audit",
            Op::UpdateCheck => "update_check",
            Op::Models => "models",
            Op::ModelTest { .. } => "model_test",
            Op::Settings => "settings",
            Op::Tasks => "tasks",
            Op::Connections => "connections",
            Op::ConfigGet { .. } => "config_get",
            Op::Secrets => "secrets",
            Op::SecretRequest { .. } => "secret_request",
            Op::DashboardLink => "dashboard_link",
            Op::ModelDefault { .. } => "model_default",
            Op::ModelHere { .. } => "model_here",
            Op::ModelFallback { .. } => "model_fallback",
            Op::ConfigSet { .. } => "config_set",
            Op::Caps { .. } => "caps",
            Op::SkillOn { .. } => "skill_on",
            Op::SkillOff { .. } => "skill_off",
            Op::McpOn { .. } => "mcp_on",
            Op::McpOff { .. } => "mcp_off",
            Op::McpRemove { .. } => "mcp_remove",
            Op::McpAdd { .. } | Op::McpAddCatalog { .. } => "mcp_add",
            Op::HooksTrust { .. } => "hooks_trust",
            Op::HooksUntrust => "hooks_untrust",
            Op::TaskAdd { .. } => "task_add",
            Op::TaskSchedule { .. } => "task_schedule",
            Op::TaskModel { .. } => "task_model",
            Op::TaskPause { .. } => "task_pause",
            Op::TaskResume { .. } => "task_resume",
            Op::TaskDelete { .. } => "task_delete",
            Op::TaskRunNow { .. } => "task_run_now",
            Op::ChannelRestart { .. } => "channel_restart",
            Op::ConfigRestore => "config_restore",
            Op::Backup => "backup",
            Op::Disconnect { .. } => "disconnect",
            Op::PackageInstall { .. } => "package_install",
            Op::PackageRemove { .. } => "package_remove",
            Op::Update => "update",
            Op::Restart => "restart",
        }
    }

    pub fn is_change(&self) -> bool {
        CHANGE_OPS.contains(&self.name())
    }

    /// The first 12 hex characters of the sha256 of the op's canonical
    /// JSON: the approval is bound to it.
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let sum = ring::digest::digest(&ring::digest::SHA256, &bytes);
        sum.as_ref()
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Where the tool gets the dashboard's handlers from: the page's own
/// context when the page is on, else one of its own.
enum Bound {
    Page(Arc<Dashboard>),
    Own(Box<Ctx>),
}

/// What every `ferrule_admin` tool shares (one per gateway).
pub struct Admin {
    pub hub: Arc<Hub>,
    bound: OnceLock<Bound>,
    ask_for: Duration,
    /// The gateway's lanes: a restart waits for the asking turn to end.
    router: OnceLock<Weak<Router>>,
    /// What the update ops install with; tests point it at a mock.
    updater: OnceLock<Updater>,
    /// Owner mode (M54 §5): change ops run at once, without the card.
    autonomous: bool,
}

/// The apply flow to use and whether the update units are installed.
type Updater = Arc<dyn Fn() -> Result<(Apply<'static>, bool), String> + Send + Sync>;

impl Admin {
    pub fn new(hub: Arc<Hub>, autonomous: bool) -> Arc<Self> {
        Arc::new(Self {
            hub,
            bound: OnceLock::new(),
            ask_for: ASK_FOR,
            router: OnceLock::new(),
            updater: OnceLock::new(),
            autonomous,
        })
    }

    /// Whether change ops run without asking the owner (owner mode).
    pub fn autonomous(&self) -> bool {
        self.autonomous
    }

    /// A test's card that waits `ask_for`, not ten minutes.
    #[cfg(test)]
    pub fn with_timeout(hub: Arc<Hub>, ask_for: Duration) -> Arc<Self> {
        Arc::new(Self {
            hub,
            bound: OnceLock::new(),
            ask_for,
            router: OnceLock::new(),
            updater: OnceLock::new(),
            autonomous: false,
        })
    }

    #[cfg(test)]
    pub fn set_updater(&self, f: Updater) {
        let _ = self.updater.set(f);
    }

    /// The gateway builds its agents before the page; the page's context
    /// is bound once it exists. A second bind is ignored.
    pub fn bind_page(&self, dash: Arc<Dashboard>) {
        let _ = self.bound.set(Bound::Page(dash));
    }

    pub fn bind_router(&self, router: Weak<Router>) {
        keys::bind(router.clone());
        let _ = self.router.set(router);
    }

    pub(crate) fn router(&self) -> Weak<Router> {
        self.router.get().cloned().unwrap_or_default()
    }

    /// This process's update flow, from the config.
    pub(crate) fn apply(&self, ctx: &Ctx) -> Result<(Apply<'static>, bool), String> {
        if let Some(f) = self.updater.get() {
            return f();
        }
        let data = ctx.data.clone().ok_or("there's no data dir here")?;
        Ok((update::real_apply(data)?, crate::update::units_installed()))
    }

    pub fn bind_own(&self, ctx: Ctx) {
        let _ = self.bound.set(Bound::Own(Box::new(ctx)));
    }

    /// The page, when it's on in this process.
    pub(crate) fn page(&self) -> Option<&Arc<Dashboard>> {
        match self.bound.get()? {
            Bound::Page(d) => Some(d),
            Bound::Own(_) => None,
        }
    }

    pub(crate) fn ctx(&self) -> Option<&Ctx> {
        match self.bound.get()? {
            Bound::Page(d) => Some(&d.ctx),
            Bound::Own(c) => Some(c),
        }
    }
}

/// The chat a session may use the tool in: the owner's private chat on a
/// channel that has one, or the dashboard's own chat. `None` for a group,
/// someone else's chat, a task and a sub-agent.
pub fn offered(hub: &Hub, session_id: &str) -> Option<ChatRef> {
    if session_id == "dashboard__owner" {
        return Some(ChatRef::new("dashboard", "owner"));
    }
    let (info, chat) = crate::channels::of_session(session_id)?;
    let owner = hub.owner_on(info.name)?;
    if ferrule_gateway::session::session_id(&owner.channel, &owner.chat) != session_id {
        return None;
    }
    if info.name == "telegram" && chat.parse::<i64>().ok()? <= 0 {
        return None;
    }
    Some(ChatRef::new(info.name, chat))
}
