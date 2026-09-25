//! The supervisor: spawns children in the background, runs them, records
//! how each run ended, delivers notices to parents, wakes an idle root,
//! enforces the limits and closes agents. One per process, shared by every
//! session.

use crate::error::AgentsError;
use crate::fence::{cap, fence, first_line};
use crate::lifecycle;
use crate::prompts;
use crate::store::{AgentRow, AgentStore, Status};
use crate::tools;
use crate::worktree;
use ferrule_core::message::Role as MsgRole;
use ferrule_core::{
    Agent, Budget, CoreError, HookEvent, HookSet, Inbox, StopFlag, Transcript, Usage,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// What the owner allows a tree of agents; `[agents]` in `ferrule.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// Deepest level an agent may be spawned at; the root is 0.
    pub max_depth: u32,
    /// Children of one parent running at once.
    pub max_children: usize,
    /// Spawned agents per tree that aren't closed.
    pub max_agents: usize,
    /// Tokens a tree's spawned agents may spend in `budget_window_secs`.
    pub max_tokens: u64,
    pub budget_window_secs: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_children: 4,
            max_agents: 12,
            max_tokens: 2_000_000,
            budget_window_secs: 24 * 3600,
        }
    }
}

/// The result a parent sees is cut here (~2k tokens): the summary contract.
pub const RESULT_CAP: usize = 8_000;
/// A notice carries this much of the result's first line.
const NOTICE_CHARS: usize = 200;
const DEFAULT_WAIT_SECS: u64 = 300;
const MAX_WAIT_SECS: u64 = 3600;
/// How long `close_agent` waits for a child to stop at its next step
/// before aborting it.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Worker,
    Planner,
    Verifier,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "worker" => Some(Role::Worker),
            "planner" => Some(Role::Planner),
            "verifier" => Some(Role::Verifier),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Worker => "worker",
            Role::Planner => "planner",
            Role::Verifier => "verifier",
        }
    }
}

/// Everything the embedding program needs to build a child agent. The
/// supervisor then adds the agent tools, the prompt additions, the inbox,
/// the budget and the stop flag, and replays the transcript.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub id: String,
    pub tree: String,
    pub parent: String,
    pub depth: u32,
    pub role: Role,
    /// Where its tools run.
    pub workspace: PathBuf,
    /// Extra roots the sandbox must let it write (a worktree's git dir).
    pub extra_writable: Vec<PathBuf>,
    /// No file writes: the sandbox in read-only mode, no `write_file`.
    pub read_only: bool,
    /// Its session: build the agent on this transcript.
    pub transcript: Transcript,
    /// The model `spawn_agent` named (already checked as connected):
    /// ahead of its role's model.
    pub model: Option<String>,
}

/// Checks a model `spawn_agent` names: its canonical ref, or why it
/// can't run on it (M21: connected models only).
pub type ModelCheck = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

pub type ChildFactory = Arc<dyn Fn(&ChildSpec) -> Result<Agent, String> + Send + Sync>;

/// Runs an idle root with a message, the way an inbound chat message
/// would: the gateway's router, or `ferrule run`'s loop.
pub trait Waker: Send + Sync {
    /// Whether `root` can be woken at all (a scheduled task can't).
    fn can_wake(&self, root: &str) -> bool;
    /// Runs `root` with `text`; false if it couldn't.
    fn wake(&self, root: &str, text: String) -> bool;
}

/// What a spawn produced, for the tool to report.
#[derive(Debug, Clone)]
pub struct Spawned {
    pub id: String,
    pub workspace: PathBuf,
    /// Anything the parent should know about where the child works.
    pub notes: Vec<String>,
    /// The parent is an idle-wakeable root.
    pub wakes_parent: bool,
    pub parent_is_root: bool,
}

pub struct SpawnRequest {
    pub task: String,
    pub name: Option<String>,
    pub role: Role,
    /// Its own worktree when the parent is in a git repo (a verifier gets
    /// a snapshot); false shares the parent's workspace.
    pub worktree: bool,
    /// A connected model to run on (checked with the [`ModelCheck`]).
    pub model: Option<String>,
}

/// What `close_tree` did.
#[derive(Debug, Clone, Default)]
pub struct Closed {
    pub ids: Vec<String>,
    /// Branches kept because they hold work, and the like.
    pub notes: Vec<String>,
}

pub(crate) struct InboxItem {
    pub(crate) text: String,
    /// A notice about this child; dropped once `wait_agent` returned the
    /// child's result, so nothing is said twice.
    pub(crate) about: Option<String>,
    /// Wakes the agent when it's an idle root.
    pub(crate) wakes: bool,
}

#[derive(Default)]
struct InboxState {
    items: Vec<InboxItem>,
    running: bool,
}

/// An agent's inbox: notices about its children and direct messages.
/// Outlives each run, so what arrives between runs waits for the next one.
pub struct AgentInbox {
    agent: String,
    sup: Weak<Supervisor>,
    state: Mutex<InboxState>,
}

impl Inbox for AgentInbox {
    fn begin(&self) {
        self.state.lock().unwrap().running = true;
    }

    fn take(&self) -> Vec<String> {
        let mut st = self.state.lock().unwrap();
        st.items.drain(..).map(|i| i.text).collect()
    }

    fn end(&self) {
        let wake = {
            let mut st = self.state.lock().unwrap();
            st.running = false;
            st.items.iter().any(|i| i.wakes)
        };
        if wake {
            if let Some(sup) = self.sup.upgrade() {
                sup.wake_root(&self.agent);
            }
        }
    }
}

impl AgentInbox {
    /// Adds an item; true when the agent is idle and the item wakes.
    pub(crate) fn push(&self, item: InboxItem) -> bool {
        let mut st = self.state.lock().unwrap();
        let wake = item.wakes && !st.running;
        st.items.push(item);
        wake
    }

    /// Everything queued, if the agent is idle.
    fn take_if_idle(&self) -> Option<Vec<InboxItem>> {
        let mut st = self.state.lock().unwrap();
        (!st.running && !st.items.is_empty()).then(|| std::mem::take(&mut st.items))
    }

    fn put_back(&self, mut items: Vec<InboxItem>) {
        let mut st = self.state.lock().unwrap();
        items.append(&mut st.items);
        st.items = items;
    }

    fn forget_about(&self, child: &str) {
        self.state
            .lock()
            .unwrap()
            .items
            .retain(|i| i.about.as_deref() != Some(child));
    }

    pub fn pending(&self) -> usize {
        self.state.lock().unwrap().items.len()
    }
}

/// Per-agent runtime state in this process.
struct Live {
    inbox: Arc<AgentInbox>,
    stop: StopFlag,
    handle: Option<JoinHandle<()>>,
}

pub struct Supervisor {
    store: AgentStore,
    limits: Limits,
    factory: ChildFactory,
    sessions_dir: PathBuf,
    live: Mutex<HashMap<String, Live>>,
    /// Limit checks and the insert that follows them happen under this, so
    /// two spawns at once can't both take the last slot.
    spawn_lock: Mutex<()>,
    /// Bumped whenever an agent's status changes; `wait_agent` watches it.
    tick: watch::Sender<u64>,
    waker: RwLock<Option<Arc<dyn Waker>>>,
    /// Where children's worktrees go; none, and every child shares its
    /// parent's workspace.
    worktrees: RwLock<Option<PathBuf>>,
    /// Which models a child may be asked to run on; none, and a spawn
    /// that names one is refused.
    models: RwLock<Option<ModelCheck>>,
    /// The root's hooks: children inherit PreToolUse/PostToolUse, and
    /// SubagentStart/Stop fire around their runs (M18).
    hooks: RwLock<HookSet>,
    /// Runs that have stopped and whose notice is still being delivered.
    finishing: AtomicUsize,
    me: Weak<Supervisor>,
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Supervisor {
    /// This process claims the agents it will run. Anything the store says
    /// is running in a process that's gone was interrupted by a restart:
    /// it's marked so, and nothing resumes on its own. Agents another live
    /// ferrule process runs are left to it.
    pub fn new(
        store: AgentStore,
        sessions_dir: impl Into<PathBuf>,
        limits: Limits,
        factory: ChildFactory,
    ) -> Result<Arc<Self>, AgentsError> {
        store.claim_process()?;
        let interrupted = store.mark_interrupted(now())?;
        if !interrupted.is_empty() {
            info!(count = interrupted.len(), "agents interrupted by a restart");
        }
        Ok(Arc::new_cyclic(|me| Supervisor {
            store,
            limits,
            factory,
            sessions_dir: sessions_dir.into(),
            live: Mutex::new(HashMap::new()),
            spawn_lock: Mutex::new(()),
            tick: watch::channel(0).0,
            waker: RwLock::new(None),
            worktrees: RwLock::new(None),
            models: RwLock::new(None),
            hooks: RwLock::new(HookSet::default()),
            finishing: AtomicUsize::new(0),
            me: me.clone(),
        }))
    }

    /// How a model named in `spawn_agent` is checked (M21).
    pub fn set_model_check(&self, check: ModelCheck) {
        *self.models.write().unwrap() = Some(check);
    }

    pub fn set_waker(&self, waker: Arc<dyn Waker>) {
        *self.waker.write().unwrap() = Some(waker);
    }

    /// Lets children on their parent's repo work in worktrees under `dir`
    /// (in the data dir, outside any repo).
    pub fn set_worktrees_dir(&self, dir: impl Into<PathBuf>) {
        *self.worktrees.write().unwrap() = Some(dir.into());
    }

    pub fn store(&self) -> &AgentStore {
        &self.store
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn arc(&self) -> Arc<Supervisor> {
        self.me.upgrade().expect("supervisor alive")
    }

    pub(crate) fn inbox(&self, id: &str) -> Arc<AgentInbox> {
        self.live_entry(id, |l| l.inbox.clone())
    }

    fn stop_flag(&self, id: &str) -> StopFlag {
        self.live_entry(id, |l| l.stop.clone())
    }

    fn live_entry<T>(&self, id: &str, f: impl FnOnce(&mut Live) -> T) -> T {
        let mut live = self.live.lock().unwrap();
        let entry = live.entry(id.to_string()).or_insert_with(|| Live {
            inbox: Arc::new(AgentInbox {
                agent: id.to_string(),
                sup: self.me.clone(),
                state: Mutex::new(InboxState::default()),
            }),
            stop: StopFlag::new(),
            handle: None,
        });
        f(entry)
    }

    /// Makes `agent` the root of a tree (its id is its session id): gives
    /// it the agent tools, the prompt addition and its inbox. The root's
    /// own spending isn't charged to the tree's budget. Its hooks become
    /// the ones every child's run is wrapped in (M18, [`crate::lifecycle`]):
    /// one process has one config and workspace, so every root's are the
    /// same.
    pub fn attach_root(
        &self,
        agent: Agent,
        root: &str,
        workspace: &Path,
    ) -> Result<Agent, AgentsError> {
        let row = match self.store.get(root)? {
            Some(row) => row,
            None => {
                let t = now();
                let row = AgentRow {
                    id: root.to_string(),
                    tree: root.to_string(),
                    parent: None,
                    depth: 0,
                    name: None,
                    role: "root".into(),
                    task: String::new(),
                    session: root.to_string(),
                    workspace: workspace.to_path_buf(),
                    worktree: None,
                    branch: None,
                    base: None,
                    status: Status::Idle,
                    result: None,
                    tokens: 0,
                    created_at: t,
                    updated_at: t,
                    model: None,
                };
                self.store.insert(&row)?;
                row
            }
        };
        if row.parent.is_some() {
            return Err(AgentsError::Invalid(format!(
                "{root} is a spawned agent, not a root"
            )));
        }
        *self.hooks.write().unwrap() = agent.hooks().clone();
        Ok(self.equip(agent, &row))
    }

    /// Tools, prompt and inbox for `row`'s agent.
    fn equip(&self, mut agent: Agent, row: &AgentRow) -> Agent {
        for tool in tools::for_agent(self.arc(), row, &self.limits) {
            agent.register_tool(tool);
        }
        agent.append_system_prompt(prompts::DATA_NOT_INSTRUCTIONS);
        agent.with_inbox(self.inbox(&row.id))
    }

    pub fn budget_exhausted(&self, tree: &str) -> Option<String> {
        let since = now() - self.limits.budget_window_secs;
        let spent = match self.store.spent_since(tree, since) {
            Ok(n) => n,
            Err(e) => {
                warn!("reading the agents' spend failed: {e}");
                return None;
            }
        };
        (spent >= self.limits.max_tokens).then(|| {
            format!(
                "the agents in this tree have used {spent} tokens, their budget is {} per {} hours",
                self.limits.max_tokens,
                self.limits.budget_window_secs / 3600
            )
        })
    }

    pub(crate) fn get(&self, id: &str) -> Result<AgentRow, AgentsError> {
        self.store
            .get(id)?
            .ok_or_else(|| AgentsError::Invalid(format!("there is no agent {id}")))
    }

    /// `id`, if `caller` spawned it.
    fn child_of(&self, caller: &str, id: &str) -> Result<AgentRow, AgentsError> {
        let row = self.get(id)?;
        if row.parent.as_deref() != Some(caller) {
            return Err(AgentsError::Invalid(format!(
                "agent {id} isn't one you started; list_agents shows yours"
            )));
        }
        Ok(row)
    }

    fn check_can_run(&self, parent: &AgentRow) -> Result<(), AgentsError> {
        let running = self.store.running_children(&parent.id)?;
        if running >= self.limits.max_children {
            return Err(AgentsError::Limit(format!(
                "you already have {running} agents running, the limit is {} at once: wait_agent for one \
                 to finish or close_agent one first",
                self.limits.max_children
            )));
        }
        if let Some(why) = self.budget_exhausted(&parent.tree) {
            return Err(AgentsError::Limit(format!(
                "no more agents can run: {why}. Finish the work yourself or report what's left"
            )));
        }
        Ok(())
    }

    /// Starts a child of `caller` on `req.task`.
    pub fn spawn(&self, caller: &str, req: SpawnRequest) -> Result<Spawned, AgentsError> {
        let parent = self.get(caller)?;
        let model = match req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            None => None,
            Some(word) => {
                let check = self.models.read().unwrap().clone();
                let Some(check) = check else {
                    return Err(AgentsError::Invalid(format!(
                        "can't run an agent on `{word}`: this process picks no models per agent; leave `model` out"
                    )));
                };
                Some(check(word).map_err(|why| {
                    AgentsError::Invalid(format!(
                        "can't run an agent on `{word}`: {why}. Leave `model` out to use its role's or yours"
                    ))
                })?)
            }
        };
        let row = {
            let _guard = self.spawn_lock.lock().unwrap();
            let depth = parent.depth + 1;
            if depth > self.limits.max_depth {
                return Err(AgentsError::Limit(format!(
                    "agents can't be nested deeper than {} levels; do this yourself",
                    self.limits.max_depth
                )));
            }
            self.check_can_run(&parent)?;
            let open = self.store.open_in_tree(&parent.tree)?;
            if open >= self.limits.max_agents {
                return Err(AgentsError::Limit(format!(
                    "this tree already has {open} open agents, the limit is {}: close_agent the ones \
                     you are done with",
                    self.limits.max_agents
                )));
            }
            let id = format!("a-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let t = now();
            let row = AgentRow {
                session: format!("agent-{id}"),
                id,
                tree: parent.tree.clone(),
                parent: Some(parent.id.clone()),
                depth,
                name: req.name.filter(|n| !n.trim().is_empty()),
                role: req.role.as_str().into(),
                task: req.task.clone(),
                workspace: parent.workspace.clone(),
                worktree: None,
                branch: None,
                base: None,
                status: Status::Running,
                result: None,
                tokens: 0,
                created_at: t,
                updated_at: t,
                model,
            };
            self.store.insert(&row)?;
            row
        };
        let mut row = row;
        let notes = self.place(&mut row, &parent, req.worktree);
        if let Err(e) = self.start(&row, &req.task) {
            let _ = self
                .store
                .finish(&row.id, Status::Failed, &e.to_string(), now());
            self.bump();
            return Err(e);
        }
        let parent_is_root = parent.parent.is_none();
        Ok(Spawned {
            id: row.id,
            workspace: row.workspace,
            notes,
            wakes_parent: parent_is_root && self.can_wake(&parent.id),
            parent_is_root,
        })
    }

    /// Gives `row` its own copy of `parent`'s repo when there is one: a
    /// worktree for a worker or planner, a snapshot for a verifier. Returns
    /// what the parent should know about it.
    fn place(&self, row: &mut AgentRow, parent: &AgentRow, want: bool) -> Vec<String> {
        let Some(root) = self.worktrees.read().unwrap().clone() else {
            return Vec::new();
        };
        let made = match Role::parse(&row.role) {
            _ if !want => return Vec::new(),
            Some(Role::Verifier) => worktree::snapshot(&parent.workspace, &root, &row.id),
            _ => worktree::for_worker(&parent.workspace, &root, &row.id),
        };
        let made = match made {
            Ok(m) => m,
            Err(note) => return note.into_iter().collect(),
        };
        if let Err(e) = self.store.set_worktree(
            &row.id,
            &made.workspace,
            &made.worktree,
            made.branch.as_deref(),
            &made.base,
        ) {
            worktree::discard(&parent.workspace, &made.worktree);
            if let Some(b) = &made.branch {
                let _ = worktree::git(&parent.workspace, &["branch", "-q", "-D", b]);
            }
            return vec![format!("It shares your workspace: {e}.")];
        }
        row.workspace = made.workspace;
        row.worktree = Some(made.worktree);
        row.branch = made.branch;
        row.base = Some(made.base);
        made.notes
    }

    fn parent_workspace(&self, row: &AgentRow) -> Option<PathBuf> {
        let parent = row.parent.as_deref()?;
        Some(self.store.get(parent).ok()??.workspace)
    }

    fn can_wake(&self, root: &str) -> bool {
        self.waker
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|w| w.can_wake(root))
    }

    /// Builds the agent for `row` from its transcript and runs it on
    /// `message` in the background. The row must already say `running`.
    fn start(&self, row: &AgentRow, message: &str) -> Result<(), AgentsError> {
        let role = Role::parse(&row.role).unwrap_or(Role::Worker);
        let transcript = Transcript::create(&self.sessions_dir, &row.session)?;
        let history = transcript.read_messages().unwrap_or_default();
        let spec = ChildSpec {
            id: row.id.clone(),
            tree: row.tree.clone(),
            parent: row.parent.clone().unwrap_or_default(),
            depth: row.depth,
            role,
            workspace: row.workspace.clone(),
            extra_writable: match (&row.worktree, &row.branch, self.parent_workspace(row)) {
                (Some(wt), Some(_), Some(pw)) => {
                    worktree::writable_git_dir(wt, &pw).into_iter().collect()
                }
                _ => Vec::new(),
            },
            // A verifier without a snapshot works in its parent's files.
            read_only: role == Role::Verifier && row.worktree.is_none(),
            transcript,
            model: row.model.clone(),
        };
        let agent = (self.factory)(&spec).map_err(AgentsError::Build)?;
        let mut agent = self.equip(agent, row);
        agent.append_system_prompt(&prompts::child_prompt(
            &row.id,
            row.parent.as_deref().unwrap_or(""),
            role,
        ));
        // Like a gateway lane: the factory wrote a fresh system prompt;
        // only the conversation is replayed on top of it.
        agent
            .messages
            .extend(history.into_iter().filter(|m| m.role != MsgRole::System));
        let hooks = self.hooks.read().unwrap().clone();
        let parent = row
            .parent
            .as_deref()
            .and_then(|p| self.store.get(p).ok().flatten());
        let child = lifecycle::ChildRun {
            id: row.id.clone(),
            role: role.as_str().to_string(),
            task: row.task.clone(),
            parent_session: parent
                .as_ref()
                .map(|p| p.session.clone())
                .unwrap_or_default(),
            cwd: parent
                .map(|p| p.workspace)
                .unwrap_or_else(|| row.workspace.clone()),
        };
        if !hooks.is_empty() {
            agent.add_hooks(hooks.for_child(&row.id, &child.parent_session));
        }
        let hooks = hooks.only(&[HookEvent::SubagentStart, HookEvent::SubagentStop]);
        let stop = self.stop_flag(&row.id);
        stop.reset();
        let agent = agent
            .with_budget(Arc::new(TreeBudget {
                sup: self.me.clone(),
                tree: row.tree.clone(),
                agent: row.id.clone(),
            }))
            .with_stop_flag(stop);

        let sup = self.arc();
        let id = row.id.clone();
        let message = message.to_string();
        // The handle goes into the map before the task can finish and
        // clear it.
        let mut live = self.live.lock().unwrap();
        let handle = tokio::spawn(async move {
            let mut agent = agent;
            let result = lifecycle::run_child(&mut agent, &hooks, &child, message).await;
            sup.finished(&id, result);
        });
        drop(live.get_mut(&row.id).map(|l| l.handle.replace(handle)));
        Ok(())
    }

    fn finished(&self, id: &str, result: Result<String, CoreError>) {
        self.finishing.fetch_add(1, Ordering::SeqCst);
        self.report(id, result);
        self.finishing.fetch_sub(1, Ordering::SeqCst);
        self.bump();
    }

    /// Records a run's end and tells the parent.
    fn report(&self, id: &str, result: Result<String, CoreError>) {
        if let Some(l) = self.live.lock().unwrap().get_mut(id) {
            l.handle = None;
        }
        let (status, text) = match result {
            Ok(answer) => (Status::Idle, answer),
            Err(e) => (Status::Failed, format!("failed: {e}")),
        };
        let recorded = match self.store.finish(id, status, &text, now()) {
            Ok(r) => r,
            Err(e) => {
                warn!(agent = id, "recording an agent's result failed: {e}");
                false
            }
        };
        if status == Status::Failed {
            if let Err(e) = self.store.release_claims(id, now()) {
                warn!(agent = id, "releasing a failed agent's tasks failed: {e}");
            }
        }
        // A closed agent's parent asked for it to stop; no notice, and
        // close tidies its worktree.
        if !recorded {
            return;
        }
        let Ok(row) = self.get(id) else {
            return;
        };
        // A verifier's snapshot is thrown away as soon as it's done.
        if let (Some(wt), None, Some(pw)) =
            (&row.worktree, &row.branch, self.parent_workspace(&row))
        {
            worktree::discard(&pw, wt);
        }
        let Some(parent) = row.parent.clone() else {
            return;
        };
        let parent_is_root = self
            .store
            .get(&parent)
            .ok()
            .flatten()
            .is_some_and(|p| p.parent.is_none());
        let label = match &row.name {
            Some(n) => format!("{} ({n})", row.id),
            None => row.id.clone(),
        };
        let verb = if status == Status::Failed {
            "failed"
        } else {
            "finished"
        };
        let text = fence(
            "agent_notice",
            &[
                ("agent", Some(&row.id)),
                ("name", row.name.as_deref()),
                ("status", Some(status.as_str())),
                ("untrusted", Some("true")),
            ],
            &format!(
                "Agent {label} {verb}: {}\nCall wait_agent with its id for the full report.",
                first_line(&text, NOTICE_CHARS)
            ),
        );
        let wake = self.inbox(&parent).push(InboxItem {
            text,
            about: Some(row.id.clone()),
            wakes: parent_is_root,
        });
        if wake {
            self.wake_root(&parent);
        }
    }

    /// Runs an idle root on whatever is in its inbox.
    fn wake_root(&self, root: &str) {
        let Some(waker) = self.waker.read().unwrap().clone() else {
            return;
        };
        if !waker.can_wake(root) {
            return;
        }
        let inbox = self.inbox(root);
        let Some(items) = inbox.take_if_idle() else {
            return;
        };
        let text = items
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let text = format!(
            "[ferrule] News from the agents you started. Act on it if it completes what you were asked; \
             otherwise answer briefly or wait for the rest.\n\n{text}"
        );
        if !waker.wake(root, text) {
            inbox.put_back(items);
        }
    }

    fn bump(&self) {
        self.tick.send_modify(|n| *n += 1);
    }

    /// Waits until one of `ids` isn't running (or the timeout), then
    /// reports on all of them.
    pub async fn wait(
        &self,
        caller: &str,
        ids: &[String],
        timeout_secs: Option<u64>,
    ) -> Result<String, AgentsError> {
        if ids.is_empty() {
            return Err(AgentsError::Invalid("give at least one agent id".into()));
        }
        for id in ids {
            self.child_of(caller, id)?;
        }
        let timeout =
            Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut tick = self.tick.subscribe();
        let caller_stop = self.stop_flag(caller);
        let rows = loop {
            let rows: Vec<AgentRow> = ids
                .iter()
                .map(|id| self.get(id))
                .collect::<Result<_, _>>()?;
            // A child that just stopped has its notice delivered first, so
            // the notice is dropped below rather than arriving after this.
            if rows.iter().any(|r| r.status != Status::Running)
                && self.finishing.load(Ordering::SeqCst) == 0
            {
                break rows;
            }
            if caller_stop.is_set() {
                return Err(AgentsError::Core(CoreError::Aborted(
                    "the agent was stopped".into(),
                )));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break rows;
            }
            let step = (deadline - now).min(Duration::from_secs(1));
            let _ = tokio::time::timeout(step, tick.changed()).await;
        };
        let done = rows.iter().filter(|r| r.status != Status::Running).count();
        let mut out = if done == 0 {
            format!(
                "None finished within {} s; all still running.",
                timeout.as_secs()
            )
        } else {
            format!("{done} of {} no longer running.", rows.len())
        };
        let inbox = self.inbox(caller);
        for r in &rows {
            out.push_str("\n\n");
            if r.status == Status::Running {
                out.push_str(&format!("Agent {} is still running.", r.id));
                continue;
            }
            inbox.forget_about(&r.id);
            out.push_str(&result_fence(r));
        }
        Ok(out)
    }

    /// Gives an idle, failed or interrupted child another instruction.
    pub fn resume(&self, caller: &str, id: &str, message: &str) -> Result<(), AgentsError> {
        let mut row = self.child_of(caller, id)?;
        {
            let _guard = self.spawn_lock.lock().unwrap();
            match row.status {
                Status::Running => {
                    return Err(AgentsError::Invalid(format!(
                        "agent {id} is still running; wait_agent for it first"
                    )))
                }
                Status::Closed => {
                    return Err(AgentsError::Invalid(format!(
                        "agent {id} was closed and can't be resumed; spawn a new one"
                    )))
                }
                _ => {}
            }
            let parent = self.get(caller)?;
            self.check_can_run(&parent)?;
            self.store.set_status(id, Status::Running, now())?;
        }
        self.bump();
        // A verifier checks the parent's work as it is now.
        if row.worktree.is_some() && row.branch.is_none() {
            let parent = self.get(caller)?;
            row.worktree = None;
            self.place(&mut row, &parent, true);
            if row.worktree.is_none() {
                row.workspace = parent.workspace.clone();
                self.store.clear_worktree(id, &row.workspace)?;
            }
        }
        if let Err(e) = self.start(&row, message) {
            let _ = self.store.finish(id, Status::Failed, &e.to_string(), now());
            self.bump();
            return Err(e);
        }
        Ok(())
    }

    /// Closes `caller`'s child `id` and everything it started.
    pub async fn close(&self, caller: &str, id: &str) -> Result<String, AgentsError> {
        self.child_of(caller, id)?;
        let closed = self.close_tree(id).await?;
        if closed.ids.is_empty() {
            return Ok(format!("Agent {id} was already closed."));
        }
        let mut out = format!("Closed {}.", closed.ids.join(", "));
        for note in closed.notes {
            out.push('\n');
            out.push_str(&note);
        }
        Ok(out)
    }

    /// Closes `id` and its descendants. Owner-side (`ferrule agents
    /// close`) as well as the tool's. Every agent is told to stop first, so
    /// they wind down together; any still going after the grace period is
    /// aborted. Then each worktree is tidied, deepest first: a worker's
    /// uncommitted work is committed to its branch, and the branch kept
    /// only if it holds work.
    pub async fn close_tree(&self, id: &str) -> Result<Closed, AgentsError> {
        let mut order = Vec::new();
        let mut stack = vec![id.to_string()];
        while let Some(next) = stack.pop() {
            for c in self.store.children(&next)? {
                stack.push(c.id);
            }
            order.push(next);
        }
        let mut closed = Vec::new();
        let mut handles = Vec::new();
        let mut tidy = Vec::new();
        for agent in order {
            let row = self.get(&agent)?;
            if row.status == Status::Closed {
                continue;
            }
            self.store.set_status(&agent, Status::Closed, now())?;
            self.store.release_claims(&agent, now())?;
            let handle = self.live_entry(&agent, |l| {
                l.stop.stop();
                l.handle.take()
            });
            handles.extend(handle.map(|h| (agent.clone(), h)));
            if let Some(parent) = &row.parent {
                self.inbox(parent).forget_about(&agent);
            }
            if let (Some(_), Some(pw)) = (&row.worktree, self.parent_workspace(&row)) {
                tidy.push((row, pw));
            }
            closed.push(agent);
        }
        let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
        for (agent, mut handle) in handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                warn!(agent = %agent, "the agent didn't stop in time, aborting it");
                handle.abort();
            }
        }
        let notes = tokio::task::spawn_blocking(move || {
            tidy.into_iter()
                .rev()
                .filter_map(|(row, pw)| {
                    let wt = row.worktree.as_deref()?;
                    match &row.branch {
                        Some(b) => worktree::close(
                            &pw,
                            wt,
                            b,
                            row.base.as_deref().unwrap_or("HEAD"),
                            &row.id,
                        ),
                        None => {
                            worktree::discard(&pw, wt);
                            None
                        }
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        self.bump();
        Ok(Closed { ids: closed, notes })
    }

    /// The caller's children, one line each.
    pub fn list(&self, caller: &str) -> Result<String, AgentsError> {
        let rows = self.store.children(caller)?;
        if rows.is_empty() {
            return Ok("You haven't started any agents.".into());
        }
        Ok(rows
            .iter()
            .map(|r| {
                let name = r
                    .name
                    .as_deref()
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                let last = r
                    .result
                    .as_deref()
                    .map(|t| format!(" — {}", first_line(t, 120)))
                    .unwrap_or_default();
                format!(
                    "{}{name} [{}] {}, {} tokens{last}",
                    r.id, r.role, r.status, r.tokens
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Whether any agent in `tree` is running, or one (anywhere) has just
    /// stopped and its notice isn't delivered yet. Once this is false,
    /// every wake-up for the tree's root has been sent.
    pub fn busy(&self, tree: &str) -> bool {
        self.finishing.load(Ordering::SeqCst) > 0
            || self
                .store
                .tree(tree)
                .map(|rows| rows.iter().any(|r| r.status == Status::Running))
                .unwrap_or(false)
    }

    /// Items waiting in `id`'s inbox.
    pub fn pending(&self, id: &str) -> usize {
        self.inbox(id).pending()
    }

    /// Waits for the next status change anywhere, or `timeout`.
    pub async fn changed(&self, timeout: Duration) {
        let mut tick = self.tick.subscribe();
        let _ = tokio::time::timeout(timeout, tick.changed()).await;
    }
}

/// A finished child's report, fenced and capped.
pub(crate) fn result_fence(r: &AgentRow) -> String {
    let body = r.result.as_deref().unwrap_or("(no report)");
    fence(
        "agent_result",
        &[
            ("agent", Some(&r.id)),
            ("name", r.name.as_deref()),
            ("role", Some(&r.role)),
            ("status", Some(r.status.as_str())),
            ("untrusted", Some("true")),
        ],
        &cap(body, RESULT_CAP),
    )
}

/// Charges a child's provider calls to its tree.
struct TreeBudget {
    sup: Weak<Supervisor>,
    tree: String,
    agent: String,
}

impl Budget for TreeBudget {
    fn charge(&self, usage: &Usage) {
        let Some(sup) = self.sup.upgrade() else {
            return;
        };
        let tokens = usage.input_tokens + usage.output_tokens;
        if let Err(e) = sup.store.charge(&self.tree, &self.agent, tokens, now()) {
            warn!("charging an agent's tokens failed: {e}");
        }
    }

    fn exhausted(&self) -> Option<String> {
        self.sup.upgrade()?.budget_exhausted(&self.tree)
    }
}
