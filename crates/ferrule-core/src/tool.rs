use crate::error::CoreError;
use crate::message::ImageRef;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// JSON-Schema tool description as sent to providers. The harness profile
/// renders this into each provider's dialect at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A non-text part of a tool result that was saved to a file rather than
/// inlined: an MCP image, audio clip or blob resource. The text in
/// [`ToolOutput::content`] names the file in prose; this is the same fact
/// as data, for the UI and the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputFile {
    /// Where the bytes landed — workspace-relative when under the workspace.
    pub path: PathBuf,
    /// The MIME type the tool reported for the part.
    pub mime_type: String,
    /// Bytes on disk, after decoding.
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    /// True when the output was cut to protect the context window.
    pub truncated: bool,
    /// Non-text parts saved to files (an MCP image under
    /// `.ferrule/mcp-content/`). Empty for a plain text result, and left
    /// out of the serialized form then, so a text-only result keeps its
    /// exact old shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ToolOutputFile>,
    /// M60: images for the model to see, as files on disk (`read_file` on
    /// a PNG, an MCP screenshot, a generated image). They ride on the
    /// tool's result message: pixels to a model that sees images, the
    /// newest few only, a note otherwise ([`crate::vision`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            files: Vec::new(),
            images: Vec::new(),
        }
    }
    /// Enforce a hard size cap — verbose tool output is the #1 source of
    /// context bloat. Keeps the head; output whose end matters (a shell
    /// command's) goes through [`spilled`](Self::spilled) instead.
    pub fn capped(content: String, max_chars: usize) -> Self {
        // M54.3: before the cut, so it never leaves part of a key.
        let content = crate::scrub::text(content);
        let total = content.chars().count();
        if total <= max_chars {
            return Self::ok(content);
        }
        let mut cut = content.chars().take(max_chars).collect::<String>();
        cut.push_str(&format!("\n…[truncated, {total} chars total]"));
        Self {
            content: cut,
            truncated: true,
            files: Vec::new(),
            images: Vec::new(),
        }
    }
    /// Like [`capped`](Self::capped), but nothing is lost. Output over
    /// `max_chars` is saved whole to `<workspace>/.ferrule/spill/<id>.log`,
    /// and the model gets `lead` first (the shell's exit-code line), then
    /// the first ~2k and the last ~8k characters, then the file's path.
    /// Output that fits comes back whole, without `lead` (the caller has it
    /// where it wants it already). Saving is best effort: when it fails the
    /// head and tail still come back, with the reason.
    pub fn spilled(
        content: String,
        lead: Option<&str>,
        max_chars: usize,
        workspace: &Path,
    ) -> Self {
        // M54.3: the spill file and the cut both get the scrubbed text.
        let content = crate::scrub::text(content);
        if content.chars().count() <= max_chars {
            return Self::ok(content);
        }
        let saved = save_spill(workspace, &content);
        Self::spill_preview(
            &content,
            lead,
            max_chars,
            saved.as_deref().map_err(String::as_str),
        )
    }
    /// What [`spilled`](Self::spilled) returns for `content` over
    /// `max_chars`, for a tool that saves the whole somewhere else (a
    /// remote workspace): `saved` is the path shown, or why it wasn't
    /// saved.
    pub fn spill_preview(
        content: &str,
        lead: Option<&str>,
        max_chars: usize,
        saved: Result<&str, &str>,
    ) -> Self {
        let content = &crate::scrub::text(content.to_string());
        let total = content.chars().count();
        let budget = max_chars.min(SPILL_HEAD + SPILL_TAIL).min(total);
        let head = budget * SPILL_HEAD / (SPILL_HEAD + SPILL_TAIL);
        let tail = budget - head;
        let at = |n: usize| {
            content
                .char_indices()
                .nth(n)
                .map_or(content.len(), |(i, _)| i)
        };
        let mut out = String::new();
        if let Some(lead) = lead {
            out.push_str(lead);
            out.push('\n');
        }
        out.push_str(&content[..at(head)]);
        out.push_str(&format!(
            "\n…[{} characters cut here]…\n",
            total - head - tail
        ));
        out.push_str(&content[at(total - tail)..]);
        out.push('\n');
        out.push_str(&match saved {
            Ok(path) => format!(
                "[{total} characters in all; the whole output is in {path}: read or grep that file for the part cut]"
            ),
            Err(why) => format!("[{total} characters in all; the whole output wasn't saved: {why}]"),
        });
        Self {
            content: out,
            truncated: true,
            files: Vec::new(),
            images: Vec::new(),
        }
    }
    /// Attach saved-file references (see [`ToolOutputFile`]).
    pub fn with_files(mut self, files: Vec<ToolOutputFile>) -> Self {
        self.files = files;
        self
    }
    /// M60: attach images for the model (see [`ToolOutput::images`]).
    pub fn with_images(mut self, images: Vec<ImageRef>) -> Self {
        self.images = images;
        self
    }
}

/// Where [`ToolOutput::spilled`] saves output too long for the context:
/// under the workspace, so `read_file` and `shell` can open it. Files stay
/// until the owner cleans the dir; nothing prunes them yet.
pub const SPILL_DIR: &str = ".ferrule/spill";
/// How much of a spilled output stays inline: about 2k characters from the
/// start and 8k from the end, where a build's or a test run's failures
/// are. A smaller cap shrinks both in proportion.
const SPILL_HEAD: usize = 2_000;
const SPILL_TAIL: usize = 8_000;

/// A new spill file's name: the time, so the dir sorts, and a random part.
pub fn spill_file_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "{}-{}.log",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        &id[..8]
    )
}

/// Write `content` to a new file under `<workspace>/.ferrule/spill/`, and
/// return its workspace-relative path.
fn save_spill(workspace: &Path, content: &str) -> Result<String, String> {
    let (mut file, shown) = new_spill_file(workspace)?;
    std::io::Write::write_all(&mut file, content.as_bytes())
        .map_err(|e| format!("could not write {shown}: {e}"))?;
    Ok(shown)
}

/// A new, empty file under `<workspace>/.ferrule/spill/`, open for
/// writing, and its workspace-relative path: where a spilled output goes,
/// and a background job's (M57.6) as it runs.
pub fn new_spill_file(workspace: &Path) -> Result<(std::fs::File, String), String> {
    let dir = workspace.join(SPILL_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {SPILL_DIR}: {e}"))?;
    // This runs in ferrule's own process, not the sandbox, and a sandboxed
    // command can make links in the workspace: a spill dir that resolves
    // outside it is refused, and the file is new (never a link followed).
    let inside = std::fs::canonicalize(&dir)
        .ok()
        .zip(std::fs::canonicalize(workspace).ok())
        .is_some_and(|(d, w)| d.starts_with(w));
    if !inside {
        return Err(format!("{SPILL_DIR} leads outside the workspace"));
    }
    // Logs, not work: git (and `auto_commit`) leaves them out.
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"*\n"));
    let name = spill_file_name();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(&name))
        .map_err(|e| format!("could not write {SPILL_DIR}/{name}: {e}"))?;
    Ok((file, format!("{SPILL_DIR}/{name}")))
}

/// Shared, immutable context handed to every tool call.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// The "room" the agent is allowed to work in. File tools must not
    /// resolve paths outside it.
    pub workspace: PathBuf,
    /// Max chars a tool may return into context.
    pub max_output_chars: usize,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            max_output_chars: 30_000,
        }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, CoreError>;
    /// Whether a successful call may have changed what the verify command
    /// checks. Tools that only read, and the agent's own notes under
    /// `.ferrule/`, say no; anything unknown is assumed to.
    fn changes_files(&self) -> bool {
        true
    }
    /// M27: whether the call only reads, so it may run at the same time as
    /// other read-only calls from the same response. Stricter than
    /// `!changes_files()`: a todo list or a memory note changes nothing the
    /// verify command checks, but it still writes. Unknown means no.
    fn read_only(&self) -> bool {
        false
    }
    /// M27: calls in the same group never overlap, even when read-only. An
    /// MCP stdio server is one pipe to one process that may not take a
    /// second request before the first is answered.
    fn serial_group(&self) -> Option<String> {
        None
    }
    /// M32: every call needs the owner's approval (the M19 gate), whatever
    /// its arguments, e.g. a plugin tool its manifest marks `approval`.
    fn needs_approval(&self) -> bool {
        false
    }
    /// M57.1: the tool may be offered by name and one line only, its schema
    /// loaded by [`TOOL_SEARCH`] when the model wants it: an MCP tool, or a
    /// built-in most sessions never call. See [`DeferTools`].
    fn deferrable(&self) -> bool {
        false
    }
    /// M57.6: the session this tool's agent serves ended (`ferrule run`
    /// finished, `ferrule chat` exited or began anew): let go of what the
    /// tool still holds, such as background jobs. A tool whose agent is
    /// dropped without this (a gateway lane closing) does it on drop.
    fn session_ended(&self) {}
    /// M58.3: an old name kept callable for one release, so a transcript or
    /// a session that still uses it works: the tool that replaced it. Such a
    /// tool is never offered (not in [`ToolRegistry::definitions`]).
    fn replaced_by(&self) -> Option<&'static str> {
        None
    }
}

/// M57.1: the tool that loads deferred tools' schemas. The registry
/// offers it, and only while it defers something.
pub const TOOL_SEARCH: &str = "tool_search";

/// M57.1: `auto` defers once more than this many tools are deferrable.
pub const DEFER_AUTO_OVER: usize = 10;

/// M57.1 (`[agent] defer_tools`): when the registry sends deferrable tools
/// ([`Tool::deferrable`]) as a name and one line in [`TOOL_SEARCH`]'s
/// description instead of their whole schemas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferTools {
    /// Once more than [`DEFER_AUTO_OVER`] tools are deferrable; from then
    /// on for the rest of the session.
    #[default]
    Auto,
    /// Whenever any tool is deferrable.
    Always,
    /// Never: every schema goes with every request, as before M57.
    Off,
}

/// What a registry that defers keeps for its session: whether `auto` has
/// turned on, and the tools loaded so far, in the order they were.
#[derive(Debug)]
struct Deferral {
    mode: DeferTools,
    on: std::sync::atomic::AtomicBool,
    loaded: std::sync::Mutex<Vec<String>>,
}

impl Deferral {
    fn loaded(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn load(&self, name: &str) {
        let mut loaded = self.loaded();
        if !loaded.iter().any(|n| n == name) {
            loaded.push(name.to_string());
        }
    }
}

/// A deferred tool's line in [`TOOL_SEARCH`]'s description: the first line
/// of its own, cut to about 120 characters.
fn one_line(description: &str) -> String {
    let first = description.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 120 {
        return first.to_string();
    }
    let mut cut: String = first.chars().take(119).collect();
    cut.push('…');
    cut
}

/// [`TOOL_SEARCH`]'s definition over `deferred` (sorted by name): the same
/// bytes for as long as the set is the same, loaded tools included, so the
/// list of tools before the loaded ones never changes.
fn search_definition(deferred: &[ToolDefinition]) -> ToolDefinition {
    let mut catalog = String::new();
    for d in deferred {
        catalog.push_str(&format!("\n- {}: {}", d.name, one_line(&d.description)));
    }
    ToolDefinition {
        name: TOOL_SEARCH.into(),
        description: format!(
            "Load tools that are offered here by name only, to keep every request small. Give words for \
             what you need (\"create github issue\") or `select:<name>,<name>` for exact names: the \
             matching tools' full schemas come back, and you can call them from then on.\n\
             Tools to load:{catalog}"
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words describing the tool you need, or select:<name>,<name>"
                },
                "max_results": {
                    "type": "integer",
                    "description": "At most this many tools (default 5)"
                }
            },
            "required": ["query"]
        }),
    }
}

/// The deferred tools `query` names: `select:a,b` exactly, a whole name, or
/// those whose name (3 points a word) or description (1) holds its words,
/// best first.
fn search_deferred<'a>(
    tools: &'a [ToolDefinition],
    query: &str,
    max: usize,
) -> Vec<&'a ToolDefinition> {
    if let Some(names) = query.strip_prefix("select:") {
        return names
            .split(',')
            .map(str::trim)
            .filter_map(|n| tools.iter().find(|t| t.name == n))
            .collect();
    }
    let q = query.to_lowercase();
    if let Some(t) = tools.iter().find(|t| t.name.to_lowercase() == q) {
        return vec![t];
    }
    let words: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut scored: Vec<(usize, &ToolDefinition)> = tools
        .iter()
        .filter_map(|t| {
            let (name, desc) = (t.name.to_lowercase(), t.description.to_lowercase());
            let score: usize = words
                .iter()
                .map(|w| match (name.contains(w), desc.contains(w)) {
                    (true, _) => 3,
                    (false, true) => 1,
                    _ => 0,
                })
                .sum();
            (score > 0).then_some((score, t))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().take(max).map(|(_, t)| t).collect()
}

/// [`TOOL_SEARCH`] itself, made by the registry over the tools it defers
/// right now.
struct ToolSearch {
    deferred: Vec<ToolDefinition>,
    state: Arc<Deferral>,
}

#[async_trait::async_trait]
impl Tool for ToolSearch {
    fn definition(&self) -> ToolDefinition {
        search_definition(&self.deferred)
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err(CoreError::ToolFailed {
                tool: TOOL_SEARCH.into(),
                message: "`query` is empty: give words for the tool you need, or select:<name>"
                    .into(),
            });
        }
        let max = args["max_results"]
            .as_u64()
            .map_or(5, |n| n.clamp(1, 20) as usize);
        let found = search_deferred(&self.deferred, query, max);
        if found.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "No tool to load matches {query:?}. The ones there are, are listed in {TOOL_SEARCH}'s \
                 description; tools you already have need no loading."
            )));
        }
        let mut out = format!(
            "Loaded {} tool{}: call {} by name from now on.",
            found.len(),
            if found.len() == 1 { "" } else { "s" },
            if found.len() == 1 { "it" } else { "them" }
        );
        for def in found {
            self.state.load(&def.name);
            let schema = serde_json::json!({
                "name": def.name,
                "description": def.description,
                "parameters": def.parameters,
            });
            out.push('\n');
            out.push_str(&schema.to_string());
        }
        Ok(ToolOutput::ok(out))
    }

    fn changes_files(&self) -> bool {
        false
    }
}

/// Tools that can change while an agent runs: MCP servers installed or
/// suspended mid-session, a skill set that grows. The registry asks every
/// attached source on each `definitions()` and `call()`, so a tool that
/// appears between two provider calls is in the next request.
pub trait ToolSource: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    sources: Vec<Arc<dyn ToolSource>>,
    /// M57.1: set by [`ToolRegistry::defer_tools`]; `None` never defers.
    defer: Option<Arc<Deferral>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.definition().name, tool);
    }
    pub fn remove(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }
    /// M57.6: tell every registered tool its session ended
    /// ([`Tool::session_ended`]).
    pub fn session_ended(&self) {
        for tool in self.tools.values() {
            tool.session_ended();
        }
    }
    /// Add a dynamic layer. A registered tool shadows a source's tool of the
    /// same name, so nothing dynamic can replace `shell` or `write_file`.
    pub fn attach(&mut self, source: Arc<dyn ToolSource>) {
        self.sources.push(source);
    }
    /// M57.1: offer deferrable tools by name until [`TOOL_SEARCH`] loads
    /// them, per `mode`. Loads are kept for as long as this registry (and
    /// its clones) live: the session.
    pub fn defer_tools(&mut self, mode: DeferTools) {
        self.defer = (mode != DeferTools::Off).then(|| {
            Arc::new(Deferral {
                mode,
                on: Default::default(),
                loaded: Default::default(),
            })
        });
    }
    /// The tool called `name`, registered or from a source.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.lookup(name)
    }
    fn lookup(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if name == TOOL_SEARCH {
            if let Some(search) = self.search_tool() {
                return Some(search);
            }
        }
        if let Some(t) = self.tools.get(name) {
            return Some(t.clone());
        }
        self.sources
            .iter()
            .flat_map(|s| s.tools())
            .find(|t| t.definition().name == name)
    }
    /// Every tool offered, with its definition, sorted by name; a
    /// registered tool shadows a source's of the same name. An old name
    /// ([`Tool::replaced_by`]) is callable but not here.
    fn all(&self) -> Vec<(ToolDefinition, Arc<dyn Tool>)> {
        let mut all: Vec<_> = self
            .tools
            .values()
            .filter(|t| t.replaced_by().is_none())
            .map(|t| (t.definition(), t.clone()))
            .collect();
        for source in &self.sources {
            for tool in source.tools() {
                let def = tool.definition();
                let shadowed = self.tools.contains_key(&def.name)
                    || all.iter().any(|(d, _)| d.name == def.name);
                if !shadowed && tool.replaced_by().is_none() {
                    all.push((def, tool));
                }
            }
        }
        all.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        all
    }
    /// The deferral, when it defers something now: `auto` turns on past
    /// [`DEFER_AUTO_OVER`] deferrable tools and stays on.
    fn deferring(&self, all: &[(ToolDefinition, Arc<dyn Tool>)]) -> Option<&Arc<Deferral>> {
        let d = self.defer.as_ref()?;
        let count = all.iter().filter(|(_, t)| t.deferrable()).count();
        if count == 0 {
            return None;
        }
        if d.mode == DeferTools::Auto && count > DEFER_AUTO_OVER {
            d.on.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        match d.mode {
            DeferTools::Always => Some(d),
            DeferTools::Auto => d.on.load(std::sync::atomic::Ordering::Relaxed).then_some(d),
            DeferTools::Off => None,
        }
    }
    fn search_tool(&self) -> Option<Arc<dyn Tool>> {
        let all = self.all();
        let state = self.deferring(&all)?.clone();
        let deferred = all
            .into_iter()
            .filter(|(_, t)| t.deferrable())
            .map(|(def, _)| def)
            .collect();
        Some(Arc::new(ToolSearch { deferred, state }))
    }
    /// What each request offers. Without deferral, every tool sorted by
    /// name, as always. Deferring (M57.1), the tools that aren't deferred
    /// and [`TOOL_SEARCH`] sorted by name, then each loaded tool in the
    /// order it was loaded: a load only appends, so the bytes before it
    /// stay the same.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let all = self.all();
        let Some(state) = self.deferring(&all) else {
            return all.into_iter().map(|(def, _)| def).collect();
        };
        let loaded = state.loaded().clone();
        let (deferred, stable): (Vec<_>, Vec<_>) =
            all.into_iter().partition(|(_, t)| t.deferrable());
        let deferred: Vec<ToolDefinition> = deferred.into_iter().map(|(def, _)| def).collect();
        let mut defs: Vec<ToolDefinition> = stable.into_iter().map(|(def, _)| def).collect();
        defs.push(search_definition(&deferred));
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs.extend(
            loaded
                .iter()
                .filter_map(|name| deferred.iter().find(|d| &d.name == name).cloned()),
        );
        defs
    }
    /// M57.1: the deferred tools not loaded yet, whole, for a provider that
    /// offers tools on its own and has its own search (the Claude Code
    /// engine). Empty when nothing is deferred.
    pub fn deferred_definitions(&self) -> Vec<ToolDefinition> {
        let all = self.all();
        let Some(state) = self.deferring(&all) else {
            return Vec::new();
        };
        let loaded = state.loaded().clone();
        all.into_iter()
            .filter(|(def, t)| t.deferrable() && !loaded.contains(&def.name))
            .map(|(def, _)| def)
            .collect()
    }
    /// M57.1: the model called these tools (or a resumed history did): a
    /// deferred one among them is loaded, as [`TOOL_SEARCH`] would have,
    /// so its schema goes with every later request.
    pub fn note_called<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        if self.defer.is_none() {
            return;
        }
        let all = self.all();
        let Some(state) = self.deferring(&all) else {
            return;
        };
        for name in names {
            if all.iter().any(|(d, t)| d.name == name && t.deferrable()) {
                state.load(name);
            }
        }
    }
    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        let tool = self
            .lookup(name)
            .ok_or_else(|| CoreError::ToolNotFound(name.to_string()))?;
        tool.call(args, ctx).await
    }
    pub fn contains(&self, name: &str) -> bool {
        self.lookup(name).is_some()
    }
    pub fn changes_files(&self, name: &str) -> bool {
        self.lookup(name).is_some_and(|t| t.changes_files())
    }
    /// Whether `name` is a known tool that only reads (M27).
    pub fn read_only(&self, name: &str) -> bool {
        self.lookup(name).is_some_and(|t| t.read_only())
    }
    /// Whether `name` is a known tool that asks for approval (M32).
    pub fn needs_approval(&self, name: &str) -> bool {
        self.lookup(name).is_some_and(|t| t.needs_approval())
    }
    /// Whether a call can't be right as written: an unknown tool, arguments
    /// that aren't an object, or a field its schema requires left out. M25
    /// counts these toward escalation; the call itself still runs.
    pub fn misfit(&self, name: &str, args: &serde_json::Value) -> bool {
        let Some(tool) = self.lookup(name) else {
            return true;
        };
        let Some(given) = args.as_object() else {
            return true;
        };
        let def = tool.definition();
        let required = def.parameters.get("required").and_then(|r| r.as_array());
        required
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str())
            .any(|field| !given.contains_key(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Named(&'static str, &'static str);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: self.1.into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, CoreError> {
            Ok(ToolOutput::ok(self.1))
        }
    }

    #[derive(Default)]
    struct Live(Mutex<Vec<Arc<dyn Tool>>>);

    impl ToolSource for Live {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn a_source_tool_appears_and_disappears_without_touching_the_registry() {
        let live = Arc::new(Live::default());
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Named("shell", "static")));
        reg.attach(live.clone());
        assert_eq!(reg.definitions().len(), 1);
        assert!(!reg.contains("mcp__x__y"));

        live.0
            .lock()
            .unwrap()
            .push(Arc::new(Named("mcp__x__y", "dyn")));
        let names: Vec<_> = reg.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["mcp__x__y", "shell"]);
        let out = reg
            .call("mcp__x__y", serde_json::json!({}), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out.content, "dyn");

        live.0.lock().unwrap().clear();
        assert!(!reg.contains("mcp__x__y"));
        assert!(matches!(
            reg.call("mcp__x__y", serde_json::json!({}), &ToolContext::default())
                .await,
            Err(CoreError::ToolNotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_registered_tool_shadows_a_dynamic_one_of_the_same_name() {
        let live = Arc::new(Live::default());
        live.0
            .lock()
            .unwrap()
            .push(Arc::new(Named("shell", "evil")));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Named("shell", "static")));
        reg.attach(live);
        let defs = reg.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "static");
        let out = reg
            .call("shell", serde_json::json!({}), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out.content, "static");
    }

    #[test]
    fn a_text_only_output_serializes_exactly_as_before_files() {
        let out = ToolOutput::ok("hi");
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            r#"{"content":"hi","truncated":false}"#
        );
        assert!(out.files.is_empty());
    }

    #[test]
    fn stored_output_without_files_still_loads() {
        let out: ToolOutput = serde_json::from_str(r#"{"content":"hi","truncated":true}"#).unwrap();
        assert_eq!(out.content, "hi");
        assert!(out.truncated);
        assert!(out.files.is_empty());
    }

    #[test]
    fn saved_files_roundtrip() {
        let out = ToolOutput::ok("[image/png saved to .ferrule/mcp-content/image-1.png (4 bytes)]")
            .with_files(vec![ToolOutputFile {
                path: PathBuf::from(".ferrule/mcp-content/image-1.png"),
                mime_type: "image/png".into(),
                size_bytes: 4,
            }]);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["files"][0]["mime_type"], "image/png");
        let back: ToolOutput = serde_json::from_value(json).unwrap();
        assert_eq!(back.files, out.files);
    }
}
