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
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
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
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<_> = self.tools.values().map(|t| t.definition()).collect();
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
            .tools
            .get(name)
            .ok_or_else(|| CoreError::ToolNotFound(name.to_string()))?;
        tool.call(args, ctx).await
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    pub fn changes_files(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.changes_files())
    }
}
