//! Declarative graph routing over the sub-agent machinery (M42 part 6,
//! `docs/m42-harness-engineering.md` §6). A graph file names nodes (work)
//! and edges (routing); the runner — deterministic Rust, never a model —
//! walks it: fan-out in parallel, fan-in on every input, a fail edge back
//! to an earlier node is a rollback, an approval node parks for the owner.
//!
//! The orchestration tax is deliberate: one node and one check stay
//! exactly `verify_command`'s job, and a graph is opt-in per task.

use anyhow::{bail, Context, Result};
use ferrule_agents::{AgentRow, Role, SpawnRequest, Status, Supervisor};
use ferrule_core::tool::ToolContext;
use ferrule_core::verify::Verifier;
use ferrule_sandbox::Sandbox;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A parsed graph file.
#[derive(Debug, Deserialize)]
pub struct GraphFile {
    /// What the whole run works toward; `--goal` on the command line wins.
    pub goal: Option<String>,
    /// Cap on node executions, all kinds counted. Default 24.
    pub max_steps: Option<usize>,
    /// The node whose pass means the graph succeeded. Default: every
    /// terminal node (no outbound edges) passed.
    pub succeed_when: Option<String>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

#[derive(Debug, Deserialize)]
pub struct Node {
    pub id: String,
    /// `agent` (default), `check` or `approval`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// worker (default), planner or verifier; agent nodes only.
    pub role: Option<String>,
    /// What the agent does; `{{goal}}`, `{{prev}}` and `{{attempt}}` are
    /// substituted. Agent nodes only.
    pub task: Option<String>,
    /// The judge, run by ferrule itself in the sandbox: exit 0 is a pass.
    /// Check nodes only.
    pub command: Option<String>,
    /// What the owner is asked; same substitutions as `task`.
    /// Approval nodes only.
    pub message: Option<String>,
    /// Re-runs on a rollback edge stop here. Default 3.
    pub max_attempts: Option<usize>,
    /// A connected model for this node's agent (`spawn_agent`'s rules).
    pub model: Option<String>,
    /// How long a node's agent may run before it's closed and failed.
    /// Default 1800 s.
    pub timeout_secs: Option<u64>,
}

fn default_kind() -> String {
    "agent".into()
}

#[derive(Debug, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// `pass`, `fail` or `always` (default).
    pub on: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    Pass,
    Fail,
    Always,
}

impl On {
    fn matches(self, verdict: Verdict) -> bool {
        match self {
            On::Always => true,
            On::Pass => verdict == Verdict::Pass,
            On::Fail => verdict == Verdict::Fail,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
        }
    }
}

#[derive(Debug)]
struct RuntimeEdge {
    from: usize,
    to: usize,
    on: On,
    /// Closes a loop: it doesn't gate its target's first run, it re-fires
    /// it — the rollback edge.
    feedback: bool,
}

struct Outcome {
    verdict: Verdict,
    summary: String,
}

/// A graph validated and indexed, ready to run.
#[derive(Debug)]
pub struct Graph {
    goal: Option<String>,
    max_steps: usize,
    succeed_when: Option<usize>,
    nodes: Vec<Node>,
    edges: Vec<RuntimeEdge>,
}

pub fn parse(text: &str) -> Result<Graph> {
    let file: GraphFile = toml::from_str(text).context("the graph file doesn't parse")?;
    validate(file)
}

fn validate(file: GraphFile) -> Result<Graph> {
    if file.nodes.is_empty() {
        bail!("the graph has no [[nodes]]");
    }
    let mut ids: HashMap<&str, usize> = HashMap::new();
    for (i, n) in file.nodes.iter().enumerate() {
        if n.id.trim().is_empty() {
            bail!("node #{i} has an empty id");
        }
        if ids.insert(n.id.as_str(), i).is_some() {
            bail!("two nodes are called {:?}", n.id);
        }
        match n.kind.as_str() {
            "agent" => {
                if n.task.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    bail!("agent node {:?} needs a task", n.id);
                }
                if let Some(role) = &n.role {
                    if Role::parse(role).is_none() {
                        bail!(
                            "node {:?}: no such role {role:?}; the roles are worker, planner and verifier",
                            n.id
                        );
                    }
                }
            }
            "check" => {
                if n.command.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    bail!("check node {:?} needs a command", n.id);
                }
            }
            "approval" => {}
            other => bail!(
                "node {:?}: unknown kind {other:?} (agent, check or approval)",
                n.id
            ),
        }
    }
    let mut edges = Vec::with_capacity(file.edges.len());
    for e in &file.edges {
        let (Some(&from), Some(&to)) = (ids.get(e.from.as_str()), ids.get(e.to.as_str())) else {
            bail!(
                "an edge names a node that doesn't exist: {:?} -> {:?}",
                e.from,
                e.to
            );
        };
        let on = match e.on.as_deref().unwrap_or("always") {
            "pass" => On::Pass,
            "fail" => On::Fail,
            "always" => On::Always,
            other => bail!(
                "edge {:?} -> {:?}: unknown `on` {other:?} (pass, fail or always)",
                e.from,
                e.to
            ),
        };
        edges.push(RuntimeEdge {
            from,
            to,
            on,
            feedback: false,
        });
    }
    // Feedback arcs, greedily in declaration order: an edge is feedback
    // when its target already reaches its source through the feedforward
    // graph built so far — removing the feedback arcs leaves a DAG, and
    // a rollback edge declared after its forward edge reads as the loop.
    let mut dag: Vec<(usize, usize)> = Vec::new();
    for e in edges.iter_mut() {
        e.feedback = reaches(&dag, e.to, e.from);
        if !e.feedback {
            dag.push((e.from, e.to));
        }
    }
    let start = file
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| !edges.iter().any(|e| e.to == *i && !e.feedback))
        .count();
    if start == 0 {
        bail!("the graph has no start node (every node is gated by another)");
    }
    let succeed_when = file
        .succeed_when
        .as_deref()
        .map(|id| {
            ids.get(id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("succeed_when names no node: {id:?}"))
        })
        .transpose()?;
    Ok(Graph {
        goal: file
            .goal
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty()),
        max_steps: file.max_steps.unwrap_or(24).max(1),
        succeed_when,
        nodes: file.nodes,
        edges,
    })
}

/// Can `from` reach `want` through the feedforward edges in `dag`?
fn reaches(dag: &[(usize, usize)], from: usize, want: usize) -> bool {
    let mut stack = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = stack.pop() {
        if n == want {
            return true;
        }
        if !seen.insert(n) {
            continue;
        }
        for e in dag {
            if e.0 == n {
                stack.push(e.1);
            }
        }
    }
    false
}

/// What one node's run came to, shown per line and kept for `{{prev}}`.
struct Exec<'a> {
    sup: &'a Arc<Supervisor>,
    root: &'a str,
    nodes: &'a [Node],
    workspace: &'a Path,
    sandbox: Arc<Sandbox>,
    check_timeout: Duration,
    auto_approve: bool,
    /// The owner's chat answers approvals (the gateway); None: the
    /// terminal, or `--yes`.
    hub: Option<&'a Arc<ferrule_trust::Hub>>,
}

impl Exec<'_> {
    fn timeout_of(&self, node: &Node) -> u64 {
        node.timeout_secs.unwrap_or(1800).clamp(1, 3600)
    }

    fn render(&self, text: &str, goal: &str, prev: &str, attempt: usize) -> String {
        text.replace("{{goal}}", goal)
            .replace("{{prev}}", prev)
            .replace("{{attempt}}", &attempt.to_string())
    }

    /// Spawns a node's agent in the background; the child id, or a failed
    /// outcome when it couldn't start.
    fn spawn_agent(&self, node: &Node, task: String) -> Result<String, Outcome> {
        let req = SpawnRequest {
            task,
            name: Some(node.id.clone()),
            role: node
                .role
                .as_deref()
                .and_then(Role::parse)
                .unwrap_or(Role::Worker),
            worktree: true,
            model: node.model.clone(),
        };
        self.sup
            .spawn(self.root, req)
            .map(|s| s.id)
            .map_err(|e| Outcome {
                verdict: Verdict::Fail,
                summary: format!("couldn't start the agent: {e}"),
            })
    }

    /// Waits for a batch of agents — all of them, capped at the longest
    /// node's timeout; a straggler is closed and counted failed.
    async fn wait_agents(&self, jobs: Vec<(usize, String)>) -> Vec<(usize, Outcome)> {
        let timeout = jobs
            .iter()
            .map(|(i, _)| self.timeout_of(&self.nodes[*i]))
            .max()
            .unwrap_or(1800);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
        // `wait` returns when any one finishes; a fan-in needs them all.
        loop {
            let any_running = jobs.iter().any(|(_, id)| {
                self.sup
                    .store()
                    .get(id)
                    .ok()
                    .flatten()
                    .is_none_or(|r| r.status == Status::Running)
            });
            if !any_running || tokio::time::Instant::now() >= deadline {
                break;
            }
            let step = (deadline - tokio::time::Instant::now()).min(Duration::from_secs(1));
            self.sup.changed(step).await;
        }
        let mut out = Vec::with_capacity(jobs.len());
        for (node_i, id) in jobs {
            let node = &self.nodes[node_i];
            let row = self.sup.store().get(&id).ok().flatten();
            let (status, result) = match &row {
                Some(r) => (r.status, r.result.clone().unwrap_or_default()),
                None => (Status::Failed, String::new()),
            };
            if status == Status::Running {
                let _ = self.sup.close(self.root, &id).await;
                out.push((
                    node_i,
                    Outcome {
                        verdict: Verdict::Fail,
                        summary: format!("still running after {} s; closed", self.timeout_of(node)),
                    },
                ));
                continue;
            }
            let is_verifier = node.role.as_deref() == Some("verifier");
            let verdict = match (status, is_verifier) {
                (Status::Idle, false) => Verdict::Pass,
                (Status::Idle, true) => verdict_of(&result),
                _ => Verdict::Fail,
            };
            out.push((
                node_i,
                Outcome {
                    verdict,
                    summary: if result.is_empty() {
                        format!("(agent ended {status} with no report)")
                    } else {
                        result
                    },
                },
            ));
        }
        out
    }

    async fn check(&self, node: &Node) -> Outcome {
        let command = node.command.clone().unwrap_or_default();
        let verifier =
            ferrule_tools::CommandVerifier::new(command, self.sandbox.clone(), self.check_timeout);
        let ctx = ToolContext {
            workspace: self.workspace.to_path_buf(),
            max_output_chars: 4_000,
        };
        match verifier.verify(&ctx).await {
            Ok(()) => Outcome {
                verdict: Verdict::Pass,
                summary: "exit 0".into(),
            },
            Err(output) => Outcome {
                verdict: Verdict::Fail,
                summary: output,
            },
        }
    }

    /// An approval node: the owner's chat when a hub is wired (M43), else
    /// `--yes`, else a terminal question.
    async fn approval(&self, node: &Node, message: String) -> Outcome {
        if let Some(hub) = self.hub {
            let timeout = Duration::from_secs(hub.config().approval_timeout_secs);
            return match hub
                .ask_owner(
                    self.root,
                    &format!("graph node {:?}", node.id),
                    &message,
                    timeout,
                )
                .await
            {
                Ok(()) => Outcome {
                    verdict: Verdict::Pass,
                    summary: "approved in the owner's chat".into(),
                },
                Err(why) => Outcome {
                    verdict: Verdict::Fail,
                    summary: why,
                },
            };
        }
        if self.auto_approve {
            return Outcome {
                verdict: Verdict::Pass,
                summary: "approved with --yes".into(),
            };
        }
        if !std::io::stdin().is_terminal() {
            return Outcome {
                verdict: Verdict::Fail,
                summary: "no terminal to approve at (pass --yes to approve automatically)".into(),
            };
        }
        print!("\x1b[1;33mapprove?\x1b[0m {message} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let yes = std::io::stdin().read_line(&mut line).is_ok()
            && matches!(line.trim().to_lowercase().as_str(), "y" | "yes");
        Outcome {
            verdict: if yes { Verdict::Pass } else { Verdict::Fail },
            summary: if yes { "approved" } else { "denied" }.into(),
        }
    }
}

/// A verifier agent's own last word; anything unclear fails closed.
fn verdict_of(result: &str) -> Verdict {
    for line in result.lines().rev().map(str::trim) {
        if line.eq_ignore_ascii_case("verdict: pass") {
            return Verdict::Pass;
        }
        if line.eq_ignore_ascii_case("verdict: fail") {
            return Verdict::Fail;
        }
    }
    Verdict::Fail
}

/// What an inbound edge's source last came to, for `{{prev}}`.
const PREV_PER_NODE: usize = 1_500;

/// Everything a graph run needs, beyond the file.
pub struct RunOpts {
    /// Wins over the file's `goal`.
    pub goal: Option<String>,
    /// The model the graph's agents run on when a node names none (a
    /// graph built on the process's shared supervisor ignores this: the
    /// role models and the default rule there).
    pub model: Option<String>,
    pub workspace: PathBuf,
    pub max_iterations: usize,
    /// Wins over the file's `max_steps`.
    pub max_steps: Option<usize>,
    /// Approve every approval node without asking (a hub, when wired,
    /// asks first).
    pub auto_approve: bool,
    /// The process's shared supervisor (the gateway's): reuse it instead
    /// of claiming the agents store with a new one.
    pub supervisor: Option<Arc<Supervisor>>,
    /// Approvals asked in the owner's chat (the gateway's hub).
    pub hub: Option<Arc<ferrule_trust::Hub>>,
    /// Collect the report instead of printing it live (a chat door reads
    /// it when the run ends).
    pub quiet: bool,
}

/// How a graph run ended: the exit code (0 = the success condition held)
/// and every line it had to say, ANSI codes stripped for chat delivery.
pub struct GraphReport {
    pub code: i32,
    pub lines: Vec<String>,
}

/// The run's output: printed live on a terminal, collected either way.
struct Says {
    quiet: bool,
    lines: Vec<String>,
}

impl Says {
    fn say(&mut self, line: String) {
        if !self.quiet {
            println!("{line}");
        }
        self.lines.push(unstyled(&line));
    }
}

/// The line without its terminal colours.
fn unstyled(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Run a graph to its end.
pub async fn run(file: &Path, opts: RunOpts) -> Result<GraphReport> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("can't read {}", file.display()))?;
    let graph = parse(&text)?;
    let goal = opts
        .goal
        .clone()
        .or(graph.goal.clone())
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "(no goal given)".into());
    let max_steps = opts.max_steps.unwrap_or(graph.max_steps);

    let (cfg, _) = crate::config::Config::load()?;
    let sandbox = crate::shared_sandbox(&cfg)?;
    let workspace = dunce::canonicalize(&opts.workspace).unwrap_or(opts.workspace.clone());
    // The gateway hands its shared supervisor over; the terminal run builds
    // its own (one process, one claim on the agents store).
    let own_sup = match &opts.supervisor {
        Some(_) => None,
        None => {
            let servers = crate::mcp_servers(&cfg);
            let mcp_tools =
                crate::connect_mcp_servers(&servers, sandbox.clone(), &workspace).await?;
            let sink = crate::ledger::build_sink(&cfg);
            let build = crate::child_builder(opts.max_iterations, mcp_tools);
            let Some(sup) = crate::agents::supervisor(&cfg, opts.model.clone(), sink, build)?
            else {
                bail!("a graph needs sub-agents: set `[agents] enabled = true`");
            };
            Some(sup)
        }
    };
    let sup = opts
        .supervisor
        .as_ref()
        .or(own_sup.as_ref())
        .unwrap()
        .clone();

    // The runner is the tree's root: a deterministic orchestrator, not an
    // agent — it routes, it never decides the work.
    let root = format!("graph-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    sup.store().insert(&AgentRow {
        id: root.clone(),
        tree: root.clone(),
        parent: None,
        depth: 0,
        name: None,
        role: "graph".into(),
        task: goal.clone(),
        session: root.clone(),
        workspace: workspace.clone(),
        worktree: None,
        branch: None,
        base: None,
        status: Status::Idle,
        result: None,
        tokens: 0,
        created_at: now,
        updated_at: now,
        model: None,
    })?;

    let exec = Exec {
        sup: &sup,
        root: &root,
        nodes: &graph.nodes,
        workspace: &workspace,
        sandbox,
        check_timeout: Duration::from_secs(cfg.agent.verify_timeout_secs),
        auto_approve: opts.auto_approve,
        hub: opts.hub.as_ref(),
    };

    let mut says = Says {
        quiet: opts.quiet,
        lines: Vec::new(),
    };
    says.say(format!(
        "\x1b[90mgraph {} — {} nodes, {} edges, root {root}\x1b[0m",
        file.display(),
        graph.nodes.len(),
        graph.edges.len()
    ));

    let n = graph.nodes.len();
    let mut outcomes: Vec<Option<Outcome>> = std::iter::repeat_with(|| None).take(n).collect();
    let mut attempts: Vec<usize> = vec![0; n];
    let mut scheduled_at: Vec<u64> = vec![0; n];
    let mut completed_at: Vec<u64> = vec![0; n];
    let mut fired_at: Vec<u64> = vec![0; graph.edges.len()];
    let mut epoch = 0u64;
    let mut steps = 0usize;

    loop {
        if steps >= max_steps {
            says.say(format!(
                "\x1b[1;33mstopped:\x1b[0m the step cap ({max_steps}) ran out"
            ));
            break;
        }
        let runnable: Vec<usize> = (0..n)
            .filter(|&i| {
                if attempts[i] >= graph.nodes[i].max_attempts.unwrap_or(3).max(1) {
                    return false;
                }
                let gates: Vec<&RuntimeEdge> = graph
                    .edges
                    .iter()
                    .filter(|e| e.to == i && !e.feedback)
                    .collect();
                let satisfied = gates.iter().all(|e| {
                    outcomes[e.from]
                        .as_ref()
                        .is_some_and(|o| e.on.matches(o.verdict))
                });
                if !satisfied {
                    return false;
                }
                if scheduled_at[i] == 0 {
                    return true;
                }
                let fresh_input = gates.iter().any(|e| {
                    completed_at[e.from] > scheduled_at[i]
                        && e.on.matches(outcomes[e.from].as_ref().unwrap().verdict)
                });
                let rollback = graph
                    .edges
                    .iter()
                    .enumerate()
                    .any(|(ei, e)| e.to == i && e.feedback && fired_at[ei] > scheduled_at[i]);
                fresh_input || rollback
            })
            .collect();
        if runnable.is_empty() {
            break;
        }

        // Inline kinds first (they're fast), then the agents fan out in
        // parallel and the join waits for them all.
        let mut agents = Vec::new();
        for i in runnable {
            let node = &graph.nodes[i];
            attempts[i] += 1;
            steps += 1;
            scheduled_at[i] = epoch + 1;
            let prev = prev(&graph, &outcomes, i);
            match node.kind.as_str() {
                "check" => {
                    let out = exec.check(node).await;
                    says.say(report(node, attempts[i], &out));
                    outcomes[i] = Some(out);
                    epoch += 1;
                    completed_at[i] = epoch;
                    fire(&graph, &outcomes, i, &mut fired_at, epoch);
                }
                "approval" => {
                    let message = exec.render(
                        node.message.as_deref().unwrap_or("continue?"),
                        &goal,
                        &prev,
                        attempts[i],
                    );
                    let out = exec.approval(node, message).await;
                    says.say(report(node, attempts[i], &out));
                    outcomes[i] = Some(out);
                    epoch += 1;
                    completed_at[i] = epoch;
                    fire(&graph, &outcomes, i, &mut fired_at, epoch);
                }
                _ => agents.push(i),
            }
        }
        if !agents.is_empty() {
            let mut jobs = Vec::with_capacity(agents.len());
            for i in agents {
                let node = &graph.nodes[i];
                let prev = prev(&graph, &outcomes, i);
                let task = exec.render(
                    node.task.as_deref().unwrap_or(""),
                    &goal,
                    &prev,
                    attempts[i],
                );
                match exec.spawn_agent(node, task) {
                    Ok(id) => jobs.push((i, id)),
                    Err(out) => {
                        says.say(report(node, attempts[i], &out));
                        outcomes[i] = Some(out);
                        epoch += 1;
                        completed_at[i] = epoch;
                        fire(&graph, &outcomes, i, &mut fired_at, epoch);
                    }
                }
            }
            for (i, out) in exec.wait_agents(jobs).await {
                says.say(report(&graph.nodes[i], attempts[i], &out));
                outcomes[i] = Some(out);
                epoch += 1;
                completed_at[i] = epoch;
                fire(&graph, &outcomes, i, &mut fired_at, epoch);
            }
        }
    }

    let (code, last) = finish(&graph, &outcomes);
    says.say(last);
    Ok(GraphReport {
        code,
        lines: says.lines,
    })
}

/// Marks the edges out of `node` whose condition its verdict met.
fn fire(
    graph: &Graph,
    outcomes: &[Option<Outcome>],
    node: usize,
    fired_at: &mut [u64],
    epoch: u64,
) {
    let Some(out) = &outcomes[node] else { return };
    for (i, e) in graph.edges.iter().enumerate() {
        if e.from == node && e.on.matches(out.verdict) {
            fired_at[i] = epoch;
        }
    }
}

/// What a node's direct predecessors last came to, for `{{prev}}`.
fn prev(graph: &Graph, outcomes: &[Option<Outcome>], node: usize) -> String {
    let mut parts = Vec::new();
    for e in graph.edges.iter().filter(|e| e.to == node) {
        if let Some(o) = &outcomes[e.from] {
            let summary: String = o.summary.chars().take(PREV_PER_NODE).collect();
            parts.push(format!(
                "── {} ({}):\n{summary}",
                graph.nodes[e.from].id,
                o.verdict.name()
            ));
        }
    }
    if parts.is_empty() {
        "(nothing came back yet)".into()
    } else {
        parts.join("\n\n")
    }
}

fn report(node: &Node, attempt: usize, out: &Outcome) -> String {
    let (mark, colour) = match out.verdict {
        Verdict::Pass => ("✓", "1;32"),
        Verdict::Fail => ("✗", "1;31"),
    };
    let first = out.summary.lines().next().unwrap_or("").trim();
    let first: String = first.chars().take(100).collect();
    format!(
        "\x1b[{colour}m{mark}\x1b[0m {} ({}, attempt {attempt}) {} — {first}",
        node.id,
        node.kind,
        out.verdict.name()
    )
}

/// The run's own verdict and its truthful summary line.
fn finish(graph: &Graph, outcomes: &[Option<Outcome>]) -> (i32, String) {
    let summary = |i: usize| {
        format!(
            "{} {}",
            graph.nodes[i].id,
            match &outcomes[i] {
                Some(o) => o.verdict.name().to_string(),
                None => "never ran".into(),
            }
        )
    };
    let success = match graph.succeed_when {
        Some(i) => outcomes[i]
            .as_ref()
            .is_some_and(|o| o.verdict == Verdict::Pass),
        None => {
            // Terminal = no way forward; a feedback (rollback) edge is a
            // loop back, not a way out.
            let terminals: Vec<usize> = (0..graph.nodes.len())
                .filter(|&i| !graph.edges.iter().any(|e| e.from == i && !e.feedback))
                .collect();
            !terminals.is_empty()
                && terminals.iter().all(|&i| {
                    outcomes[i]
                        .as_ref()
                        .is_some_and(|o| o.verdict == Verdict::Pass)
                })
        }
    };
    let report: Vec<String> = (0..graph.nodes.len()).map(summary).collect();
    if success {
        (
            0,
            format!("\x1b[1;32mgraph: goal met\x1b[0m ({})", report.join(", ")),
        )
    } else {
        (
            2,
            format!("\x1b[1;33mgraph: not met\x1b[0m ({})", report.join(", ")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollback_edge_is_feedback_and_gates_nothing() {
        let g = parse(
            r#"
            goal = "x"
            [[nodes]]
            id = "implement"
            task = "do it"
            [[nodes]]
            id = "verify"
            kind = "check"
            command = "true"
            [[edges]]
            from = "implement"
            to = "verify"
            [[edges]]
            from = "verify"
            to = "implement"
            on = "fail"
            "#,
        )
        .unwrap();
        let rollback = &g.edges[1];
        assert!(rollback.feedback, "the fail-back edge closes the loop");
        assert!(!g.edges[0].feedback);
    }

    #[test]
    fn validation_says_what_is_wrong() {
        for (text, want) in [
            ("goal = \"x\"", "no [[nodes]]"),
            ("[[nodes]]\nid = \"a\"\nkind = \"check\"", "needs a command"),
            ("[[nodes]]\nid = \"a\"\nkind = \"agent\"", "needs a task"),
            (
                "[[nodes]]\nid = \"a\"\ntask = \"x\"\n[[edges]]\nfrom = \"a\"\nto = \"b\"",
                "doesn't exist",
            ),
            (
                "[[nodes]]\nid = \"a\"\ntask = \"x\"\nrole = \"boss\"",
                "no such role",
            ),
        ] {
            let err = parse(text).unwrap_err().to_string();
            assert!(err.contains(want), "{want:?} not in {err:?}");
        }
    }

    #[test]
    fn a_verdict_is_read_from_the_last_lines_and_fails_closed() {
        assert_eq!(verdict_of("looks good\nVERDICT: PASS"), Verdict::Pass);
        assert_eq!(verdict_of("issues found\nVERDICT: FAIL"), Verdict::Fail);
        assert_eq!(verdict_of("I'm not sure"), Verdict::Fail);
    }

    #[test]
    fn a_loop_of_only_edges_has_no_start() {
        let err = parse(
            "[[nodes]]\nid = \"a\"\ntask = \"x\"\n[[nodes]]\nid = \"b\"\nkind = \"check\"\ncommand = \"true\"\n[[edges]]\nfrom = \"a\"\nto = \"b\"\n[[edges]]\nfrom = \"b\"\nto = \"a\"\non = \"fail\"",
        );
        // a <- fail b is feedback, so "a" is still a start node.
        assert!(err.is_ok(), "{err:?}");
    }
}
