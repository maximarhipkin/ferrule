mod config;
mod doctor;
mod ledger;
mod memory_tools;
mod probe;
mod secrets;
mod service;
mod setup;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Parser, Subcommand};
use ferrule_core::tool::Tool;
use ferrule_core::{Agent, AgentConfig, AgentEvent, HarnessProfile, ToolContext, Transcript};
use ferrule_gateway::{
    Channel, Gateway, LocalChannel, NewTask, Router, RunOutcome, Scheduler, TaskKind, TaskStore,
    TelegramChannel,
};
use ferrule_mcp::McpServerConfig;
use ferrule_memory::MemoryStore;
use ferrule_providers::OpenAiCompatProvider;
use ferrule_proxy::{Broker, BrokerConfig, Upstream};
use ferrule_sandbox::{Egress, Mode, Sandbox};
use ferrule_tools::standard_registry;
use ferrule_tools::{
    CommandVerifier, ListDirTool, ReadFileTool, ShellTool, WebFetchTool, WriteFileTool,
};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "ferrule",
    version,
    about = "A portable, memory-efficient agent runtime in Rust"
)]
struct Cli {
    /// Config file to use instead of ./ferrule.toml or the global one
    /// (also `$FERRULE_CONFIG`)
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive setup: provider and key, Telegram, tool credentials,
    /// sandbox, background service. Re-run it any time to change one part
    Setup,
    /// Check the config, keys, Telegram, sandbox and service, and say what to fix
    Doctor {
        /// Skip the checks that call provider and Telegram APIs
        #[arg(long)]
        offline: bool,
    },
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
    /// Where the config lives, and ways to open it (`ferrule setup` is the
    /// guided way to change it)
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
    /// Show the shell sandbox that applies here, and test that it holds.
    /// `ferrule sandbox -- CMD…` runs CMD the way the agent's shell tool would
    Sandbox {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Internal: open a loopback socket and report, run inside the sandbox
        #[arg(long, hide = true)]
        probe_net: bool,
        /// Command to run under the sandbox (and `[secrets]` placeholders)
        #[arg(last = true)]
        exec: Vec<String>,
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
    Add {
        text: String,
        #[arg(long)]
        tags: Option<String>,
    },
    Search {
        query: String,
    },
    Recent {
        #[arg(long, default_value_t = 10)]
        n: usize,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Show the config file, the saved-keys file, the data dir and the
    /// service unit
    Path,
    /// Open the config in $VISUAL / $EDITOR, then check it still parses
    Edit,
    /// Print a commented example with every option
    Example,
    /// Write that example to ./ferrule.toml
    Init,
}

/// Sync on purpose: `--config` and the secrets file go into the environment
/// before the runtime starts any thread, since `set_var` isn't thread-safe.
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(path) = &cli.config {
        std::env::set_var("FERRULE_CONFIG", std::path::absolute(path)?);
    }
    secrets::load_into_env();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(dispatch(cli.cmd))
}

async fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Setup => setup::run().await?,
        Cmd::Doctor { offline } => {
            if !doctor::run(offline).await? {
                std::process::exit(1);
            }
        }
        Cmd::Config {
            op: ConfigCmd::Path,
        } => config_path_cmd()?,
        Cmd::Config {
            op: ConfigCmd::Edit,
        } => config_edit_cmd()?,
        Cmd::Config {
            op: ConfigCmd::Example,
        } => print!("{}", config::EXAMPLE_CONFIG),
        Cmd::Config {
            op: ConfigCmd::Init,
        } => {
            if PathBuf::from("ferrule.toml").exists() {
                println!("ferrule.toml already exists");
            } else {
                std::fs::write("ferrule.toml", config::EXAMPLE_CONFIG)?;
                println!("wrote ferrule.toml — edit it, or run `ferrule setup` to fill it in interactively");
            }
        }
        Cmd::Memory { op } => {
            let store = MemoryStore::open(config::data_dir()?.join("memory.db"))?;
            match op {
                MemoryCmd::Add { text, tags } => {
                    let tag_refs: Vec<&str> = tags
                        .as_deref()
                        .map(|t| t.split(',').collect())
                        .unwrap_or_default();
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
        Cmd::Run {
            prompt,
            provider,
            workspace,
            max_iterations,
            show_reasoning,
        } => {
            run_once(&prompt, provider, workspace, max_iterations, show_reasoning).await?;
        }
        Cmd::Chat {
            provider,
            workspace,
        } => {
            chat(provider, workspace).await?;
        }
        Cmd::Gateway {
            provider,
            workspace,
            max_iterations,
        } => {
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
        Cmd::Sandbox {
            probe_net: true, ..
        } => probe_net(),
        Cmd::Sandbox {
            workspace,
            probe_net: false,
            exec,
        } if !exec.is_empty() => {
            let (cfg, _) = config::Config::load()?;
            let status = shared_sandbox(&cfg)?
                .command(&exec[0], &exec[1..], &workspace.canonicalize()?)?
                .status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::Sandbox {
            workspace,
            probe_net: false,
            ..
        } => {
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
    let sandbox = shared_sandbox(&cfg)?;
    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers, sandbox, &workspace).await;
    let ledger = ledger::LedgerTag::new(&ledger::build_sink(&cfg), task_shape, None);
    let sessions_dir = config::data_dir()?.join("sessions");
    let transcript = Transcript::create(&sessions_dir, session_id).ok();
    build_agent_from(
        provider_name,
        workspace,
        max_iterations,
        transcript,
        &mcp_tools,
        ledger,
    )
}

/// Spawn every configured MCP server once and return its tools. A server
/// that fails to start is logged and skipped — it never stops the agent.
/// Callers that build many agents (the gateway, one per session) call this
/// once and share the result, so N sessions don't mean N copies of each
/// server process.
///
/// Servers run in the workspace, through the same sandbox as shell commands
/// plus a state dir of their own under the data dir — see `ServerHost`.
async fn connect_mcp_servers(
    servers: &[McpServerConfig],
    sandbox: Arc<Sandbox>,
    workspace: &Path,
) -> Vec<Arc<dyn Tool>> {
    let mut tools = Vec::new();
    for server in servers {
        let state_dir = match mcp_state_dir(&server.name) {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(
                    "mcp server `{}`: couldn't create its state dir ({e}); continuing without it",
                    server.name
                );
                continue;
            }
        };
        let host = ferrule_mcp::ServerHost {
            sandbox: sandbox.clone(),
            workspace: workspace.to_path_buf(),
            state_dir,
        };
        match ferrule_mcp::connect_and_build_tools(server.clone(), host).await {
            Ok(t) => tools.extend(t),
            Err(e) => tracing::warn!(
                "mcp server `{}` failed to start ({e}); continuing without it",
                server.name
            ),
        }
    }
    tools
}

/// `<data dir>/mcp/<name>`, created if missing: one server's home, caches
/// and temp dir.
fn mcp_state_dir(server_name: &str) -> Result<PathBuf> {
    let dir = config::data_dir()?
        .join("mcp")
        .join(mcp_dir_name(server_name));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A server name as a directory name. A name that had to be changed gets a
/// hash of the original, so `a.b` and `a_b` don't share a home.
fn mcp_dir_name(server_name: &str) -> String {
    let safe: String = server_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe == server_name && !safe.is_empty() {
        return safe;
    }
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let hash = server_name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{safe}-{:08x}", hash as u32)
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
    let provider = Arc::new(OpenAiCompatProvider::new(
        name,
        &pcfg.base_url,
        key,
        &pcfg.model,
    ));
    let profile = HarnessProfile::by_name(&pcfg.profile);

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let tool_ctx = ToolContext {
        workspace,
        max_output_chars: 30_000,
    };

    let mut registry = standard_registry();
    let sandbox = shared_sandbox(&cfg)?;
    let broker = shared_broker(&cfg)?;
    registry.register(Arc::new(ShellTool::sandboxed(sandbox.clone())));
    registry.register(Arc::new(WebFetchTool::with_egress(
        sandbox.egress().cloned(),
    )));
    // The file tools run in this process, outside the sandbox: they refuse
    // its hidden paths themselves, or a workspace that contains the data
    // dir would let `read_file` hand over the saved keys.
    let hidden = sandbox.policy().hidden.clone();
    registry.register(Arc::new(ReadFileTool::hiding(hidden.clone())));
    registry.register(Arc::new(WriteFileTool::hiding(hidden.clone())));
    registry.register(Arc::new(ListDirTool::hiding(hidden)));
    warn_data_in_workspace(&sandbox, &tool_ctx.workspace);
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

    if let Some(broker) = broker {
        system.push_str(&format!(
            "\n\n[Credentials]\n{}",
            broker.model_note().trim_end()
        ));
    }

    // Context baseline: living documentation written for agents (AGENTS.md et al).
    if let Some((name, content)) = ferrule_core::load_context_baseline(&tool_ctx.workspace) {
        system.push_str(&format!(
            "\n\n[Workspace context baseline: {name}]\n{content}"
        ));
    }

    // Validation: ferrule runs the check itself when a run that changed
    // files tries to finish, and sends a failure back to be fixed.
    if let Some(cmd) = &cfg.agent.verify_command {
        system.push_str(&format!(
            "\n\n[Validation policy] When you finish after changing files, ferrule runs `{cmd}`. \
             If it fails you get its output back and keep working: fix forward, don't revert. \
             You can run it yourself with the shell tool before finishing."
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
        AgentConfig {
            max_iterations,
            ..Default::default()
        },
        tool_ctx,
        transcript,
    )
    .with_system_prompt(system);
    if let Some(tag) = ledger {
        agent = agent.with_ledger(tag.sink, tag.task_shape, tag.origin, pcfg.model.clone());
    }
    if let Some(cmd) = &cfg.agent.verify_command {
        let timeout = Duration::from_secs(cfg.agent.verify_timeout_secs);
        agent = agent.with_verifier(Arc::new(CommandVerifier::new(
            cmd.clone(),
            sandbox,
            timeout,
        )));
    }
    Ok(agent)
}

/// The config's sandbox policy, completed with what only the host knows:
/// the env vars the config names as holding secrets, and the directories
/// holding them on disk.
fn sandbox_policy(cfg: &config::Config) -> ferrule_sandbox::Policy {
    let mut policy = cfg.sandbox.clone();
    policy
        .secret_vars
        .extend(cfg.providers.values().map(|p| p.api_key_env.clone()));
    policy
        .secret_vars
        .extend(cfg.gateway.telegram_token_env.clone());
    // Commands get these back as placeholders, from the credential proxy.
    policy.secret_vars.extend(cfg.secrets.keys().cloned());
    policy.hidden.extend(hidden_paths());
    policy
}

/// The secrets file's directory and the credential proxy's CA key — never
/// readable by a sandboxed command. The sandbox resolves them per command,
/// so the proxy's keys (written when the broker first starts) are covered
/// too; the secrets directory is created now so a file `ferrule setup`
/// writes into it later lands inside something already hidden.
fn hidden_paths() -> Vec<PathBuf> {
    let Ok(data) = config::data_dir() else {
        return Vec::new();
    };
    let private = data.join("private");
    let _ = std::fs::create_dir_all(&private);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700));
    }
    vec![private, data.join("proxy").join("keys")]
}

/// Landlock can't grant a folder without what's in it, so when the
/// workspace holds the data dir, the folders on the way down to the keys
/// stay read-only for commands. Said once per process (the gateway builds
/// an agent per session).
fn warn_data_in_workspace(sandbox: &Sandbox, workspace: &Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if let Some(data) = data_in_workspace(sandbox, workspace) {
        ONCE.call_once(|| {
            eprintln!(
                "ferrule: the workspace {} contains ferrule's own data ({}). To keep your saved keys out of reach, \
                 commands can't create files or folders directly in it, or in the folders on the way to the data; the rest works. \
                 A folder of its own, like ~/ferrule-workspace, avoids this.",
                setup::tilde(workspace),
                setup::tilde(&data)
            )
        });
    }
}

/// The data dir, when it sits inside `workspace` under an active Landlock
/// sandbox — the one case where commands lose write access to the
/// workspace root. Seatbelt denies by rule and has no such limit.
fn data_in_workspace(sandbox: &Sandbox, workspace: &Path) -> Option<PathBuf> {
    let data = config::data_dir().ok()?.canonicalize().ok()?;
    (cfg!(target_os = "linux") && sandbox.is_active() && data.starts_with(workspace))
        .then_some(data)
}

/// Built (and probed) once per process — the gateway builds an agent per
/// session and shouldn't fork a probe for each. A `[sandbox]` edit takes a
/// restart to apply.
fn shared_sandbox(cfg: &config::Config) -> Result<Arc<Sandbox>> {
    static SANDBOX: OnceLock<Arc<Sandbox>> = OnceLock::new();
    if let Some(sandbox) = SANDBOX.get() {
        return Ok(sandbox.clone());
    }
    let mut sandbox = Sandbox::new(sandbox_policy(cfg)).map_err(|e| anyhow!(e))?;
    let broker = shared_broker(cfg)?;
    for w in secrets_warnings(cfg, &sandbox, broker) {
        eprintln!("ferrule: {w}");
    }
    if let Some(broker) = broker {
        let ca_cert_pem = std::fs::read_to_string(broker.ca_cert_path())
            .with_context(|| format!("reading {}", broker.ca_cert_path().display()))?;
        sandbox = sandbox
            .with_env(broker.child_env())
            .with_egress(Some(Egress {
                proxy_url: broker.proxy_url(),
                ca_cert_pem,
            }));
    }
    Ok(SANDBOX.get_or_init(|| Arc::new(sandbox)).clone())
}

/// The credential proxy behind `[secrets]`, started once per process on the
/// current runtime and kept for its lifetime. `None` without `[secrets]`, or
/// when none of them is set.
fn shared_broker(cfg: &config::Config) -> Result<Option<&'static Broker>> {
    static BROKER: OnceLock<Option<Broker>> = OnceLock::new();
    if let Some(broker) = BROKER.get() {
        return Ok(broker.as_ref());
    }
    let broker = if cfg.secrets.is_empty() {
        None
    } else {
        let cfg = BrokerConfig {
            secrets: cfg
                .secrets
                .iter()
                .map(|(name, spec)| (name.clone(), spec.into()))
                .collect(),
            state_dir: config::data_dir()?.join("proxy"),
            upstream: Upstream::from_env()?,
            ca_bundle: None,
        };
        Broker::start(cfg, |name| std::env::var(name).ok())?
    };
    // A racing caller's broker is dropped (and stopped) here; both get the winner.
    Ok(BROKER.get_or_init(|| broker).as_ref())
}

/// What makes `[secrets]` weaker than it looks in this setup.
fn secrets_warnings(
    cfg: &config::Config,
    sandbox: &Sandbox,
    broker: Option<&Broker>,
) -> Vec<String> {
    if cfg.secrets.is_empty() {
        return Vec::new();
    }
    let Some(broker) = broker else {
        let names: Vec<&str> = cfg.secrets.keys().map(String::as_str).collect();
        return vec![format!(
            "[secrets]: none of {} is set in ferrule's environment, so commands get none of them",
            names.join(", ")
        )];
    };
    let mut warnings = broker.warnings().to_vec();
    if !sandbox.is_active() {
        warnings.push(
            "[secrets] without an active sandbox: commands can read ferrule's own environment, \
             real values included; the placeholders only keep them out of the transcript"
                .into(),
        );
    } else if !sandbox.policy().network {
        warnings.push(
            "[sandbox] network = false: commands can't reach the credential proxy, so [secrets] \
             can't be used"
                .into(),
        );
    }
    warnings
}

fn spawn_renderer(show_reasoning: bool) -> mpsc::Sender<AgentEvent> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::AssistantText { text } => println!("\n\x1b[1massistant:\x1b[0m {text}"),
                AgentEvent::Reasoning { text } if show_reasoning => {
                    println!(
                        "\x1b[90m[reasoning: {}…]\x1b[0m",
                        text.chars().take(200).collect::<String>()
                    )
                }
                AgentEvent::ToolCallStarted {
                    name, arguments, ..
                } => {
                    let args = arguments.to_string();
                    println!(
                        "\x1b[36m▶ {name}\x1b[0m {}",
                        args.chars().take(160).collect::<String>()
                    )
                }
                AgentEvent::ToolCallFinished {
                    name,
                    ok,
                    output_chars,
                    ..
                } => {
                    println!(
                        "\x1b[90m  {} {name} ({output_chars} chars)\x1b[0m",
                        if ok { "✓" } else { "✗" }
                    )
                }
                AgentEvent::Compacted {
                    folded_messages,
                    est_tokens_before,
                    est_tokens_after,
                } => {
                    println!("\x1b[33m[compacted {folded_messages} messages: ~{est_tokens_before} → ~{est_tokens_after} tokens]\x1b[0m")
                }
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                } => {
                    println!("\x1b[90m  [usage: in {input_tokens} (cached {cached_input_tokens}) / out {output_tokens}]\x1b[0m")
                }
                AgentEvent::ProviderRetry {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error,
                } => {
                    println!("\x1b[33m[provider failed, retry {attempt}/{max_attempts} in {:.1}s: {error}]\x1b[0m", delay_ms as f64 / 1000.0)
                }
                AgentEvent::Stuck { note } => println!("\x1b[33m{note}\x1b[0m"),
                AgentEvent::VerifyStarted { check } => println!("\x1b[36m▶ check\x1b[0m {check}"),
                AgentEvent::VerifyFinished { check, ok } => {
                    println!(
                        "\x1b[90m  {} check `{check}`\x1b[0m",
                        if ok { "✓" } else { "✗" }
                    )
                }
                AgentEvent::RunIncomplete { reason, .. } => {
                    println!("\x1b[33m[stopped: {reason}]\x1b[0m")
                }
                AgentEvent::Error { message } => eprintln!("\x1b[31merror: {message}\x1b[0m"),
                _ => {}
            }
        }
    });
    tx
}

async fn run_once(
    prompt: &str,
    provider: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    show_reasoning: bool,
) -> Result<()> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut agent = build_agent(provider, workspace, max_iterations, &session_id, "run").await?;
    let tx = spawn_renderer(show_reasoning);
    let answer = agent.run(prompt, tx).await;
    match answer {
        Ok(text) => {
            match &agent.incomplete {
                Some(reason) => println!("\n\x1b[1;33mincomplete ({reason}):\x1b[0m {text}"),
                None => println!("\n\x1b[1;32mfinal:\x1b[0m {text}"),
            }
            let u = &agent.usage;
            println!(
                "\x1b[90m[total usage: in {} (cached {}) / out {}]\x1b[0m",
                u.input_tokens, u.cached_input_tokens, u.output_tokens
            );
            // Scripts can tell a status answer from a finished job.
            if agent.incomplete.is_some() {
                std::process::exit(2);
            }
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
            Ok(text) => match &agent.incomplete {
                Some(reason) => println!("\n\x1b[1;33magent (incomplete: {reason}):\x1b[0m {text}"),
                None => println!("\n\x1b[1;32magent:\x1b[0m {text}"),
            },
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
        let token = std::env::var(env_var).map_err(|_| {
            anyhow!("env var `{env_var}` not set (needed by [gateway].telegram_token_env) — run `ferrule setup`, or export it")
        })?;
        let telegram: Arc<dyn Channel> = Arc::new(
            TelegramChannel::with_base_url(token, cfg.gateway.telegram_base_url.clone())
                .with_allowed_chats(cfg.gateway.telegram_allowed_chats.clone()),
        );
        named_channels.insert(telegram.name().to_string(), telegram);
    }

    Ok(named_channels)
}

async fn run_gateway(
    provider: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let sessions_dir = config::data_dir()?.join("sessions");

    let sandbox = shared_sandbox(&cfg)?;
    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers, sandbox, &workspace).await;
    let ledger_sink = ledger::build_sink(&cfg);
    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |session_id, transcript| {
        let (shape, origin) = ledger::classify_session(session_id);
        let tag = ledger::LedgerTag::new(&ledger_sink, shape, origin);
        build_agent_from(
            factory_provider.clone(),
            factory_workspace.clone(),
            max_iterations,
            Some(transcript),
            &mcp_tools,
            tag,
        )
        .map_err(|e| ferrule_gateway::GatewayError::Channel(e.to_string()))
    });

    let named_channels = build_channels(&cfg)?;
    if named_channels.is_empty() {
        bail!("no channel enabled in [gateway] — run `ferrule setup`, or set `local = true` and/or `telegram_token_env` in the config");
    }
    if named_channels.contains_key("telegram") && cfg.gateway.telegram_allowed_chats.is_empty() {
        tracing::warn!(
            "telegram_allowed_chats is empty: the bot answers nobody, it only replies with each chat's id. \
             Run `ferrule setup` or add the id to [gateway] telegram_allowed_chats"
        );
    }
    let adapters: Vec<Arc<dyn Channel>> = named_channels.values().cloned().collect();

    // Arc'd so the same router serves both the gateway's channel adapters
    // and the scheduler's task-triggered turns — one router, two front
    // doors (see `ferrule_gateway::Gateway::new`'s doc comment).
    let router = Arc::new(Router::new(
        sessions_dir,
        agent_factory,
        named_channels.clone(),
    ));

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
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

async fn tasks_cmd(op: TasksCmd) -> Result<()> {
    match op {
        TasksCmd::Add {
            name,
            kind,
            schedule,
            timezone,
            channel,
            chat_id,
            prompt,
            gate,
        } => {
            let kind = parse_task_kind(&kind)?;
            let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
            let now = chrono::Utc::now();
            let next_run_at =
                ferrule_gateway::initial_next_run_at(kind, &schedule, &timezone, now)?;
            let id = uuid::Uuid::new_v4().to_string();
            let task = store.add(
                NewTask {
                    name,
                    kind,
                    schedule,
                    timezone,
                    channel,
                    chat_id,
                    prompt,
                    gate,
                },
                id,
                now.timestamp(),
                next_run_at,
            )?;
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
                    if t.kind == TaskKind::Cron {
                        "cron"
                    } else {
                        "once"
                    },
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
                let status = r.status.as_str();
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
        TasksCmd::RunNow {
            id,
            provider,
            workspace,
            max_iterations,
        } => {
            tasks_run_now(&id, provider, workspace, max_iterations).await?;
        }
    }
    Ok(())
}

async fn tasks_run_now(
    id: &str,
    provider: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let sessions_dir = config::data_dir()?.join("sessions");
    let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
    let task = store
        .get(id)?
        .ok_or_else(|| anyhow!("task `{id}` not found"))?;

    let sandbox = shared_sandbox(&cfg)?;
    let mcp_tools = connect_mcp_servers(&cfg.mcp.servers, sandbox, &workspace).await;
    let ledger_sink = ledger::build_sink(&cfg);
    let factory_provider = provider;
    let factory_workspace = workspace.clone();
    let agent_factory: ferrule_gateway::AgentFactory = Arc::new(move |session_id, transcript| {
        let (shape, origin) = ledger::classify_session(session_id);
        let tag = ledger::LedgerTag::new(&ledger_sink, shape, origin);
        build_agent_from(
            factory_provider.clone(),
            factory_workspace.clone(),
            max_iterations,
            Some(transcript),
            &mcp_tools,
            tag,
        )
        .map_err(|e| ferrule_gateway::GatewayError::Channel(e.to_string()))
    });

    let named_channels = build_channels(&cfg)?;
    let router = Arc::new(Router::new(
        sessions_dir,
        agent_factory,
        named_channels.clone(),
    ));
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
        Ok(RunOutcome::Incomplete { answer, reason }) => {
            println!("incomplete ({reason}):\n{answer}")
        }
        Ok(RunOutcome::Skipped { reason }) => println!(
            "skipped: {}",
            reason.unwrap_or_else(|| "(no reason given)".into())
        ),
        Ok(RunOutcome::AlreadyRunning) => {
            println!("a run for this task is already in progress; try again shortly")
        }
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
        if cfg.project {
            ""
        } else {
            " (project skills off)"
        }
    );
    for s in &set.skills {
        let offered = if s.model_invocable { "model" } else { "hidden" };
        println!(
            "  {:<32} {:<8} {:<7} {}",
            s.name,
            s.scope.as_str(),
            offered,
            s.location.display()
        );
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
    let cfg = match config::Config::load() {
        Ok((cfg, _)) => Some(cfg),
        Err(e) => {
            eprintln!("({e} — showing default [sandbox] settings)");
            None
        }
    };
    let policy = cfg
        .as_ref()
        .map(sandbox_policy)
        .unwrap_or_else(|| ferrule_sandbox::Policy {
            hidden: hidden_paths(),
            ..Default::default()
        });
    let workspace = workspace.canonicalize()?;
    let mut sandbox = Sandbox::new(policy).map_err(|e| anyhow!(e))?;
    let broker = match &cfg {
        Some(cfg) => shared_broker(cfg)?,
        None => None,
    };
    let warnings = cfg
        .as_ref()
        .map(|cfg| secrets_warnings(cfg, &sandbox, broker))
        .unwrap_or_default();
    if let Some(broker) = broker {
        sandbox = sandbox.with_env(broker.child_env());
    }
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
    println!(
        "mode      {mode}{}",
        if policy.require { " (required)" } else { "" }
    );
    if sandbox.is_active() {
        println!(
            "network   {}",
            if policy.network { "allowed" } else { "blocked" }
        );
        println!(
            "writable  {}",
            if roots.is_empty() {
                "nothing (except /dev/null)".into()
            } else {
                roots[0].display().to_string()
            }
        );
        for root in roots.iter().skip(1) {
            println!("          {}", root.display());
        }
    }
    println!(
        "env       {} secret var(s) withheld{}",
        withheld.len(),
        if withheld.is_empty() {
            String::new()
        } else {
            format!(": {}", withheld.join(", "))
        }
    );
    if let Some(broker) = broker {
        for (i, s) in broker.secrets().iter().enumerate() {
            let hosts: Vec<String> = s.hosts.iter().map(ToString::to_string).collect();
            let url = if s.in_url { " (URL too)" } else { "" };
            println!(
                "{}{} → {}{url}",
                if i == 0 { "secrets   " } else { "          " },
                s.name,
                hosts.join(", ")
            );
        }
        println!(
            "          placeholders swapped by the proxy on {}",
            broker.addr()
        );
    }
    for w in &warnings {
        println!("warning   {w}");
    }

    let mut failures = 0;
    let mut report = |what: &str, ok: bool| {
        println!("  {}  {what}", if ok { "ok  " } else { "FAIL" });
        failures += usize::from(!ok);
    };
    let sh = |script: &str, arg: &Path| -> Result<std::process::Output> {
        Ok(sandbox
            .command(
                "/bin/sh",
                [
                    "-c".as_ref(),
                    script.as_ref(),
                    "sh".as_ref(),
                    arg.as_os_str(),
                ],
                &workspace,
            )?
            .stdin(std::process::Stdio::null())
            .output()?)
    };
    println!("\nchecks");

    let env = if cfg!(windows) {
        sandbox
            .command("cmd", ["/d", "/c", "set"], &workspace)?
            .stdin(std::process::Stdio::null())
            .output()?
    } else {
        sh("env", Path::new(""))?
    };
    let env = String::from_utf8_lossy(&env.stdout);
    let brokered: Vec<&str> = broker
        .map(|b| b.secrets().iter().map(|s| s.name.as_str()).collect())
        .unwrap_or_default();
    report(
        "secret env vars are not visible to commands",
        withheld
            .iter()
            .filter(|name| !brokered.contains(&name.as_str()))
            .all(|name| !env.lines().any(|l| l.starts_with(&format!("{name}=")))),
    );
    if let Some(broker) = broker {
        let placeholders_only = broker.secrets().iter().all(|s| {
            env.lines()
                .any(|l| l == format!("{}={}", s.name, s.placeholder))
                && std::env::var(&s.name).map_or(true, |real| !env.contains(&real))
        });
        report(
            "[secrets] reach commands as placeholders only",
            placeholders_only,
        );
    }
    if !sandbox.is_active() {
        println!("  (no sandbox: nothing else to check)");
        return if failures == 0 {
            Ok(())
        } else {
            bail!("{failures} check(s) failed")
        };
    }

    let probe_name = format!(".ferrule-sandbox-probe-{}", std::process::id());
    let inside = workspace.join(&probe_name);
    let wrote = sh("echo probe > \"$1\"", &inside)?.status.success();
    let _ = std::fs::remove_file(&inside);
    match policy.mode {
        Mode::ReadOnly => report("workspace write is refused (read-only)", !wrote),
        _ => report("workspace write works", wrote),
    }
    if let Some(data) =
        data_in_workspace(&sandbox, &workspace).filter(|_| !wrote && policy.mode != Mode::ReadOnly)
    {
        println!("        the workspace holds ferrule's data ({}), so commands can't create files or folders at its top level;", setup::tilde(&data));
        println!("        inside existing folders they can. A folder of its own, like ~/ferrule-workspace, avoids this.");
    }

    // Aim outside every root, at a place this process itself can write — so
    // a refusal is the sandbox, not plain file permissions.
    let candidates = [
        std::env::var_os("HOME").map(PathBuf::from),
        Some(PathBuf::from("/var/tmp")),
        workspace.parent().map(Path::to_path_buf),
    ];
    let target = candidates
        .into_iter()
        .flatten()
        .filter_map(|dir| dir.canonicalize().ok())
        .find(|dir| {
            let probe = dir.join(&probe_name);
            let writable =
                !roots.iter().any(|r| dir.starts_with(r)) && std::fs::write(&probe, "").is_ok();
            let _ = std::fs::remove_file(&probe);
            writable
        });
    match target {
        Some(dir) => {
            let probe = dir.join(&probe_name);
            let out = sh("echo probe > \"$1\"", &probe)?;
            let escaped = out.status.success() || probe.exists();
            let _ = std::fs::remove_file(&probe);
            report(
                &format!("write outside the roots is refused ({})", dir.display()),
                !escaped,
            );
        }
        None => {
            println!("  skip  write outside the roots (no writable dir outside them to aim at)")
        }
    }

    // Scrubbing the env is moot if a command can read it out of ferrule
    // itself; Landlock's ptrace scoping is what refuses this.
    if cfg!(target_os = "linux") {
        let environ = PathBuf::from(format!("/proc/{}/environ", std::process::id()));
        let read = sh("cat \"$1\" > /dev/null", &environ)?.status.success();
        report(
            "ferrule's own environment is unreadable (/proc/<pid>/environ)",
            !read,
        );
    }
    let saved = secrets::path()?;
    if saved.exists() {
        let read = sh("cat \"$1\" > /dev/null", &saved)?.status.success();
        report(
            &format!("the saved keys are unreadable ({})", saved.display()),
            !read,
        );
    }

    let me = std::env::current_exe()?;
    let net = sandbox
        .command(&me, ["sandbox", "--probe-net"], &workspace)?
        .stdin(std::process::Stdio::null())
        .output()?;
    let opened = net.status.success();
    if policy.network {
        report("network sockets open", opened);
    } else {
        report(
            "network sockets are refused",
            !opened && String::from_utf8_lossy(&net.stdout).contains("refused"),
        );
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

/// `ferrule config path`: where everything setup writes lives.
fn config_path_cmd() -> Result<()> {
    match config::config_path()? {
        Some(path) => println!("config    {}", path.display()),
        None => println!(
            "config    none yet (`ferrule setup` writes {})",
            config::global_config_path()?.display()
        ),
    }
    let keys = secrets::path()?;
    println!(
        "keys      {}{}",
        keys.display(),
        if keys.exists() { "" } else { " (none saved)" }
    );
    println!("data      {}", config::data_dir()?.display());
    if let service::Status::Installed { unit, .. } = service::status() {
        println!("service   {}", unit.display());
    }
    Ok(())
}

/// `ferrule config edit`: open the config in the user's editor, then say
/// whether it still parses.
fn config_edit_cmd() -> Result<()> {
    let Some(path) = config::config_path()? else {
        bail!("there's no config yet; `ferrule setup` writes one");
    };
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|dirs| std::env::split_paths(&dirs).any(|dir| dir.join(name).is_file()))
    };
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|e| !e.trim().is_empty()))
        .or_else(|| {
            ["nano", "vi"]
                .into_iter()
                .find(|e| on_path(e))
                .map(str::to_string)
        })
        .or_else(|| cfg!(windows).then(|| "notepad".to_string()))
        .ok_or_else(|| anyhow!("no editor found; set $EDITOR"))?;
    // $EDITOR may carry arguments, like "code --wait".
    let mut words = editor.split_whitespace();
    let program = words.next().ok_or_else(|| anyhow!("$EDITOR is blank"))?;
    let status = std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .status()
        .map_err(|e| anyhow!("couldn't start `{editor}`: {e}"))?;
    if !status.success() {
        bail!("`{editor}` exited with {status}");
    }
    let text = std::fs::read_to_string(&path)?;
    if let Err(e) = toml::from_str::<config::Config>(&text) {
        bail!("{} doesn't parse any more:\n{e}", path.display());
    }
    println!("✓ {} parses", path.display());
    if matches!(
        service::status(),
        service::Status::Installed { running: true, .. }
    ) {
        println!("  the background service still runs the old settings: `ferrule setup` → Background service → Restart it");
    }
    Ok(())
}

fn ledger_cmd(since: Option<String>) -> Result<()> {
    let since = since
        .map(|s| ledger::parse_since(&s, chrono::Utc::now()))
        .transpose()?;
    let path = ledger::ledger_path()?;
    let (records, malformed) = ledger::read_records(&path, since)?;
    if records.is_empty() {
        println!(
            "no ledger rows{} in {}",
            if since.is_some() {
                " in that window"
            } else {
                ""
            },
            path.display()
        );
    } else {
        println!("{}", ledger::render_table(&ledger::aggregate(&records)));
    }
    if malformed > 0 {
        eprintln!("skipped {malformed} malformed line(s)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_dir_names_are_safe_and_distinct() {
        assert_eq!(mcp_dir_name("github"), "github");
        assert_eq!(mcp_dir_name("my-fs_2"), "my-fs_2");
        let dotted = mcp_dir_name("a.b");
        assert!(dotted.starts_with("a_b-"), "{dotted}");
        assert_ne!(dotted, mcp_dir_name("a_b"));
        assert_ne!(mcp_dir_name("../x"), "../x");
        assert!(!mcp_dir_name("").is_empty());
    }
}
