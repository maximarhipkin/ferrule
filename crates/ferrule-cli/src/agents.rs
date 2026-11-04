//! Sub-agents in the CLI: the supervisor every entry point shares, the
//! factory that builds a child like any other agent (narrowed to what its
//! role may do), the wakers that run an idle root when its children report,
//! and `ferrule agents`.

use crate::config::{self, AgentsConfig, Config};
use crate::ledger;
use crate::models::Scope;
use anyhow::{bail, Result};
use ferrule_agents::{
    AgentRow, AgentStore, ChildFactory, ChildSpec, Role, Status, Supervisor, Waker,
};
use ferrule_core::tool::Tool;
use ferrule_core::LedgerSink;
use ferrule_gateway::Router;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use tokio::sync::mpsc;

/// Builds one agent: the CLI's `build_agent_from`, handed in so this module
/// doesn't need to know how.
pub type Build = Arc<
    dyn Fn(Scope, &ChildSpec, Option<ledger::LedgerTag>) -> Result<ferrule_core::Agent>
        + Send
        + Sync,
>;

/// The supervisor for this process, or None when `[agents] enabled =
/// false`. `provider` is the process's one-off (`--provider`/`--model`);
/// a role in `[agents.roles]` can name another model. Otherwise a child
/// runs on what its root's task or chat runs on (docs/m21-models.md §3).
pub fn supervisor(
    cfg: &Config,
    provider: Option<String>,
    sink: Option<Arc<dyn LedgerSink>>,
    build: Build,
) -> Result<Option<Arc<Supervisor>>> {
    if !cfg.agents.enabled {
        return Ok(None);
    }
    check_roles(cfg)?;
    let roles: HashMap<String, Option<String>> = cfg
        .agents
        .roles
        .iter()
        .map(|(r, c)| (r.clone(), c.model.clone().or_else(|| c.provider.clone())))
        .collect();
    let me: Arc<OnceLock<Weak<Supervisor>>> = Arc::default();
    let factory: ChildFactory = {
        let me = me.clone();
        Arc::new(move |spec: &ChildSpec| {
            let sup = me.get().and_then(Weak::upgrade);
            let mut scope = Scope::for_session(&spec.tree);
            scope.session = spec.id.clone();
            // A model spawn_agent named (checked as connected when it
            // spawned) goes ahead of its role's.
            let scope = match (&spec.model, role_provider(&roles, sup.as_deref(), spec)) {
                (Some(m), _) => scope.fixed(Some(m.clone()), "spawn_agent's model"),
                (None, Some((role, word))) => scope.fixed(Some(word), &format!("role {role}")),
                (None, None) => scope.fixed(provider.clone(), "the root's model"),
            };
            let tag =
                ledger::LedgerTag::new(&sink, "agent", Some(format!("agent:{}", spec.parent)));
            build(scope, spec, tag).map_err(|e| format!("{e:#}"))
        })
    };
    let data = config::data_dir()?;
    let store = AgentStore::open(data.join("agents.db"))?;
    let sup = Supervisor::new(store, data.join("sessions"), cfg.agents.limits(), factory)?;
    let _ = me.set(Arc::downgrade(&sup));
    *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&sup);
    let models = crate::models::shared()?;
    // M67: with `[models] bot_picks`, the agent picks any model the
    // connected OpenRouter has; one that isn't connected yet is connected
    // for the spawn (`deny` still refuses it).
    let openrouter = cfg
        .providers
        .iter()
        .find(|(_, p)| p.plan.is_none() && p.base_url.contains("openrouter.ai"))
        .map(|(name, _)| name.clone())
        .filter(|_| cfg.models.bot_picks);
    if openrouter.is_some() {
        sup.set_model_hint(PICK_HINT.into());
    }
    sup.set_model_check(Arc::new(move |word| {
        let why = match models.resolve(word) {
            Ok(e) => return Ok(e.reference()),
            Err(why) => why,
        };
        let Some(or) = &openrouter else {
            return Err(why);
        };
        let word = word.trim();
        let id = word.strip_prefix(&format!("{or}/")).unwrap_or(word);
        // Connected but refused (denied), or not an id at all: as it was.
        if models.catalog().resolve_unchecked(word).is_ok() || !ferrule_media::is_model_id(id) {
            return Err(why);
        }
        models
            .add_model(or, id, None, "the bot")
            .map_err(|e| format!("{e:#}"))?;
        models.resolve(&format!("{or}/{id}")).map(|e| e.reference())
    }));
    sup.set_worktrees_dir(data.join("worktrees"));
    sup.set_named_agents(named_agents(cfg));
    Ok(Some(sup))
}

/// M67: `spawn_agent`'s `model`, when the bot picks.
const PICK_HINT: &str = "The model best suited to this agent's job: pick it yourself when the \
    job needs a different strength (find_models kind=text lists OpenRouter's, with prices; it \
    must take tools). A connected model (provider/model or an alias) or an OpenRouter id like \
    google/gemini-3.8-flash. Leave it out to use its role's model or yours.";

/// M66: the supervisor this process runs its agents on (the gateway's),
/// so the page's Close stops a running one too.
static LIVE: std::sync::Mutex<Weak<Supervisor>> = std::sync::Mutex::new(Weak::new());

/// The process's supervisor, while it lives.
pub fn live() -> Option<Arc<Supervisor>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).upgrade()
}

/// M68: `[agents.custom]`, as the supervisor offers them (roles checked
/// by [`check_roles`]).
pub fn named_agents(cfg: &Config) -> Vec<ferrule_agents::NamedAgent> {
    cfg.agents
        .custom
        .iter()
        .map(|(name, c)| ferrule_agents::NamedAgent {
            name: name.clone(),
            about: c.about.clone(),
            role: c
                .role
                .as_deref()
                .and_then(Role::parse)
                .unwrap_or(Role::Worker),
            model: c.model.clone().filter(|m| !m.trim().is_empty()),
            instructions: c.instructions.clone(),
        })
        .collect()
}

/// M68: a name the page and `spawn_agent` take for an agent of the owner's.
pub fn agent_name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && Role::parse(name).is_none()
}

pub fn check_roles(cfg: &Config) -> Result<()> {
    for (name, c) in &cfg.agents.custom {
        if !agent_name_ok(name) {
            bail!("[agents.custom.{name}]: a name is 1–32 of a-z, 0-9 and -, and not a role's");
        }
        if let Some(r) = c.role.as_deref().filter(|r| Role::parse(r).is_none()) {
            bail!("[agents.custom.{name}] role = \"{r}\": the roles are worker, planner, verifier and media");
        }
    }
    for (role, rc) in &cfg.agents.roles {
        if Role::parse(role).is_none() {
            bail!(
                "[agents.roles.{role}]: no such role; the roles are worker, planner, verifier and media"
            );
        }
        if let Some(p) = &rc.provider {
            if !cfg.providers.contains_key(p) {
                bail!("[agents.roles.{role}] provider = \"{p}\": there is no [providers.{p}]");
            }
        }
        if let Some(m) = &rc.model {
            if rc.provider.is_some() {
                bail!("[agents.roles.{role}]: set `model` or `provider`, not both");
            }
            if let Err(e) = crate::models::Catalog::from_config(cfg).resolve(m) {
                bail!("[agents.roles.{role}] model = \"{m}\": {e}");
            }
        }
    }
    Ok(())
}

/// The model for `spec` (a ref, or a provider's name) and the role that
/// names it: its own role's, else the nearest ancestor's whose role has
/// one, so a verifier's helpers stay on the verifier's model. None: the
/// root's.
fn role_provider(
    roles: &HashMap<String, Option<String>>,
    sup: Option<&Supervisor>,
    spec: &ChildSpec,
) -> Option<(String, String)> {
    let mut chain = vec![spec.role.as_str().to_string()];
    if let Some(sup) = sup {
        let mut next = Some(spec.parent.clone());
        while let Some(id) = next {
            let Ok(Some(row)) = sup.store().get(&id) else {
                break;
            };
            chain.push(row.role);
            next = row.parent;
        }
    }
    chain
        .iter()
        .find_map(|r| Some((r.clone(), roles.get(r).cloned().flatten()?)))
}

/// What a child may use of the tools the root has: a read-only child
/// can't write files or memories, and a verifier or read-only child only
/// gets the MCP tools that don't change anything (MCP servers run in the
/// root's workspace, not the child's copy).
pub fn narrows(spec: Option<&ChildSpec>) -> (bool, bool) {
    match spec {
        None => (false, false),
        Some(s) => (s.read_only, s.read_only || s.role == Role::Verifier),
    }
}

pub fn only_reading(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .filter(|t| !t.changes_files())
        .cloned()
        .collect()
}

/// The gateway's: a chat's root is run by its lane, answering in the chat.
pub struct RouterWaker(pub Weak<Router>);

impl Waker for RouterWaker {
    fn can_wake(&self, root: &str) -> bool {
        self.0.upgrade().is_some_and(|r| r.can_wake(root))
    }
    fn wake(&self, root: &str, text: String) -> bool {
        self.0.upgrade().is_some_and(|r| r.wake(root, text))
    }
}

/// `ferrule run`'s: the loop that waits for the tree runs the root.
pub struct ChannelWaker {
    pub root: String,
    pub tx: mpsc::UnboundedSender<String>,
}

impl Waker for ChannelWaker {
    fn can_wake(&self, root: &str) -> bool {
        root == self.root
    }
    fn wake(&self, root: &str, text: String) -> bool {
        root == self.root && self.tx.send(text).is_ok()
    }
}

pub fn store() -> Result<AgentStore> {
    Ok(AgentStore::open(config::data_dir()?.join("agents.db"))?)
}

/// `ferrule agents list`: every tree, children indented under parents.
pub fn list(all: bool) -> Result<()> {
    let store = store()?;
    let rows: Vec<AgentRow> = store
        .all()?
        .into_iter()
        .filter(|r| all || r.status != Status::Closed || r.parent.is_none())
        .collect();
    let mut shown = 0;
    for root in rows.iter().filter(|r| r.parent.is_none()) {
        let mut lines = Vec::new();
        below(&rows, &root.id, 1, &store, &mut lines);
        if lines.is_empty() {
            continue;
        }
        println!("{} (root)", root.id);
        for line in &lines {
            println!("{line}");
        }
        shown += lines.len();
    }
    if shown == 0 {
        println!(
            "No agents{}.",
            if all {
                ""
            } else {
                " open (--all shows closed ones)"
            }
        );
    }
    Ok(())
}

/// A line per agent below `parent`, each followed by its own children.
fn below(
    rows: &[AgentRow],
    parent: &str,
    depth: usize,
    store: &AgentStore,
    lines: &mut Vec<String>,
) {
    for r in rows.iter().filter(|r| r.parent.as_deref() == Some(parent)) {
        let name = r
            .name
            .as_deref()
            .map(|n| format!(" \"{n}\""))
            .unwrap_or_default();
        let elsewhere =
            if r.status == Status::Running && store.running_elsewhere(&r.id).unwrap_or(false) {
                " (in another ferrule process)"
            } else {
                ""
            };
        let branch = r
            .branch
            .as_deref()
            .map(|b| format!(", branch {b}"))
            .unwrap_or_default();
        let model = r
            .model
            .as_deref()
            .map(|m| format!(", on {m}"))
            .unwrap_or_default();
        lines.push(format!(
            "{}{}{name} [{}] {}{elsewhere}, {} tokens{branch}{model}",
            "  ".repeat(depth),
            r.id,
            r.role,
            r.status,
            r.tokens
        ));
        below(rows, &r.id, depth + 1, store, lines);
    }
}

/// `ferrule agents close`: `id` and everything it started.
pub async fn close(cfg: &AgentsConfig, id: &str) -> Result<()> {
    println!("{}", close_text(cfg, &config::data_dir()?, id, None).await?);
    Ok(())
}

/// What `ferrule agents close` does, and its words (M66: the page's Close
/// too), over `<data>`. With `sup` (this process's own, which runs them) a
/// running agent here is stopped as well; one running in another process
/// is refused.
pub async fn close_text(
    cfg: &AgentsConfig,
    data: &std::path::Path,
    id: &str,
    sup: Option<Arc<Supervisor>>,
) -> Result<String> {
    let store = AgentStore::open(data.join("agents.db"))?;
    let Some(row) = store.get(id)? else {
        bail!("there is no agent {id}; `ferrule agents list` shows them");
    };
    let mut stack = vec![row.id.clone()];
    while let Some(next) = stack.pop() {
        if store.running_elsewhere(&next)? {
            bail!(
                "agent {next} is running in another ferrule process (the gateway, or a `ferrule run`); \
                 only that process can stop it"
            );
        }
        stack.extend(store.children(&next)?.into_iter().map(|c| c.id));
    }
    let sup = match sup {
        Some(sup) => sup,
        None => {
            let refuse: ChildFactory =
                Arc::new(|_| Err("`ferrule agents close` starts no agents".into()));
            let sup = Supervisor::new(store, data.join("sessions"), cfg.limits(), refuse)?;
            sup.set_worktrees_dir(data.join("worktrees"));
            sup
        }
    };
    let closed = sup.close_tree(id).await?;
    let mut lines = vec![if closed.ids.is_empty() {
        format!("{id} was already closed.")
    } else {
        format!("Closed {}.", closed.ids.join(", "))
    }];
    lines.extend(closed.notes);
    Ok(lines.join("\n"))
}
