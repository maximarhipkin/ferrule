mod config;

use ferrule_core::{Agent, AgentConfig, AgentEvent, HarnessProfile, ToolContext, Transcript};
use ferrule_gateway::{
    Channel, Gateway, LocalChannel, NewTask, RunOutcome, RunStatus, Router, Scheduler, TaskKind, TaskStore, TelegramChannel,
};
use ferrule_memory::MemoryStore;
use ferrule_providers::OpenAiCompatProvider;
use ferrule_tools::standard_registry;
use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
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
    }
    Ok(())
}

fn build_agent(
    provider_name: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    session_id: &str,
) -> Result<Agent> {
    let sessions_dir = config::data_dir()?.join("sessions");
    let transcript = Transcript::create(&sessions_dir, session_id).ok();
    build_agent_from(provider_name, workspace, max_iterations, transcript)
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
) -> Result<Agent> {
    let (cfg, _) = config::Config::load()?;
    let (name, pcfg, key) = cfg.resolve_provider(provider_name.as_deref())?;
    let provider = Arc::new(OpenAiCompatProvider::new(name, &pcfg.base_url, key, &pcfg.model));
    let profile = HarnessProfile::by_name(&pcfg.profile);

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let tool_ctx = ToolContext { workspace, max_output_chars: 30_000 };

    let mut system = format!(
        "You are an autonomous agent running inside ferrule. Workspace: {}. \
         Use tools to act on the world; verify with evidence; persist important facts with the memory CLI when asked. \
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

    if let Ok(store) = MemoryStore::open(config::data_dir()?.join("memory.db")) {
        if let Ok(mem) = store.assemble_context(None, 2_000) {
            if !mem.is_empty() {
                system.push_str(&format!("\n\n[Long-term memory]\n{mem}"));
            }
        }
    }

    let agent = Agent::new(
        provider,
        standard_registry(),
        profile,
        AgentConfig { max_iterations, ..Default::default() },
        tool_ctx,
        transcript,
    )
    .with_system_prompt(system);
    Ok(agent)
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
    let mut agent = build_agent(provider, workspace, max_iterations, &session_id)?;
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
    let mut agent = build_agent(provider, workspace, 60, &session_id)?;
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

    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |_session_id, transcript| {
        build_agent_from(factory_provider.clone(), factory_workspace.clone(), max_iterations, Some(transcript))
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

    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |_session_id, transcript| {
        build_agent_from(factory_provider.clone(), factory_workspace.clone(), max_iterations, Some(transcript))
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
