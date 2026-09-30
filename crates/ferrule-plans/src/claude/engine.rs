//! The Claude Code engine: a `Provider` whose turn is one run of the
//! unmodified `claude -p` (design §7.1).
//!
//! A `complete()` either starts a turn (the new message on stdin, `--resume`
//! with the chat's claude session when there is one) or carries on a turn
//! that paused on a bridged ferrule tool: claude called
//! `mcp__ferrule__remember`, say, so the engine returned that call as a
//! `tool_call`, the agent ran it like any other, and the next `complete()`
//! brings its result back to the waiting MCP request.
//!
//! The claude session id rides on the answer's `native` blocks (api
//! `claude-code`), so it is in the transcript with the turn it belongs to,
//! and compaction, which drops native blocks, starts the next turn fresh
//! with a recap.

use super::bridge::{Bridge, BridgeCall, Reply, APPROVE};
use super::env::{self, Launch, WRITE_TOOLS};
use super::stream::{self, Event, RateLimit, TurnResult};
use super::token::{exported_token, Credential, TokenStore};
use async_trait::async_trait;
use ferrule_core::guard::{Guard, GuardedCall, Verdict};
use ferrule_core::message::{Message, NativeBlocks, Role, ToolCall, Usage};
use ferrule_core::provider::{
    call_context, CallContext, CompletionRequest, CompletionResponse, Delta, Provider,
};
use ferrule_core::tool::ToolDefinition;
use ferrule_core::CoreError;
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The `native` api the engine's answers carry.
pub const NATIVE_API: &str = "claude-code";
/// The plan's name in the usage file and the ledger.
pub const PLAN: &str = "claude-code";
/// With no session to resume, how much of the chat a fresh turn is told.
const RECAP_MESSAGES: usize = 12;
const RECAP_BYTES: usize = 8 * 1024;
/// How much of claude's stderr an error quotes.
const STDERR_TAIL: usize = 2048;

/// The ferrule tools a turn's claude gets through the bridge (§7.4).
/// Ferrule's file, shell and fetch tools stay out: claude has its own.
pub const BRIDGED_TOOLS: &[&str] = &[
    "remember",
    "recall",
    "forget",
    "search_history",
    "schedule_task",
    "list_tasks",
    "cancel_task",
    "send_message",
    "web_search",
    "spawn_agent",
    "board",
    "post_to_board",
    "read_board",
];

/// How the engine runs claude. One per configured model.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The `claude` binary.
    pub binary: PathBuf,
    /// `CLAUDE_CONFIG_DIR` for the child: ferrule's own by default.
    pub config_dir: PathBuf,
    /// Where a pasted setup-token is sealed. `None`: exported token or
    /// claude's own login only.
    pub private_dir: Option<PathBuf>,
    /// Where the usage file is. `None`: readings aren't kept.
    pub data_dir: Option<PathBuf>,
    /// The workspace when the run doesn't say (`CallContext`).
    pub workspace: PathBuf,
    pub turn_timeout: Duration,
    /// Past this many bytes from claude, the turn is stopped.
    pub max_output_bytes: u64,
    pub scrub_subprocess_env: bool,
    /// `ferrule claude-mcp`: the program and the args before nothing else.
    /// `None`: no bridge, so no ferrule tools and no permission prompt
    /// (claude then refuses whatever isn't allowed outright).
    pub bridge_command: Option<(PathBuf, Vec<String>)>,
    /// The M26 sandbox; claude runs under it as a helper. `None`: not
    /// confined (tests, or the sandbox off).
    pub sandbox: Option<Sandbox>,
    /// M36: updates claude when a turn failed because it is too old or its
    /// install is broken; the turn is then tried once more.
    pub repair: Option<Arc<dyn Repairer>>,
}

/// M36 §5.3: whoever can update `claude` here (directly, or by asking the
/// update unit).
#[async_trait]
pub trait Repairer: Send + Sync + std::fmt::Debug {
    /// Update claude now; `why` is the failed turn's error. `Ok` once the
    /// update ran.
    async fn update_claude(&self, why: &str) -> anyhow::Result<()>;
}

impl EngineConfig {
    pub fn new(binary: PathBuf, config_dir: PathBuf, workspace: PathBuf) -> Self {
        Self {
            binary,
            config_dir,
            private_dir: None,
            data_dir: None,
            workspace,
            turn_timeout: Duration::from_secs(20 * 60),
            max_output_bytes: 16 * 1024 * 1024,
            scrub_subprocess_env: false,
            bridge_command: None,
            sandbox: None,
            repair: None,
        }
    }

    /// The credential a turn would run on now.
    pub fn credential(&self) -> anyhow::Result<Credential> {
        match &self.private_dir {
            Some(p) => TokenStore::new(p).credential(),
            None => Ok(exported_token().map_or(Credential::ClaudeLogin, Credential::Exported)),
        }
    }
}

/// A `claude-code/<model>` model.
pub struct ClaudeCode {
    name: String,
    model: String,
    config: Arc<EngineConfig>,
    /// Turns waiting on a bridged tool's result.
    paused: Mutex<Vec<Turn>>,
}

impl std::fmt::Debug for ClaudeCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCode")
            .field("name", &self.name)
            .field("model", &self.model)
            .finish()
    }
}

impl ClaudeCode {
    pub fn new(name: impl Into<String>, model: impl Into<String>, config: EngineConfig) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            config: Arc::new(config),
            paused: Mutex::new(Vec::new()),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn paused(&self) -> std::sync::MutexGuard<'_, Vec<Turn>> {
        self.paused.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop (so kill) paused turns nobody came back for in time.
    fn reap(&self) {
        let timeout = self.config.turn_timeout;
        self.paused()
            .retain(|t| t.paused_at.is_none_or(|at| at.elapsed() < timeout));
    }

    /// Starts a turn for the chat in `req`; a claude too old for Claude's
    /// servers, or a broken install, is updated and the turn tried once more.
    async fn start(
        &self,
        req: &CompletionRequest,
        ctx: &CallContext,
    ) -> Result<CompletionResponse, CoreError> {
        let first = self.start_once(req, ctx).await;
        let (Err(e), Some(repair)) = (&first, &self.config.repair) else {
            return first;
        };
        let kind = ferrule_core::failure::classify(e);
        let fixable = match kind {
            ferrule_core::failure::Kind::ClaudeTooOld => true,
            // Installed but broken; a missing one is found, not updated.
            ferrule_core::failure::Kind::ClaudeMissing => {
                super::cli::find(&self.config.binary).is_some()
            }
            _ => false,
        };
        if !fixable {
            return first;
        }
        match repair.update_claude(&e.to_string()).await {
            Ok(()) => {
                tracing::info!(kind = %kind.name(), "claude-code: claude updated; trying the turn again");
                self.start_once(req, ctx).await
            }
            Err(fix) => {
                tracing::warn!(error = %format!("{fix:#}"), "claude-code: claude couldn't be updated");
                first
            }
        }
    }

    async fn start_once(
        &self,
        req: &CompletionRequest,
        ctx: &CallContext,
    ) -> Result<CompletionResponse, CoreError> {
        let resume = resume_id(&req.messages);
        if let Some(id) = &resume {
            // A turn of this chat that was left paused (the run stopped
            // mid-tool) is over: a new message starts a new turn.
            self.paused()
                .retain(|t| t.session_id.as_deref() != Some(id.as_str()));
        }
        let prompt = prompt(&req.messages, resume.is_some());
        let turn = self.spawn(req, ctx, resume.clone(), prompt).await?;
        match self.drive(turn, req, ctx).await? {
            Outcome::ResumeMissing if resume.is_some() => {
                tracing::info!(
                    "claude-code: the session to resume is gone; starting fresh with a recap"
                );
                let turn = self
                    .spawn(req, ctx, None, prompt_fresh(&req.messages))
                    .await?;
                match self.drive(turn, req, ctx).await? {
                    Outcome::ResumeMissing => Err(CoreError::Provider(
                        "Claude Code couldn't start a session".into(),
                    )),
                    other => self.finish(other),
                }
            }
            other => self.finish(other),
        }
    }

    fn finish(&self, outcome: Outcome) -> Result<CompletionResponse, CoreError> {
        match outcome {
            Outcome::Done(r) => Ok(r),
            Outcome::Paused(turn, r) => {
                self.paused().push(*turn);
                Ok(r)
            }
            Outcome::ResumeMissing => Err(CoreError::Provider(
                "Claude Code has no conversation to resume".into(),
            )),
        }
    }

    async fn spawn(
        &self,
        req: &CompletionRequest,
        ctx: &CallContext,
        resume: Option<String>,
        prompt: String,
    ) -> Result<Turn, CoreError> {
        let cfg = &self.config;
        let fail = |what: &str, e: &dyn std::fmt::Display| {
            CoreError::Provider(format!("Claude Code: {what}: {e}"))
        };
        std::fs::create_dir_all(&cfg.config_dir).map_err(|e| fail("the config dir", &e))?;
        env::check_config_dir(&cfg.config_dir)
            .map_err(|e| CoreError::Provider(format!("{e:#}")))?;
        let credential = cfg
            .credential()
            .map_err(|e| fail("the Claude plan's token", &format!("{e:#}")))?;
        let workspace = ctx
            .workspace
            .clone()
            .unwrap_or_else(|| cfg.workspace.clone());

        let dir = TurnDir::create(&cfg.config_dir).map_err(|e| fail("the turn's files", &e))?;
        let system: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .filter_map(|m| m.content.as_deref())
            .collect();
        let system_prompt_file = if system.is_empty() {
            None
        } else {
            let f = dir.0.join("system.md");
            ferrule_connections::seal::write_private(&f, system.join("\n\n").as_bytes())
                .map_err(|e| fail("the system prompt", &format!("{e:#}")))?;
            Some(f)
        };

        let (calls_tx, calls) = mpsc::channel(16);
        let (bridge, mcp_config) = match &cfg.bridge_command {
            Some((program, args)) => {
                let bridge = Bridge::start(bridged(&req.tools), calls_tx)
                    .await
                    .map_err(|e| fail("the tool bridge", &e))?;
                let f = dir.0.join("mcp.json");
                let doc = bridge.mcp_config(program, args);
                ferrule_connections::seal::write_private(&f, doc.to_string().as_bytes())
                    .map_err(|e| fail("the tool bridge", &format!("{e:#}")))?;
                (Some(bridge), Some(f))
            }
            None => (None, None),
        };

        let launch = Launch {
            model: self.model.clone(),
            resume: resume.clone(),
            system_prompt_file,
            mcp_config,
            config_dir: cfg.config_dir.clone(),
            token: credential.token().cloned(),
            disallowed: disallowed(&req.tools),
            scrub_subprocess_env: cfg.scrub_subprocess_env,
        };
        let parent = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok());
        let spec = env::spec(&launch, parent);
        tracing::debug!(argv = %spec.argv_line(), "claude-code: starting a turn");

        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut std_cmd = match &cfg.sandbox {
            Some(sandbox) => sandbox
                .for_helper(&cfg.config_dir, &[])
                .command(&cfg.binary, &spec.args, &workspace)
                .map_err(|e| fail("sandbox setup", &e))?,
            None => {
                let mut c = std::process::Command::new(&cfg.binary);
                c.args(&spec.args).current_dir(&workspace);
                c
            }
        };
        // Its own process group: /stop takes claude's Bash commands and
        // MCP servers down with it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut std_cmd, 0);
        let mut cmd = tokio::process::Command::from(std_cmd);
        spec.apply(&mut cmd);
        // A bridged call waits for the agent to run the tool; claude
        // mustn't give up on it first.
        cmd.env("MCP_TOOL_TIMEOUT", cfg.turn_timeout.as_millis().to_string());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CoreError::Provider(format!(
                    "Claude Code isn't installed here ({} not found): npm install -g @anthropic-ai/claude-code",
                    cfg.binary.display()
                ))
            } else {
                fail("starting claude", &e)
            }
        })?;
        let group = Group(child.id());

        let mut stdin = child.stdin.take().expect("piped");
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > STDERR_TAIL * 2 {
                        let cut = t.len() - STDERR_TAIL;
                        let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                        t.drain(..cut);
                    }
                }
            });
        }
        let (lines_tx, lines) = mpsc::channel(256);
        let stdout = child.stdout.take().expect("piped");
        let cap = cfg.max_output_bytes;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut total: u64 = 0;
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        total += n as u64;
                        if total > cap {
                            let _ = lines_tx.send(Line::TooMuch).await;
                            break;
                        }
                        let text = String::from_utf8_lossy(&line).into_owned();
                        if lines_tx.send(Line::Text(text)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Turn {
            child,
            _group: group,
            lines,
            calls,
            _bridge: bridge,
            _dir: dir,
            stderr_tail,
            pending: HashMap::new(),
            queued: Vec::new(),
            session_id: resume.clone(),
            resumed: resume.is_some(),
            limit: None,
            used: Duration::ZERO,
            started: Instant::now(),
            paused_at: None,
            model: self.model.clone(),
        })
    }

    /// Runs `turn` until it answers, fails, or pauses on bridged calls.
    async fn drive(
        &self,
        mut turn: Turn,
        req: &CompletionRequest,
        ctx: &CallContext,
    ) -> Result<Outcome, CoreError> {
        let timeout = self.config.turn_timeout;
        let deadline = tokio::time::Instant::now() + timeout.saturating_sub(turn.used);
        let mut segment = String::new();
        loop {
            // Bridged calls that came in together go back together.
            if !turn.queued.is_empty() {
                while let Ok(call) = turn.calls.try_recv() {
                    self.take_call(&mut turn, call, ctx).await;
                }
                let calls = std::mem::take(&mut turn.queued);
                let mut tool_calls = Vec::new();
                for call in calls {
                    let id = format!(
                        "claude-code-{}",
                        hex(&ferrule_connections::seal::random::<6>())
                    );
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        name: call.name,
                        arguments: call.args,
                    });
                    turn.pending.insert(id, call.reply);
                }
                let mut message = Message::assistant(
                    (!segment.is_empty()).then(|| segment.clone()),
                    tool_calls,
                    None,
                );
                message.native = turn.native();
                turn.pause();
                return Ok(Outcome::Paused(
                    Box::new(turn),
                    CompletionResponse {
                        message,
                        usage: Usage::default(),
                    },
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(CoreError::Provider(format!(
                        "Claude Code's turn ran past {} min and was stopped",
                        timeout.as_secs() / 60
                    )));
                }
                Some(call) = turn.calls.recv() => {
                    self.take_call(&mut turn, call, ctx).await;
                }
                line = turn.lines.recv() => {
                    let text = match line {
                        Some(Line::Text(t)) => t,
                        Some(Line::TooMuch) => {
                            return Err(CoreError::Provider(format!(
                                "Claude Code wrote more than {} MB in one turn; it was stopped",
                                self.config.max_output_bytes / (1024 * 1024)
                            )));
                        }
                        None => return self.ended_early(&mut turn).await,
                    };
                    match stream::parse(&text) {
                        Some(Event::Init { session_id, model }) => {
                            turn.session_id = Some(session_id);
                            if let Some(m) = model {
                                turn.model = m;
                            }
                        }
                        Some(Event::Text(t)) => {
                            segment.push_str(&t);
                            if let Some(sink) = &req.stream {
                                sink.send(Delta::Text(t));
                            }
                        }
                        Some(Event::Progress) => {
                            if let Some(sink) = &req.stream {
                                sink.send(Delta::Progress);
                            }
                        }
                        Some(Event::RateLimit(limit)) => {
                            if let Some(data) = &self.config.data_dir {
                                crate::UsageFile::new(data).record(PLAN, limit.reading(crate::now()));
                            }
                            turn.limit = Some(limit);
                        }
                        Some(Event::Result(result)) => {
                            return self.done(turn, result);
                        }
                        Some(Event::Other) | None => {}
                    }
                }
            }
        }
    }

    /// A bridged call: the permission prompt is answered here; a ferrule
    /// tool is queued, to go back to the agent.
    async fn take_call(&self, turn: &mut Turn, call: BridgeCall, ctx: &CallContext) {
        if call.name != APPROVE {
            turn.queued.push(call);
            return;
        }
        let answer = approve(ctx.guard.as_ref(), &call.args).await;
        let _ = call.reply.send(Reply::ok(answer.to_string()));
    }

    fn done(&self, turn: Turn, result: TurnResult) -> Result<Outcome, CoreError> {
        if result.is_error && turn.resumed && stream::resume_missing(&result.text) {
            return Ok(Outcome::ResumeMissing);
        }
        if let Some(e) = stream::result_error(&result, turn.limit.as_ref(), crate::now()) {
            return Err(e);
        }
        let mut turn = turn;
        if let Some(id) = result.session_id.clone() {
            turn.session_id = Some(id);
        }
        if let Some(m) = &result.model {
            turn.model = m.clone();
        }
        let mut message = Message::assistant(Some(result.text.clone()), vec![], None);
        message.native = turn.native();
        let mut usage = result.usage.clone();
        usage.notional_usd = result.total_cost_usd;
        Ok(Outcome::Done(CompletionResponse { message, usage }))
    }

    /// claude's stdout closed without a result.
    async fn ended_early(&self, turn: &mut Turn) -> Result<Outcome, CoreError> {
        let status = tokio::time::timeout(Duration::from_secs(5), turn.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        // Give the stderr reader a moment to catch the last words.
        tokio::task::yield_now().await;
        let tail = turn
            .stderr_tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .to_string();
        if turn.resumed && stream::resume_missing(&tail) {
            return Ok(Outcome::ResumeMissing);
        }
        let fake = TurnResult {
            is_error: true,
            text: tail.clone(),
            ..TurnResult::default()
        };
        if let Some(CoreError::Provider(m)) = stream::result_error(&fake, None, crate::now()) {
            if m == stream::NOT_SIGNED_IN {
                return Err(CoreError::Provider(m));
            }
        }
        let code = status
            .and_then(|s| s.code())
            .map_or("no exit code".to_string(), |c| format!("exit {c}"));
        let tail: String = tail
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Err(CoreError::Provider(format!(
            "Claude Code ended without an answer ({code}){}",
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        )))
    }
}

#[async_trait]
impl Provider for ClaudeCode {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let ctx = call_context();
        self.reap();
        let trailing: Vec<&Message> = req
            .messages
            .iter()
            .rev()
            .take_while(|m| m.role == Role::Tool)
            .collect();
        let answered = |t: &Turn| {
            trailing.iter().any(|m| {
                m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| t.pending.contains_key(id))
            })
        };
        let resumed = {
            let mut paused = self.paused();
            paused
                .iter()
                .position(answered)
                .map(|i| paused.swap_remove(i))
        };
        let Some(mut turn) = resumed else {
            return self.start(&req, &ctx).await;
        };
        for m in &trailing {
            if let Some(reply) = m
                .tool_call_id
                .as_deref()
                .and_then(|id| turn.pending.remove(id))
            {
                let _ = reply.send(Reply::ok(m.content.clone().unwrap_or_default()));
            }
        }
        for (_, reply) in turn.pending.drain() {
            let _ = reply.send(Reply::error("ferrule didn't run this call"));
        }
        turn.resume();
        match self.drive(turn, &req, &ctx).await? {
            Outcome::ResumeMissing => Err(CoreError::Provider(
                "Claude Code lost its session mid-turn".into(),
            )),
            other => self.finish(other),
        }
    }
}

enum Outcome {
    Done(CompletionResponse),
    Paused(Box<Turn>, CompletionResponse),
    /// `--resume` found no session: start again, fresh.
    ResumeMissing,
}

enum Line {
    Text(String),
    TooMuch,
}

/// One running `claude`. Dropping it kills the process group, closes the
/// bridge and removes the turn's files.
struct Turn {
    child: tokio::process::Child,
    _group: Group,
    lines: mpsc::Receiver<Line>,
    calls: mpsc::Receiver<BridgeCall>,
    _bridge: Option<Bridge>,
    _dir: TurnDir,
    stderr_tail: Arc<Mutex<String>>,
    /// Bridged calls handed to the agent, by tool call id.
    pending: HashMap<String, tokio::sync::oneshot::Sender<Reply>>,
    queued: Vec<BridgeCall>,
    session_id: Option<String>,
    resumed: bool,
    limit: Option<RateLimit>,
    /// Time claude has worked, across pauses; the timeout counts only this.
    used: Duration,
    started: Instant,
    paused_at: Option<Instant>,
    model: String,
}

impl Turn {
    fn native(&self) -> Option<NativeBlocks> {
        let id = self.session_id.as_ref()?;
        Some(NativeBlocks {
            api: NATIVE_API.into(),
            model: self.model.clone(),
            items: vec![json!({ "session_id": id })],
        })
    }

    fn pause(&mut self) {
        self.used += self.started.elapsed();
        self.paused_at = Some(Instant::now());
    }

    fn resume(&mut self) {
        self.started = Instant::now();
        self.paused_at = None;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Kills a process group when dropped (unix; on Windows `kill_on_drop` and
/// the sandbox's job end the tree).
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            ferrule_sandbox::kill_process_group(pid);
        }
    }
}

/// A turn's files (the appended prompt, the MCP config), removed with it.
struct TurnDir(PathBuf);

impl TurnDir {
    fn create(config_dir: &Path) -> std::io::Result<Self> {
        let dir = config_dir
            .join("ferrule-turns")
            .join(hex(&ferrule_connections::seal::random::<8>()));
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(dir))
    }
}

impl Drop for TurnDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The ferrule tools offered on the bridge: the agent's, filtered by
/// [`BRIDGED_TOOLS`].
pub fn bridged(tools: &[ToolDefinition]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .filter(|t| BRIDGED_TOOLS.contains(&t.name.as_str()))
        .cloned()
        .collect()
}

/// claude's tools this run mustn't have, from what the agent has: no
/// ferrule file-writing tool (plan mode, a read-only sub-agent) means no
/// claude edits; no shell means no `Bash`; and so on.
pub fn disallowed(tools: &[ToolDefinition]) -> Vec<String> {
    let has = |n: &str| tools.iter().any(|t| t.name == n);
    let mut out = Vec::new();
    if !has("write_file") && !has("edit_file") {
        out.extend(WRITE_TOOLS.iter().map(|s| s.to_string()));
    }
    if !has("shell") {
        out.push("Bash".into());
    }
    if !has("web_fetch") {
        out.push("WebFetch".into());
    }
    if !has("web_search") {
        out.push("WebSearch".into());
    }
    if !has("spawn_agent") {
        out.push("Task".into());
    }
    out
}

/// A claude tool as the ferrule call the guard judges (§7.5): the name,
/// the arguments, and whether it changes files.
pub fn ferrule_call(tool: &str, input: &Value) -> (String, Value, bool) {
    let path = || input["file_path"].clone();
    match tool {
        "Bash" => ("shell".into(), json!({"command": input["command"]}), true),
        "Edit" | "MultiEdit" => ("edit_file".into(), json!({"path": path()}), true),
        "Write" => ("write_file".into(), json!({"path": path()}), true),
        "NotebookEdit" => (
            "edit_file".into(),
            json!({"path": input["notebook_path"]}),
            true,
        ),
        "WebFetch" => ("web_fetch".into(), json!({"url": input["url"]}), false),
        "WebSearch" => ("web_search".into(), json!({"query": input["query"]}), false),
        "Task" | "Agent" => ("spawn_agent".into(), input.clone(), false),
        // Unknown: judged as a tool that may change files.
        other => (other.to_string(), input.clone(), true),
    }
}

/// The permission prompt's answer for `args` (`{tool_name, input}`).
pub async fn approve(guard: Option<&Arc<dyn Guard>>, args: &Value) -> Value {
    let tool = args["tool_name"].as_str().unwrap_or("");
    let input = args.get("input").cloned().unwrap_or_else(|| json!({}));
    let allow = || json!({"behavior": "allow", "updatedInput": input});
    // The bridge's tools are gated again when they run as ferrule tools.
    if tool.starts_with(&format!("mcp__{}__", env::BRIDGE_SERVER)) {
        return allow();
    }
    let Some(guard) = guard else {
        return allow();
    };
    let (name, call_args, changes_files) = ferrule_call(tool, &input);
    let verdict = guard
        .before_tool_call(GuardedCall {
            tool: &name,
            args: &call_args,
            changes_files,
            needs_approval: false,
        })
        .await;
    match verdict {
        Verdict::Allow => allow(),
        Verdict::Refuse(why) => json!({"behavior": "deny", "message": why}),
    }
}

/// The claude session this chat's last engine answer ran in, unless
/// something else answered since (another model after a fallback): then
/// that model's turns aren't in claude's session, and a recap is needed.
pub fn resume_id(messages: &[Message]) -> Option<String> {
    let last = messages.iter().rposition(|m| m.role == Role::Assistant)?;
    let native = messages[last].native.as_ref()?;
    if native.api != NATIVE_API {
        return None;
    }
    native.items.first()?["session_id"]
        .as_str()
        .map(str::to_string)
}

/// What goes on claude's stdin: with a session to resume, what came after
/// its last answer (the new message); without, a recap and the message.
pub fn prompt(messages: &[Message], resuming: bool) -> String {
    if !resuming {
        return prompt_fresh(messages);
    }
    let last = messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .map_or(0, |i| i + 1);
    let parts: Vec<String> = messages[last..]
        .iter()
        .filter_map(|m| match m.role {
            Role::User => Some(ferrule_core::vision::text_with_notes(
                m,
                ferrule_core::vision::NoteWhy::TextOnly,
            ))
            .filter(|t| !t.is_empty()),
            Role::Tool => m
                .content
                .as_deref()
                .map(|c| format!("(A tool result from before: {})", clip(c, 2000))),
            _ => None,
        })
        .collect();
    parts.join("\n\n")
}

/// A fresh turn's stdin: the last messages before the new one, bounded,
/// then the new one.
pub fn prompt_fresh(messages: &[Message]) -> String {
    let Some(last_user) = messages.iter().rposition(|m| m.role == Role::User) else {
        return String::new();
    };
    let new = ferrule_core::vision::text_with_notes(
        &messages[last_user],
        ferrule_core::vision::NoteWhy::TextOnly,
    );
    let mut lines: Vec<String> = messages[..last_user]
        .iter()
        .filter_map(|m| {
            let text = m.content.as_deref().filter(|c| !c.trim().is_empty())?;
            let who = match m.role {
                Role::User => "User",
                Role::Assistant => "You",
                _ => return None,
            };
            Some(format!("{who}: {}", clip(text, 2000)))
        })
        .collect();
    if lines.len() > RECAP_MESSAGES {
        lines.drain(..lines.len() - RECAP_MESSAGES);
    }
    while lines.iter().map(String::len).sum::<usize>() > RECAP_BYTES && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.is_empty() {
        return new;
    }
    format!(
        "Earlier in this conversation (a recap, not the whole of it):\n\n{}\n\nThe new message:\n\n{new}",
        lines.join("\n\n")
    )
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(id: &str) -> Message {
        let mut m = Message::assistant(Some("hi".into()), vec![], None);
        m.native = Some(NativeBlocks {
            api: NATIVE_API.into(),
            model: "haiku".into(),
            items: vec![json!({"session_id": id})],
        });
        m
    }

    #[test]
    fn a_photo_reaches_claude_code_as_a_note() {
        let img = ferrule_core::ImageRef {
            path: "/w/inbox/cat.jpg".into(),
            mime: "image/jpeg".into(),
            name: Some("cat.jpg".into()),
        };
        let msgs = vec![
            Message::system("s"),
            Message::user_with_images("what is this?", vec![img]),
        ];
        for text in [prompt(&msgs, true), prompt(&msgs, false)] {
            assert!(text.contains("what is this?"), "{text}");
            assert!(
                text.contains("can't see images") && text.contains("cat.jpg"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_chat_resumes_its_session_and_a_fresh_one_gets_a_bounded_recap() {
        let mut msgs = vec![
            Message::system("persona"),
            Message::user("first"),
            native("s-1"),
            Message::user("second"),
        ];
        assert_eq!(resume_id(&msgs).as_deref(), Some("s-1"));
        assert_eq!(prompt(&msgs, true), "second");

        // Another model answered since (a fallback): no resume.
        msgs.push(Message::assistant(Some("from gpt".into()), vec![], None));
        msgs.push(Message::user("third"));
        assert_eq!(resume_id(&msgs), None);
        let p = prompt(&msgs, false);
        assert!(
            p.contains("User: first") && p.contains("You: from gpt"),
            "{p}"
        );
        assert!(p.ends_with("The new message:\n\nthird"), "{p}");
        assert!(
            !p.contains("persona"),
            "the persona goes in the system prompt"
        );

        let mut long = vec![];
        for i in 0..40 {
            long.push(Message::user(format!("{i} {}", "x".repeat(1500))));
            long.push(Message::assistant(Some("ok".into()), vec![], None));
        }
        long.push(Message::user("now"));
        let p = prompt_fresh(&long);
        assert!(p.len() < RECAP_BYTES + 200, "{}", p.len());
        assert!(!p.contains("User: 0 "));
        assert_eq!(prompt_fresh(&[Message::user("only")]), "only");
    }

    #[test]
    fn claude_loses_tools_the_agent_does_not_have() {
        let def = |n: &str| ToolDefinition {
            name: n.into(),
            description: String::new(),
            parameters: json!({}),
        };
        let none = disallowed(&[]);
        for t in ["Edit", "Write", "Bash", "WebFetch", "Task"] {
            assert!(none.contains(&t.to_string()), "{t}");
        }
        let full = disallowed(&[
            def("write_file"),
            def("shell"),
            def("web_fetch"),
            def("web_search"),
            def("spawn_agent"),
        ]);
        assert!(full.is_empty(), "{full:?}");
        let b = bridged(&[def("remember"), def("shell"), def("send_message")]);
        let names: Vec<_> = b.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["remember", "send_message"]);

        assert_eq!(
            ferrule_call("Bash", &json!({"command": "ls"})),
            ("shell".into(), json!({"command": "ls"}), true)
        );
        assert_eq!(
            ferrule_call("Write", &json!({"file_path": "a"})).0,
            "write_file"
        );
    }
}
