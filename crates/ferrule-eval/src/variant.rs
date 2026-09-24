//! The two harnesses the A/B compares, built from the same provider, model,
//! tools, sandbox and context window (see `docs/m14-eval.md`, "What each
//! variant gets").

use ferrule_core::tool::Tool;
use ferrule_core::{
    Agent, AgentConfig, ContextOverflow, HarnessProfile, Provider, ToolContext, Transcript,
};
use ferrule_sandbox::{Mode, Sandbox};
use ferrule_tools::{
    standard_registry, CommandVerifier, ListDirTool, ReadFileTool, ShellTool, WebFetchTool,
    WriteFileTool,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    /// ferrule as it ships.
    Engineered,
    /// What ferrule replaced: truncation, no verification, no memory, no
    /// retries, no stuck detector, a one-line prompt.
    Naive,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Engineered => "engineered",
            Variant::Naive => "naive",
        }
    }

    pub fn parse(s: &str) -> Option<Vec<Variant>> {
        match s {
            "engineered" => Some(vec![Variant::Engineered]),
            "naive" => Some(vec![Variant::Naive]),
            "ab" | "both" => Some(vec![Variant::Engineered, Variant::Naive]),
            _ => None,
        }
    }
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Builds the memory tools on a given database (the CLI's `remember` and
/// `recall`); the engineered variant gets them on a fresh database per run.
pub type MemoryTools = Arc<dyn Fn(PathBuf) -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// The profile both variants start from, with the window override applied:
/// the model is managed as if it had `window` tokens.
pub fn windowed(profile: &HarnessProfile, window: Option<usize>) -> HarnessProfile {
    let mut p = profile.clone();
    if let Some(n) = window {
        p.context_window = n;
        p.output_reserve = p.output_reserve.min(n / 4);
    }
    p
}

/// What goes into one agent.
pub struct Build<'a> {
    pub variant: Variant,
    pub provider: Arc<dyn Provider>,
    pub profile: &'a HarnessProfile,
    pub sandbox: &'a Arc<Sandbox>,
    pub memory_tools: Option<&'a MemoryTools>,
    pub workspace: &'a Path,
    pub state: &'a Path,
    pub max_iterations: usize,
    /// The engineered variant's verify command.
    pub check: Option<&'a str>,
    pub transcript: Option<Transcript>,
}

/// The base of ferrule's own system prompt, kept in step with the CLI's
/// `build_agent_from`.
fn engineered_prompt(workspace: &Path, directive: &str) -> String {
    format!(
        "You are an autonomous agent running inside ferrule. Workspace: {}. \
         Use tools to act on the world; verify with evidence; persist important facts with the remember tool when asked. \
         For multi-step work, maintain your task list with write_todos and log decisions with log_diary. {}",
        workspace.display(),
        directive
    )
}

/// The naive harness's whole system prompt.
fn naive_prompt(workspace: &Path) -> String {
    format!(
        "You are an autonomous agent. Workspace: {}. Use the tools to complete the task.",
        workspace.display()
    )
}

pub fn build(b: Build<'_>) -> Agent {
    let tool_ctx = ToolContext {
        workspace: b.workspace.to_path_buf(),
        max_output_chars: 30_000,
    };
    let mut registry = standard_registry();
    registry.register(Arc::new(ShellTool::sandboxed(b.sandbox.clone())));
    registry.register(Arc::new(WebFetchTool::with_egress(
        b.sandbox.egress().cloned(),
    )));
    let hidden = b.sandbox.policy().hidden.clone();
    registry.register(Arc::new(ReadFileTool::hiding(hidden.clone())));
    registry.register(Arc::new(WriteFileTool::hiding(hidden.clone())));
    registry.register(Arc::new(ListDirTool::hiding(hidden)));
    if b.sandbox.policy().mode == Mode::ReadOnly {
        registry.remove("write_file");
    }

    let mut profile = b.profile.clone();
    let mut config = AgentConfig {
        max_iterations: b.max_iterations,
        ..Default::default()
    };
    let system = match b.variant {
        Variant::Naive => {
            profile.retain_reasoning = false;
            profile.compaction_threshold = 1.0;
            profile.system_directive = String::new();
            config.overflow = ContextOverflow::Truncate;
            config.detect_stuck = false;
            config.retry.max_attempts = 1;
            naive_prompt(b.workspace)
        }
        Variant::Engineered => {
            let mut system = engineered_prompt(b.workspace, &profile.system_directive);
            if let Some((name, content)) = ferrule_core::load_context_baseline(b.workspace) {
                system.push_str(&format!(
                    "\n\n[Workspace context baseline: {name}]\n{content}"
                ));
            }
            if let Some(cmd) = b.check {
                system.push_str(&format!(
                    "\n\n[Validation policy] When you finish after changing files, ferrule runs `{cmd}`. \
                     If it fails you get its output back and keep working: fix forward, don't revert. \
                     You can run it yourself with the shell tool before finishing."
                ));
            }
            // Only the fixture's own skills: the user's would make the run
            // depend on the machine.
            let roots: Vec<_> = ferrule_skills::default_roots(b.workspace, true, &[])
                .into_iter()
                .filter(|r| r.scope == ferrule_skills::Scope::Project)
                .collect();
            let skills = Arc::new(ferrule_skills::discover(&roots, &[]));
            if let Some(catalog) = skills.catalog() {
                system.push_str(&format!("\n\n[Skills]\n{catalog}"));
                for tool in ferrule_skills::tools(skills) {
                    registry.register(tool);
                }
            }
            if let Some(make) = b.memory_tools {
                for tool in make(b.state.join("memory.db")) {
                    registry.register(tool);
                }
            }
            // M15: what compaction shortens or drops stays reachable.
            if let Some(t) = &b.transcript {
                registry.register(Arc::new(ferrule_core::SearchHistoryTool::new(t)));
            }
            system
        }
    };

    let mut agent = Agent::new(
        b.provider,
        registry,
        profile,
        config,
        tool_ctx,
        b.transcript,
    )
    .with_system_prompt(system);
    if let (Variant::Engineered, Some(cmd)) = (b.variant, b.check) {
        agent = agent.with_verifier(Arc::new(CommandVerifier::new(
            cmd,
            b.sandbox.clone(),
            Duration::from_secs(300),
        )));
    }
    agent
}
