//! The Gemini CLI engine: a `Provider` whose turn is one run of the
//! unmodified `gemini` binary (`@google/gemini-cli`) signed in via Google
//! OAuth.

use super::env::{self, Launch};
use super::stream::{self, Event, TurnResult};
use crate::claude::bridge::{Bridge, BridgeCall, Reply, APPROVE};
use crate::claude::engine::{bridged, prompt, prompt_fresh, with_deferred, with_notes};
use async_trait::async_trait;
use ferrule_core::message::{Message, NativeBlocks, Role, ToolCall, Usage};
use ferrule_core::provider::{
    call_context, CallContext, CompletionRequest, CompletionResponse, Delta, Provider,
};
use ferrule_core::CoreError;
use ferrule_sandbox::Sandbox;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The `native` api the engine's answers carry.
pub const NATIVE_API: &str = "gemini-cli";
/// The plan's name in the usage file and the ledger.
pub const PLAN: &str = "gemini-cli";
const STDERR_TAIL: usize = 2048;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub binary: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub workspace: PathBuf,
    pub turn_timeout: Duration,
    pub max_output_bytes: u64,
    pub bridge_command: Option<(PathBuf, Vec<String>)>,
    pub sandbox: Option<Sandbox>,
    pub guest_hidden: Vec<PathBuf>,
}

impl EngineConfig {
    pub fn new(binary: PathBuf, config_dir: PathBuf, workspace: PathBuf) -> Self {
        Self {
            binary,
            config_dir,
            data_dir: None,
            workspace,
            turn_timeout: Duration::from_secs(20 * 60),
            max_output_bytes: 16 * 1024 * 1024,
            bridge_command: None,
            sandbox: None,
            guest_hidden: Vec::new(),
        }
    }
}

/// A `gemini-cli/<model>` model.
pub struct GeminiCli {
    name: String,
    model: String,
    config: Arc<EngineConfig>,
    paused: Arc<Mutex<Vec<Turn>>>,
}

impl std::fmt::Debug for GeminiCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiCli")
            .field("name", &self.name)
            .field("model", &self.model)
            .finish()
    }
}

impl GeminiCli {
    pub fn new(name: impl Into<String>, model: impl Into<String>, config: EngineConfig) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            config: Arc::new(config),
            paused: Arc::default(),
        }
    }

    fn paused(&self) -> std::sync::MutexGuard<'_, Vec<Turn>> {
        self.paused.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reap(&self) {
        let timeout = self.config.turn_timeout;
        self.paused()
            .retain(|t| t.paused_at.is_none_or(|at| at.elapsed() < timeout));
    }

    async fn start(
        &self,
        req: &CompletionRequest,
        ctx: &CallContext,
    ) -> Result<CompletionResponse, CoreError> {
        let resume = resume_id(&req.messages);
        if let Some(id) = &resume {
            self.paused()
                .retain(|t| t.session_id.as_deref() != Some(id.as_str()));
        }
        let prompt = prompt(&req.messages, resume.is_some());
        let turn = self.spawn(req, ctx, resume.clone(), prompt).await?;
        match self.drive(turn, req, ctx).await? {
            Outcome::ResumeMissing if resume.is_some() => {
                let turn = self
                    .spawn(req, ctx, None, prompt_fresh(&req.messages))
                    .await?;
                match self.drive(turn, req, ctx).await? {
                    Outcome::ResumeMissing => Err(CoreError::Provider(
                        "Gemini CLI couldn't start a session".into(),
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
                "Gemini CLI has no conversation to resume".into(),
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
            CoreError::Provider(format!("Gemini CLI: {what}: {e}"))
        };
        std::fs::create_dir_all(&cfg.config_dir).map_err(|e| fail("the config dir", &e))?;
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

        let binary = super::cli::resolve(&cfg.binary);
        let is_agy = env::is_agy(&binary);
        let tools = with_deferred(&req.tools, &ctx.deferred_tools);
        let has_tool = |n: &str| tools.iter().any(|t| t.name == n);
        let mode = if !has_tool("write_file") && !has_tool("edit_file") && !has_tool("shell") {
            Some("plan".to_string())
        } else if !has_tool("shell") {
            Some("accept-edits".to_string())
        } else {
            None
        };

        let (calls_tx, calls) = mpsc::channel(16);
        let bridge = match (&cfg.bridge_command, is_agy) {
            (Some((program, args)), false) => {
                let bridge = Bridge::start(bridged(&tools), calls_tx)
                    .await
                    .map_err(|e| fail("the tool bridge", &e))?;
                let doc = bridge.mcp_config(program, args);
                let server = doc["mcpServers"][env::BRIDGE_SERVER].clone();
                env::write_bridge_settings(&cfg.config_dir, Some(server))
                    .map_err(|e| fail("the tool bridge settings", &format!("{e:#}")))?;
                Some(bridge)
            }
            _ => None,
        };

        let effective_prompt = if is_agy && resume.is_none() {
            let extra = bridged(&tools);
            if extra.is_empty() {
                prompt.clone()
            } else {
                let mut defs = String::from(
                    "When you need to call one of the following Ferrule tools (which are not built into the CLI), \
                     respond ONLY with a single JSON object of the form:\n\
                     {\"tool_calls\": [{\"name\": \"<tool_name>\", \"arguments\": {<args>}}]}\n\
                     Ferrule tools:\n",
                );
                for t in &extra {
                    defs.push_str(&format!(
                        "- {}: {} Parameters: {}\n",
                        t.name, t.description, t.parameters
                    ));
                }
                format!("{defs}\n{prompt}")
            }
        } else {
            prompt.clone()
        };

        let launch = Launch {
            is_agy,
            model: self.model.clone(),
            resume: resume.clone(),
            prompt: effective_prompt,
            mode,
            system_prompt_file,
            config_dir: cfg.config_dir.clone(),
            has_bridge: bridge.is_some(),
        };
        let parent = std::env::vars_os().filter_map(|(k, _)| k.into_string().ok());
        let spec = env::spec(&launch, parent);

        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut std_cmd = match &cfg.sandbox {
            Some(sandbox) => sandbox
                .for_helper(&cfg.config_dir, &[])
                .command(&binary, &spec.args, &workspace)
                .map_err(|e| fail("sandbox setup", &e))?,
            None => {
                let mut c = std::process::Command::new(&binary);
                c.args(&spec.args).current_dir(&workspace);
                c
            }
        };
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut std_cmd, 0);
        let mut cmd = tokio::process::Command::from(std_cmd);
        spec.apply(&mut cmd);
        cmd.stdin(if is_agy {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CoreError::Provider(format!(
                    "Gemini CLI isn't installed here ({} not found): {}",
                    binary.display(),
                    super::cli::INSTALL
                ))
            } else {
                fail("starting gemini", &e)
            }
        })?;
        let group = Group(child.id());

        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin.write_all(prompt.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }
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
            errors: Vec::new(),
            session_id: resume.clone(),
            resumed: resume.is_some(),
            used: Duration::ZERO,
            started: Instant::now(),
            paused_at: None,
            model: self.model.clone(),
        })
    }

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
            if !turn.queued.is_empty() {
                while let Ok(call) = turn.calls.try_recv() {
                    self.take_call(&mut turn, call, ctx).await;
                }
                let calls = std::mem::take(&mut turn.queued);
                let mut tool_calls = Vec::new();
                for call in calls {
                    let id = format!(
                        "gemini-cli-{}",
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
                        stop_reason: None,
                    },
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(CoreError::Provider(format!(
                        "Gemini CLI's turn ran past {} min and was stopped",
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
                                "Gemini CLI wrote more than {} MB in one turn; it was stopped",
                                self.config.max_output_bytes / (1024 * 1024)
                            )));
                        }
                        None => return self.ended_early(&mut turn, &segment).await,
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
                        Some(Event::Error(e)) => {
                            turn.errors.push(e);
                        }
                        Some(Event::Result(mut result)) => {
                            if result.text.is_empty() && !segment.is_empty() {
                                result.text = segment;
                            }
                            result.errors.extend(std::mem::take(&mut turn.errors));
                            return self.done(turn, result);
                        }
                        Some(Event::Other) | None => {}
                    }
                }
            }
        }
    }

    async fn take_call(&self, turn: &mut Turn, call: BridgeCall, ctx: &CallContext) {
        if call.name != APPROVE {
            turn.queued.push(call);
            return;
        }
        let answer = crate::claude::engine::approve(ctx.guard.as_ref(), &call.args).await;
        let _ = call.reply.send(Reply::ok(answer.to_string()));
    }

    fn done(&self, mut turn: Turn, result: TurnResult) -> Result<Outcome, CoreError> {
        if result.is_error && turn.resumed && stream::resume_missing(&result.text) {
            return Ok(Outcome::ResumeMissing);
        }
        if let Some(e) = stream::result_error(&result) {
            return Err(e);
        }
        if let Some(id) = result.session_id.clone() {
            turn.session_id = Some(id);
        }
        if let Some(m) = &result.model {
            turn.model = m.clone();
        }
        let mut message = match parse_tool_calls(&result.text) {
            Some(calls) if !calls.is_empty() => Message::assistant(None, calls, None),
            _ => Message::assistant(Some(result.text), vec![], None),
        };
        message.native = turn.native();
        Ok(Outcome::Done(CompletionResponse {
            message,
            usage: result.usage,
            stop_reason: None,
        }))
    }

    async fn ended_early(&self, turn: &mut Turn, segment: &str) -> Result<Outcome, CoreError> {
        let status = tokio::time::timeout(Duration::from_secs(5), turn.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        tokio::task::yield_now().await;
        let tail = turn
            .stderr_tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .to_string();
        if status.is_some_and(|s| s.success()) && !segment.trim().is_empty() {
            let mut message = Message::assistant(Some(segment.to_string()), vec![], None);
            message.native = turn.native();
            return Ok(Outcome::Done(CompletionResponse {
                message,
                usage: Usage::default(),
                stop_reason: None,
            }));
        }
        if turn.resumed && stream::resume_missing(&tail) {
            return Ok(Outcome::ResumeMissing);
        }
        let fake = TurnResult {
            is_error: true,
            text: tail.clone(),
            errors: turn.errors.clone(),
            ..TurnResult::default()
        };
        if let Some(e) = stream::result_error(&fake) {
            return Err(e);
        }
        let code = status
            .and_then(|s| s.code())
            .map_or("no exit code".to_string(), |c| format!("exit {c}"));
        Err(CoreError::Provider(format!(
            "Gemini CLI ended without an answer ({code}){}",
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        )))
    }
}

#[async_trait]
impl Provider for GeminiCli {
    fn name(&self) -> &str {
        &self.name
    }

    fn guest(&self) -> Option<Arc<dyn Provider>> {
        let mut config = (*self.config).clone();
        config.sandbox = config.sandbox.map(|s| s.narrowed(&config.guest_hidden));
        Some(Arc::new(GeminiCli {
            name: self.name.clone(),
            model: self.model.clone(),
            config: Arc::new(config),
            paused: self.paused.clone(),
        }))
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
                let _ = reply.send(Reply::ok(with_notes(m)));
            }
        }
        for (_, reply) in turn.pending.drain() {
            let _ = reply.send(Reply::error("ferrule didn't run this call"));
        }
        turn.resume();
        match self.drive(turn, &req, &ctx).await? {
            Outcome::ResumeMissing => Err(CoreError::Provider(
                "Gemini CLI lost its session mid-turn".into(),
            )),
            other => self.finish(other),
        }
    }
}

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

enum Outcome {
    Done(CompletionResponse),
    Paused(Box<Turn>, CompletionResponse),
    ResumeMissing,
}

enum Line {
    Text(String),
    TooMuch,
}

struct Turn {
    child: tokio::process::Child,
    _group: Group,
    lines: mpsc::Receiver<Line>,
    calls: mpsc::Receiver<BridgeCall>,
    _bridge: Option<Bridge>,
    _dir: TurnDir,
    stderr_tail: Arc<Mutex<String>>,
    pending: HashMap<String, tokio::sync::oneshot::Sender<Reply>>,
    queued: Vec<BridgeCall>,
    errors: Vec<String>,
    session_id: Option<String>,
    resumed: bool,
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

struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            ferrule_sandbox::kill_process_group(pid);
        }
    }
}

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

fn parse_tool_calls(text: &str) -> Option<Vec<ToolCall>> {
    let trimmed = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let arr = v.get("tool_calls")?.as_array()?;
    let mut out = Vec::new();
    for item in arr {
        let name = item.get("name")?.as_str()?.to_string();
        let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let id = format!(
            "gemini-cli-{}",
            hex(&ferrule_connections::seal::random::<6>())
        );
        out.push(ToolCall {
            id,
            name,
            arguments,
        });
    }
    Some(out)
}
