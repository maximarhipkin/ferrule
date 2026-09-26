use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// JSON-Schema tool description as sent to providers. The harness profile
/// renders this into each provider's dialect at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    /// True when the output was cut to protect the context window.
    pub truncated: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
        }
    }
    /// Enforce a hard size cap — verbose tool output is the #1 source of
    /// context bloat. Long output goes to a spill file later; for now we cut.
    pub fn capped(content: String, max_chars: usize) -> Self {
        if content.len() <= max_chars {
            return Self {
                content,
                truncated: false,
            };
        }
        let mut cut = content.chars().take(max_chars).collect::<String>();
        cut.push_str(&format!("\n…[truncated, {} chars total]", content.len()));
        Self {
            content: cut,
            truncated: true,
        }
    }
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
    /// Add a dynamic layer. A registered tool shadows a source's tool of the
    /// same name, so nothing dynamic can replace `shell` or `write_file`.
    pub fn attach(&mut self, source: Arc<dyn ToolSource>) {
        self.sources.push(source);
    }
    /// The tool called `name`, registered or from a source.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.lookup(name)
    }
    fn lookup(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.tools.get(name) {
            return Some(t.clone());
        }
        self.sources
            .iter()
            .flat_map(|s| s.tools())
            .find(|t| t.definition().name == name)
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<_> = self.tools.values().map(|t| t.definition()).collect();
        for source in &self.sources {
            for tool in source.tools() {
                let def = tool.definition();
                if !defs.iter().any(|d| d.name == def.name) {
                    defs.push(def);
                }
            }
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
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
}
