//! `remember` / `recall`: long-term memory as tools, run by ferrule itself.
//!
//! The sandbox keeps shell commands out of ferrule's data dir (it also holds
//! the task store, whose gate scripts run unsandboxed), so the agent can no
//! longer shell out to `ferrule memory add`. These give it the one write it
//! needs there, through an API that can only add a memory.

use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_memory::MemoryStore;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn tools(db: PathBuf) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RememberTool { db: db.clone() }),
        Arc::new(RecallTool { db }),
    ]
}

/// Opens the store per call, off the async runtime: SQLite calls block, and
/// a fresh connection per call means no lock is held between turns.
async fn with_store<T: Send + 'static>(
    tool: &str,
    db: &Path,
    f: impl FnOnce(&MemoryStore) -> Result<T, ferrule_memory::MemoryError> + Send + 'static,
) -> Result<T, CoreError> {
    let db = db.to_path_buf();
    let failed = |message: String| CoreError::ToolFailed {
        tool: tool.into(),
        message,
    };
    tokio::task::spawn_blocking(move || MemoryStore::open(&db).and_then(|store| f(&store)))
        .await
        .map_err(|e| failed(e.to_string()))?
        .map_err(|e| failed(e.to_string()))
}

pub struct RememberTool {
    db: PathBuf,
}

#[async_trait::async_trait]
impl Tool for RememberTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "remember".into(),
            description: "Save a fact to long-term memory, kept across sessions. \
                          One self-contained fact per call; recent memories are \
                          added to future system prompts."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The fact, written to make sense on its own later" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags" }
                },
                "required": ["content"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let content = args["content"].as_str().unwrap_or("").trim().to_string();
        if content.is_empty() {
            return Err(CoreError::ToolFailed {
                tool: "remember".into(),
                message: "empty content".into(),
            });
        }
        let tags: Vec<String> = args["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let id = with_store("remember", &self.db, move |store| {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            store.remember(&content, &tags)
        })
        .await?;
        Ok(ToolOutput::ok(format!("remembered (#{id})")))
    }
}

pub struct RecallTool {
    db: PathBuf,
}

#[async_trait::async_trait]
impl Tool for RecallTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "recall".into(),
            description: "Search long-term memory (keyword search, recent facts rank higher)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords to search for" },
                    "limit": { "type": "integer", "description": "Max results (default 10)" }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let query = args["query"].as_str().unwrap_or("").to_string();
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let found =
            with_store("recall", &self.db, move |store| store.recall(&query, limit)).await?;
        if found.is_empty() {
            return Ok(ToolOutput::ok("no matching memories"));
        }
        let lines: Vec<String> = found
            .iter()
            .map(|m| format!("#{} {}", m.id, m.content))
            .collect();
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_then_recall() {
        let dir = tempfile::tempdir().unwrap();
        let tools = tools(dir.path().join("memory.db"));
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        let out = tools[0]
            .call(
                json!({"content": "the deploy target is fly.io", "tags": ["infra"]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.starts_with("remembered"));
        let out = tools[1]
            .call(json!({"query": "deploy"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("fly.io"), "{}", out.content);
        assert!(tools[0].call(json!({"content": "  "}), &ctx).await.is_err());
    }
}
