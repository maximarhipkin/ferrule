mod config;
mod ledger;
mod memory_tools;

use ferrule_core::{Agent, AgentConfig, AgentEvent, HarnessProfile, ToolContext, Transcript};
use ferrule_core::tool::Tool;
use ferrule_gateway::{
    Channel, Gateway, LocalChannel, NewTask, RunOutcome, RunStatus, Router, Scheduler, TaskKind, TaskStore, TelegramChannel,
};
use ferrule_memory::MemoryStore;
use ferrule_providers::OpenAiCompatProvider;
use ferrule_sandbox::{Mode, Sandbox};
use ferrule_tools::standard_registry;
use ferrule_tools::ShellTool;
use ferrule_mcp::McpServerConfig;
use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "ferrule", version, about = "A portable, memory-efficient agent runtime in Rust")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a one-shot task
    Run {
        prompt: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, default_value_t = 60)]
        max_iterations: usize,
        /// Show model reasoning in the event stream
        #[arg(long)]
        show_reasoning: bool,
    },
    /// Interactive chat session (Ctrl-D to exit)
    Chat {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Agent memory operations
    Memory {
        #[command(subcommand)]
        op: MemoryCmd,
    },
    /// Write an example ferrule.toml to the current directory
    Config {
        #[command(subcommand)]
        op: ConfigCmd,
    },
    /// Run the long-lived gateway daemon (channel adapters + session router)
    Gateway {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, default_value_t = 60)]
        max_iterations: usize,
    },
    /// Scheduled task management (cron / one-shot agent turns)
    Tasks {
        #[command(subcommand)]
        op: TasksCmd,
    },
    /// Per-call provider ledger: calls, errors, tokens, cache hits, latency, cost
    Ledger {
        /// Only rows at or after this point: 7d, 12h, 30m or an RFC 3339 time
        #[arg(long)]
        since: Option<String>,
    },
    /// List the Agent Skills (SKILL.md) an agent in this workspace would load
    Skills {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Show the shell sandbox that applies here, and test that it holds
    Sandbox {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Internal: open a loopback socket and report, run inside the sandbox
        #[arg(long, hide = true)]
        probe_net: bool,
    },
}

#[derive(Subcommand)]
enum TasksCmd {
    /// Add a new scheduled task
    Add {
        name: String,
        /// "cron" (5-field expression) or "once" (RFC 3339 timestamp)
        #[arg(long)]
        kind: String,
        /// Cron: e.g. "0 9 * * *". Once: e.g. "2026-10-01T09:00:00+03:00".
        #[arg(long)]
        schedule: String,
        /// IANA timezone, consulted for `cron` tasks only.
        #[arg(long, default_value = "UTC")]
        timezone: String,
        /// Destination channel the result is delivered to (e.g. "local").
        #[arg(long)]
        channel: String,
        #[arg(long)]
        chat_id: String,
        /// The prompt sent to the agent when the task fires.
        #[arg(long)]
        prompt: String,
        /// Optional shell command run before waking the agent; stdout
        /// `{"wakeAgent": false}` skips the run. See `ferrule-gateway`'s
        /// `scheduler::gate` module doc for the full contract.
        #[arg(long)]
        gate: Option<String>,
    },
    /// List all tasks
    List,
    /// Pause a task — it stays configured but never fires until resumed
    Pause { id: String },
    /// Resume a paused task
    Resume { id: String },
    /// Delete a task and its run history
    Delete { id: String },
    /// Show recent run history for a task
    Runs {
        id: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Execute a task immediately, once, outside its normal schedule
    /// (still subject to the no-overlap guard and its gate, if any)
    RunNow {
        id: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, default_value_t = 60)]
        max_iterations: usize,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    Add { text: String, #[arg(long)] tags: Option<String> },
    Search { query: String },
    Recent { #[arg(long, default_value_t = 10)] n: usize },
}

#[derive(Subcommand)]
enum ConfigCmd {
    Init,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Config { op: ConfigCmd::Init } => {
            if PathBuf::from("ferrule.toml").exists() {
                println!("ferrule.toml already exists");
            } else {
                std::fs::write("ferrule.toml", config::EXAMPLE_CONFIG)?;
                println!("wrote ferrule.toml — edit it, then set the referenced env vars");
            }
        }
        Cmd::Memory { op } => {
            let store = MemoryStore::open(config::data_dir()?.join("memory.db"))?;
            match op {
                MemoryCmd::Add { text, tags } => {
                    let tag_refs: Vec<&str> = tags.as_deref().map(|t| t.split(',').collect()).unwrap_or_default();
                    let id = store.remember(&text, &tag_refs)?;
                    println!("remembered (#{id})");
                }
                MemoryCmd::Search { query } => {
                    for m in store.recall(&query, 10)? {
                        println!("[{:.3}] #{} {}", m.score, m.id, m.content);
                    }
                }
                MemoryCmd::Recent { n } => {
                    for m in store.recent(n)? {
                        println!("#{} {}", m.id, m.content);
                    }
                }
            }
        }
        Cmd::Run { prompt, provider, workspace, max_iterations, show_reasoning } => {
            run_once(&prompt, provider, workspace, max_iterations, show_reasoning).await?;
        }
        Cmd::Chat { provider, workspace } => {
            chat(provider, workspace).await?;
        }
        Cmd::Gateway { provider, workspace, max_iterations } => {
            run_gateway(provider, workspace, max_iterations).await?;
        }
        Cmd::Tasks { op } => {
            tasks_cmd(op).await?;
        }
        Cmd::Ledger { since } => {
            ledger_cmd(since)?;
        }
        Cmd::Skills { workspace } => {
            skills_cmd(workspace);
        }
        Cmd::Sandbox { probe_net: true, .. } => probe_net(),
        Cmd::Sandbox { workspace, probe_net: false } => {
            sandbox_cmd(workspace)?;
        }
    }
    Ok(())
}

async fn build_agent(
    provider_name: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    session_id: &str,
    task_shape: &str,
) -> Result<Agent> {
    let (cfg, _) = config::Config::load()?;
    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers).await;
    let ledger = ledger::LedgerTag::new(&ledger::build_sink(&cfg), task_shape, None);
    let sessions_dir = config::data_dir()?.join("sessions");
    let transcript = Transcript::create(&sessions_dir, session_id).ok();
    build_agent_from(provider_name, workspace, max_iterations, transcript, &mcp_tools, ledger)
}

/// Spawn every configured MCP server once and return its tools. A server
/// that fails to start is logged and skipped — it never stops the agent.
/// Callers that build many agents (the gateway, one per session) call this
/// once and share the result, so N sessions don't mean N copies of each
/// server process.
async fn connect_mcp_servers(servers: &[McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    let mut tools = Vec::new();
    for server in servers {
        match ferrule_mcp::connect_and_build_tools(server.clone()).await {
            Ok(t) => tools.extend(t),
            Err(e) => tracing::warn!("mcp server `{}` failed to start ({e}); continuing without it", server.name),
        }
    }
    tools
}

/// Shared assembly logic for every entry point that needs a ready-to-run
/// `Agent` (one-shot `run`, interactive `chat`, and every gateway session).
/// Takes an already-created (or reopened) `Transcript` rather than a raw
/// session id so the gateway's `Router` — which owns transcript lifecycle
/// for resumable sessions — can hand in the exact same transcript it just
/// read history from, instead of this function creating a second one.
fn build_agent_from(
    provider_name: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    transcript: Option<Transcript>,
    mcp_tools: &[Arc<dyn Tool>],
    ledger: Option<ledger::LedgerTag>,
) -> Result<Agent> {
    let (cfg, _) = config::Config::load()?;
    let (name, pcfg, key) = cfg.resolve_provider(provider_name.as_deref())?;
    let provider = Arc::new(OpenAiCompatProvider::new(name, &pcfg.base_url, key, &pcfg.model));
    let profile = HarnessProfile::by_name(&pcfg.profile);

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let tool_ctx = ToolContext { workspace, max_output_chars: 30_000 };

    let mut registry = standard_registry();
    let sandbox = shared_sandbox(&cfg)?;
    registry.register(Arc::new(ShellTool::sandboxed(sandbox.clone())));
    if sandbox.policy().mode == Mode::ReadOnly {
        registry.remove("write_file");
    }
    for tool in memory_tools::tools(config::data_dir()?.join("memory.db")) {
        registry.register(tool);
    }
    for tool in mcp_tools {
        registry.register(tool.clone());
    }

    let mut system = format!(
        "You are an autonomous agent running inside ferrule. Workspace: {}. \
         Use tools to act on the world; verify with evidence; persist important facts with the remember tool when asked. \
         For multi-step work, maintain your task list with write_todos and log decisions with log_diary. {}",
        tool_ctx.workspace.display(),
        profile.system_directive
    );

    // Context baseline: living documentation written for agents (AGENTS.md et al).
    if let Some((name, content)) = ferrule_core::load_context_baseline(&tool_ctx.workspace) {
        system.push_str(&format!("\n\n[Workspace context baseline: {name}]\n{content}"));
    }

    // Validation: the build system is truth. Pass = submit; fail = fix forward.
    if let Some(cmd) = &cfg.agent.verify_command {
        system.push_str(&format!(
            "\n\n[Validation policy] Before considering any code change complete, run `{cmd}` via the shell tool. \
             If it fails, fix forward — do not revert, do not stop until it passes."
        ));
    }

    // Agent Skills: names + descriptions in the prompt, full instructions
    // loaded on demand through the activate_skill tool. Rescanned per agent,
    // so a skill installed while the gateway runs shows up in new sessions.
    if cfg.skills.enabled {
        let skills = Arc::new(discover_skills(&cfg.skills, &tool_ctx.workspace));
        if let Some(catalog) = skills.catalog() {
            system.push_str(&format!("\n\n[Skills]\n{catalog}"));
        }
        for tool in ferrule_skills::tools(skills) {
            registry.register(tool);
        }
    }

    if let Ok(store) = MemoryStore::open(config::data_dir()?.join("memory.db")) {
        if let Ok(mem) = store.assemble_context(None, 2_000) {
            if !mem.is_empty() {
                system.push_str(&format!("\n\n[Long-term memory]\n{mem}"));
            }
        }
    }

    let mut agent = Agent::new(
        provider,
        registry,
        profile,
        AgentConfig { max_iterations, ..Default::default() },
        tool_ctx,
        transcript,
    )
    .with_system_prompt(system);
    if let Some(tag) = ledger {
        agent = agent.with_ledger(tag.sink, tag.task_shape, tag.origin, pcfg.model.clone());
    }
    Ok(agent)
}

/// The config's sandbox policy, completed with what only the host knows:
/// the env vars the config names as holding secrets.
fn sandbox_policy(cfg: &config::Config) -> ferrule_sandbox::Policy {
    let mut policy = cfg.sandbox.clone();
    policy.secret_vars.extend(cfg.providers.values().map(|p| p.api_key_env.clone()));
    policy.secret_vars.extend(cfg.gateway.telegram_token_env.clone());
    policy
}

/// Built (and probed) once per process — the gateway builds an agent per
/// session and shouldn't fork a probe for each. A `[sandbox]` edit takes a
/// restart to apply.
fn shared_sandbox(cfg: &config::Config) -> Result<Arc<Sandbox>> {
    static SANDBOX: OnceLock<Arc<Sandbox>> = OnceLock::new();
    if let Some(sandbox) = SANDBOX.get() {
        return Ok(sandbox.clone());
    }
    let sandbox = Arc::new(Sandbox::new(sandbox_policy(cfg)).map_err(|e| anyhow!(e))?);
    Ok(SANDBOX.get_or_init(|| sandbox).clone())
}

fn spawn_renderer(show_reasoning: bool) -> mpsc::Sender<AgentEvent> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::AssistantText { text } => println!("\n\x1b[1massistant:\x1b[0m {text}"),
                AgentEvent::Reasoning { text } if show_reasoning => {
                    println!("\x1b[90m[reasoning: {}…]\x1b[0m", text.chars().take(200).collect::<String>())
                }
                AgentEvent::ToolCallStarted { name, arguments, .. } => {
                    let args = arguments.to_string();
                    println!("\x1b[36m▶ {name}\x1b[0m {}", args.chars().take(160).collect::<String>())
                }
                AgentEvent::ToolCallFinished { name, ok, output_chars, .. } => {
                    println!("\x1b[90m  {} {name} ({output_chars} chars)\x1b[0m", if ok { "✓" } else { "✗" })
                }
                AgentEvent::Compacted { folded_messages, est_tokens_before, est_tokens_after } => {
                    println!("\x1b[33m[compacted {folded_messages} messages: ~{est_tokens_before} → ~{est_tokens_after} tokens]\x1b[0m")
                }
                AgentEvent::Usage { input_tokens, output_tokens, cached_input_tokens } => {
                    println!("\x1b[90m  [usage: in {input_tokens} (cached {cached_input_tokens}) / out {output_tokens}]\x1b[0m")
                }
                AgentEvent::Error { message } => eprintln!("\x1b[31merror: {message}\x1b[0m"),
                _ => {}
            }
        }
    });
    tx
}

async fn run_once(prompt: &str, provider: Option<String>, workspace: PathBuf, max_iterations: usize, show_reasoning: bool) -> Result<()> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut agent = build_agent(provider, workspace, max_iterations, &session_id, "run").await?;
    let tx = spawn_renderer(show_reasoning);
    let answer = agent.run(prompt, tx).await;
    match answer {
        Ok(text) => {
            println!("\n\x1b[1;32mfinal:\x1b[0m {text}");
            let u = &agent.usage;
            println!("\x1b[90m[total usage: in {} (cached {}) / out {}]\x1b[0m", u.input_tokens, u.cached_input_tokens, u.output_tokens);
        }
        Err(e) => {
            eprintln!("\x1b[31mrun failed: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn chat(provider: Option<String>, workspace: PathBuf) -> Result<()> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut agent = build_agent(provider, workspace, 60, &session_id, "chat").await?;
    println!("ferrule chat — Ctrl-D to exit. Session {session_id}");
    let stdin = std::io::stdin();
    loop {
        print!("\n\x1b[1;34myou>\x1b[0m ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        let tx = spawn_renderer(false);
        match agent.run(prompt, tx).await {
            Ok(text) => println!("\n\x1b[1;32magent:\x1b[0m {text}"),
            Err(e) => eprintln!("\x1b[31mrun failed: {e}\x1b[0m"),
        }
    }
    Ok(())
}

/// Builds every channel enabled in `[gateway]`, keyed by channel name. Shared
/// by the long-lived daemon (`run_gateway`) and `ferrule tasks run-now` (which
/// needs the same destination channels available to deliver its one result,
/// without starting the daemon's inbound loops).
fn build_channels(cfg: &config::Config) -> Result<HashMap<String, Arc<dyn Channel>>> {
    let mut named_channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();

    if cfg.gateway.local {
        let local: Arc<dyn Channel> = Arc::new(LocalChannel::stdio("local"));
        named_channels.insert(local.name().to_string(), local);
    }

    if let Some(env_var) = &cfg.gateway.telegram_token_env {
        let token = std::env::var(env_var)
            .map_err(|_| anyhow!("env var `{env_var}` not set (needed by [gateway].telegram_token_env)"))?;
        let telegram: Arc<dyn Channel> = Arc::new(TelegramChannel::with_base_url(token, cfg.gateway.telegram_base_url.clone()));
        named_channels.insert(telegram.name().to_string(), telegram);
    }

    Ok(named_channels)
}

async fn run_gateway(provider: Option<String>, workspace: PathBuf, max_iterations: usize) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let sessions_dir = config::data_dir()?.join("sessions");

    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers).await;
    let ledger_sink = ledger::build_sink(&cfg);
    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |session_id, transcript| {
        let (shape, origin) = ledger::classify_session(session_id);
        let tag = ledger::LedgerTag::new(&ledger_sink, shape, origin);
        build_agent_from(factory_provider.clone(), factory_workspace.clone(), max_iterations, Some(transcript), &mcp_tools, tag)
            .map_err(|e| ferrule_gateway::GatewayError::Channel(e.to_string()))
    });

    let named_channels = build_channels(&cfg)?;
    if named_channels.is_empty() {
        bail!("no channel enabled in [gateway] — set `local = true` and/or `telegram_token_env` in ferrule.toml");
    }
    let adapters: Vec<Arc<dyn Channel>> = named_channels.values().cloned().collect();

    // Arc'd so the same router serves both the gateway's channel adapters
    // and the scheduler's task-triggered turns — one router, two front
    // doors (see `ferrule_gateway::Gateway::new`'s doc comment).
    let router = Arc::new(Router::new(sessions_dir, agent_factory, named_channels.clone()));

    let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
    let scheduler = Arc::new(Scheduler::new(
        store,
        router.clone(),
        named_channels,
        Duration::from_secs(cfg.scheduler.tick_interval_secs),
        Duration::from_secs(cfg.scheduler.gate_timeout_secs),
        cfg.scheduler.gate_workspace.clone(),
    )?);
    let scheduler_handle = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            if let Err(e) = scheduler.run().await {
                tracing::error!(error = %e, "scheduler stopped");
            }
        })
    };

    let mut gateway = Gateway::new(router);
    for channel in adapters {
        gateway.add_channel(channel);
    }

    tracing::info!("gateway starting");
    let result = gateway.run().await;
    // The scheduler's own `run()` loops forever by design (see its doc
    // comment); once the gateway is done there is nothing left to serve, so
    // it's stopped explicitly rather than left dangling.
    scheduler_handle.abort();
    result?;
    Ok(())
}

fn parse_task_kind(s: &str) -> Result<TaskKind> {
    match s {
        "cron" => Ok(TaskKind::Cron),
        "once" => Ok(TaskKind::Once),
        other => bail!("kind must be `cron` or `once`, got `{other}`"),
    }
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339()).unwrap_or_else(|| ts.to_string())
}

async fn tasks_cmd(op: TasksCmd) -> Result<()> {
    match op {
        TasksCmd::Add { name, kind, schedule, timezone, channel, chat_id, prompt, gate } => {
            let kind = parse_task_kind(&kind)?;
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            let now = chrono::Utc::now();
            let next_run_at = ferrule_gateway::initial_next_run_at(kind, &schedule, &timezone, now)?;
            let id = uuid::Uuid::new_v4().to_string();
            let task = store.add(NewTask { name, kind, schedule, timezone, channel, chat_id, prompt, gate }, id, now.timestamp(), next_run_at)?;
            println!("added task {} ({})", task.id, task.name);
            match task.next_run_at {
                Some(t) => println!("next run: {}", fmt_ts(t)),
                None => println!("next run: never (no schedule computed)"),
            }
        }
        TasksCmd::List => {
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            let tasks = store.list()?;
            if tasks.is_empty() {
                println!("no tasks");
            }
            for t in tasks {
                println!(
                    "{}  {:<24}  {:<5}  {:<24}  {:<7}  next={}",
                    t.id,
                    t.name,
                    if t.kind == TaskKind::Cron { "cron" } else { "once" },
                    t.schedule,
                    if t.enabled { "enabled" } else { "paused" },
                    t.next_run_at.map(fmt_ts).unwrap_or_else(|| "-".into()),
                );
            }
        }
        TasksCmd::Pause { id } => {
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            if store.set_enabled(&id, false)? {
                println!("paused {id}");
            } else {
                println!("no such task: {id}");
            }
        }
        TasksCmd::Resume { id } => {
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            if store.set_enabled(&id, true)? {
                println!("resumed {id}");
            } else {
                println!("no such task: {id}");
            }
        }
        TasksCmd::Delete { id } => {
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            if store.delete(&id)? {
                println!("deleted {id}");
            } else {
                println!("no such task: {id}");
            }
        }
        TasksCmd::Runs { id, limit } => {
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            let runs = store.runs_for(&id, limit)?;
            if runs.is_empty() {
                println!("no runs for {id}");
            }
            for r in runs {
                let status = match r.status {
                    RunStatus::Running => "running",
                    RunStatus::Succeeded => "succeeded",
                    RunStatus::Failed => "failed",
                    RunStatus::Skipped => "skipped",
                };
                println!(
                    "{}  {:<10}  started={}  finished={}  {}",
                    r.id,
                    status,
                    fmt_ts(r.started_at),
                    r.finished_at.map(fmt_ts).unwrap_or_else(|| "-".into()),
                    r.detail.as_deref().unwrap_or(""),
                );
            }
        }
        TasksCmd::RunNow { id, provider, workspace, max_iterations } => {
            tasks_run_now(&id, provider, workspace, max_iterations).await?;
        }
    }
    Ok(())
}

async fn tasks_run_now(id: &str, provider: Option<String>, workspace: PathBuf, max_iterations: usize) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let sessions_dir = config::data_dir()?.join("sessions");
    let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
    let task = store.get(id)?.ok_or_else(|| anyhow!("task `{id}` not found"))?;

    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers).await;
    let ledger_sink = ledger::build_sink(&cfg);
    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |session_id, transcript| {
        let (shape, origin) = ledger::classify_session(session_id);
        let tag = ledger::LedgerTag::new(&ledger_sink, shape, origin);
        build_agent_from(factory_provider.clone(), factory_workspace.clone(), max_iterations, Some(transcript), &mcp_tools, tag)
            .map_err(|e| ferrule_gateway::GatewayError::Channel(e.to_string()))
    });

    let named_channels = build_channels(&cfg)?;
    let router = Arc::new(Router::new(sessions_dir, agent_factory, named_channels.clone()));
    // Sharing the same db file (via TaskStore::open above) as any running
    // `ferrule gateway` daemon means the no-overlap guard and interrupted-run
    // recovery apply here exactly as they do to a scheduled tick — this is
    // a real, guarded execution, not a bypass.
    let scheduler = Scheduler::new(
        store,
        router,
        named_channels,
        Duration::from_secs(cfg.scheduler.tick_interval_secs),
        Duration::from_secs(cfg.scheduler.gate_timeout_secs),
        cfg.scheduler.gate_workspace.clone(),
    )?;

    match scheduler.execute(&task).await {
        Ok(RunOutcome::Succeeded { answer }) => println!("succeeded:\n{answer}"),
        Ok(RunOutcome::Skipped { reason }) => println!("skipped: {}", reason.unwrap_or_else(|| "(no reason given)".into())),
        Ok(RunOutcome::AlreadyRunning) => println!("a run for this task is already in progress; try again shortly"),
        Err(e) => {
            eprintln!("run failed: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn discover_skills(cfg: &config::SkillsConfig, workspace: &Path) -> ferrule_skills::SkillSet {
    let paths: Vec<PathBuf> = cfg.paths.iter().map(|p| expand_home(p)).collect();
    let roots = ferrule_skills::default_roots(workspace, cfg.project, &paths);
    let set = ferrule_skills::discover(&roots, &cfg.disabled);
    for d in &set.diagnostics {
        tracing::debug!(path = %d.path.display(), severity = ?d.severity, "skill: {}", d.message);
    }
    set
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

fn skills_cmd(workspace: PathBuf) {
    // Listing works without a config file: the defaults are what an agent
    // would use too.
    let cfg = match config::Config::load() {
        Ok((cfg, _)) => cfg.skills,
        Err(e) => {
            eprintln!("({e} — showing default [skills] settings)");
            config::SkillsConfig::default()
        }
    };
    if !cfg.enabled {
        println!("skills are off ([skills] enabled = false) — agents load none of these");
    }
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let set = discover_skills(&cfg, &workspace);
    println!(
        "{} skill(s), {} offered to the model{}",
        set.skills.len(),
        set.invocable().count(),
        if cfg.project { "" } else { " (project skills off)" }
    );
    for s in &set.skills {
        let offered = if s.model_invocable { "model" } else { "hidden" };
        println!("  {:<32} {:<8} {:<7} {}", s.name, s.scope.as_str(), offered, s.location.display());
    }
    if set.skills.iter().any(|s| !s.model_invocable) {
        println!("  (hidden = `disable-model-invocation: true`; not in the catalog, no tool can load it)");
    }
    if !set.diagnostics.is_empty() {
        println!("\n{} note(s):", set.diagnostics.len());
        for d in &set.diagnostics {
            let level = match d.severity {
                ferrule_skills::Severity::Warning => "warn",
                ferrule_skills::Severity::Skipped => "skip",
            };
            println!("  {level}  {}: {}", d.path.display(), d.message);
        }
    }
}

/// `ferrule sandbox`: the sandbox that applies here, then evidence that it
/// holds — each check runs through the same `Sandbox::command` the shell
/// tool uses. Exits non-zero if any check fails.
fn sandbox_cmd(workspace: PathBuf) -> Result<()> {
    let policy = match config::Config::load() {
        Ok((cfg, _)) => sandbox_policy(&cfg),
        Err(e) => {
            eprintln!("({e} — showing default [sandbox] settings)");
            ferrule_sandbox::Policy::default()
        }
    };
    let workspace = workspace.canonicalize()?;
    let sandbox = Sandbox::new(policy).map_err(|e| anyhow!(e))?;
    let policy = sandbox.policy();
    let roots = sandbox.writable_roots(&workspace);
    let withheld = sandbox.scrubbed_vars();

    println!("backend   {}", sandbox.backend());
    if let Some(why) = sandbox.degraded() {
        println!("          shell commands run UNSANDBOXED: {why}");
    }
    let mode = match policy.mode {
        Mode::Off => "off",
        Mode::ReadOnly => "read-only",
        Mode::WorkspaceWrite => "workspace-write",
    };
    println!("mode      {mode}{}", if policy.require { " (required)" } else { "" });
    if sandbox.is_active() {
        println!("network   {}", if policy.network { "allowed" } else { "blocked" });
        println!("writable  {}", if roots.is_empty() { "nothing (except /dev/null)".into() } else { roots[0].display().to_string() });
        for root in roots.iter().skip(1) {
            println!("          {}", root.display());
        }
    }
    println!("env       {} secret var(s) withheld{}", withheld.len(), if withheld.is_empty() { String::new() } else { format!(": {}", withheld.join(", ")) });

    let mut failures = 0;
    let mut report = |what: &str, ok: bool| {
        println!("  {}  {what}", if ok { "ok  " } else { "FAIL" });
        failures += usize::from(!ok);
    };
    let sh = |script: &str, arg: &Path| -> Result<std::process::Output> {
        Ok(sandbox
            .command("/bin/sh", ["-c".as_ref(), script.as_ref(), "sh".as_ref(), arg.as_os_str()], &workspace)?
            .stdin(std::process::Stdio::null())
            .output()?)
    };
    println!("\nchecks");

    let env = sh("env", Path::new(""))?;
    let env = String::from_utf8_lossy(&env.stdout);
    report("secret env vars are not visible to commands", withheld.iter().all(|name| !env.lines().any(|l| l.starts_with(&format!("{name}=")))));
    if !sandbox.is_active() {
        println!("  (no sandbox: nothing else to check)");
        return if failures == 0 { Ok(()) } else { bail!("{failures} check(s) failed") };
    }

    let probe_name = format!(".ferrule-sandbox-probe-{}", std::process::id());
    let inside = workspace.join(&probe_name);
    let wrote = sh("echo probe > \"$1\"", &inside)?.status.success();
    let _ = std::fs::remove_file(&inside);
    match policy.mode {
        Mode::ReadOnly => report("workspace write is refused (read-only)", !wrote),
        _ => report("workspace write works", wrote),
    }

    // Aim outside every root, at a place this process itself can write — so
    // a refusal is the sandbox, not plain file permissions.
    let candidates = [std::env::var_os("HOME").map(PathBuf::from), Some(PathBuf::from("/var/tmp")), workspace.parent().map(Path::to_path_buf)];
    let target = candidates.into_iter().flatten().filter_map(|dir| dir.canonicalize().ok()).find(|dir| {
        let probe = dir.join(&probe_name);
        let writable = !roots.iter().any(|r| dir.starts_with(r)) && std::fs::write(&probe, "").is_ok();
        let _ = std::fs::remove_file(&probe);
        writable
    });
    match target {
        Some(dir) => {
            let probe = dir.join(&probe_name);
            let out = sh("echo probe > \"$1\"", &probe)?;
            let escaped = out.status.success() || probe.exists();
            let _ = std::fs::remove_file(&probe);
            report(&format!("write outside the roots is refused ({})", dir.display()), !escaped);
        }
        None => println!("  skip  write outside the roots (no writable dir outside them to aim at)"),
    }

    let me = std::env::current_exe()?;
    let net = sandbox.command(&me, ["sandbox", "--probe-net"], &workspace)?.stdin(std::process::Stdio::null()).output()?;
    let opened = net.status.success();
    if policy.network {
        report("network sockets open", opened);
    } else {
        report("network sockets are refused", !opened && String::from_utf8_lossy(&net.stdout).contains("refused"));
    }
    if failures == 0 {
        Ok(())
    } else {
        bail!("{failures} check(s) failed")
    }
}

/// Run by `ferrule sandbox` inside the sandbox: can this process open an
/// inet socket? Loopback only, so the answer doesn't depend on connectivity.
fn probe_net() {
    let udp = std::net::UdpSocket::bind("127.0.0.1:0");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0");
    match (udp, tcp) {
        (Ok(_), Ok(_)) => println!("opened"),
        (udp, tcp) => {
            let err = udp.err().or(tcp.err()).expect("one of them failed");
            let refused = err.kind() == std::io::ErrorKind::PermissionDenied;
            println!("{}: {err}", if refused { "refused" } else { "failed" });
            std::process::exit(1);
        }
    }
}

fn ledger_cmd(since: Option<String>) -> Result<()> {
    let since = since.map(|s| ledger::parse_since(&s, chrono::Utc::now())).transpose()?;
    let path = ledger::ledger_path()?;
    let (records, malformed) = ledger::read_records(&path, since)?;
    if records.is_empty() {
        println!("no ledger rows{} in {}", if since.is_some() { " in that window" } else { "" }, path.display());
    } else {
        println!("{}", ledger::render_table(&ledger::aggregate(&records)));
    }
    if malformed > 0 {
        eprintln!("skipped {malformed} malformed line(s)");
    }
    Ok(())
}
