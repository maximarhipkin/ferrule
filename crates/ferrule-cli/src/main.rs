mod config;

use ferrule_core::{Agent, AgentConfig, AgentEvent, HarnessProfile, ToolContext, Transcript};
use ferrule_memory::MemoryStore;
use ferrule_providers::OpenAiCompatProvider;
use ferrule_tools::standard_registry;
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
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
    }
    Ok(())
}

fn build_agent(
    provider_name: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    session_id: &str,
) -> Result<Agent> {
    let (cfg, _) = config::Config::load()?;
    let (name, pcfg, key) = cfg.resolve_provider(provider_name.as_deref())?;
    let provider = Arc::new(OpenAiCompatProvider::new(name, &pcfg.base_url, key, &pcfg.model));
    let profile = HarnessProfile::by_name(&pcfg.profile);

    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let tool_ctx = ToolContext { workspace, max_output_chars: 30_000 };

    let sessions_dir = config::data_dir()?.join("sessions");
    let transcript = Transcript::create(&sessions_dir, session_id).ok();

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
