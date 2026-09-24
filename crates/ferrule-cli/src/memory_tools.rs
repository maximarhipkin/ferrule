//! `remember` / `recall` / `update_memory` / `forget`: long-term memory as
//! tools, run by ferrule itself.
//!
//! The sandbox keeps shell commands out of ferrule's data dir (it also holds
//! the task store, whose gate scripts run unsandboxed), so the agent can't
//! shell out to `ferrule memory add`. These give it the writes it needs
//! there, each narrowed by [`MemoryAccess`] (`docs/m15-memory.md` §8).

use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::SessionRecall;
use ferrule_memory::{Decision, Inserted, MemoryStore};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What an agent may do to the shared long-term memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAccess {
    /// The root agent: add, correct (`update_memory`, `remember` with
    /// `replaces`) and delete (`forget`).
    Full,
    /// A writing sub-agent: add only. It sees a slice of the work, so it
    /// never decides that a shared fact is wrong.
    Append,
    /// A read-only sub-agent: `recall` only.
    Read,
}

impl MemoryAccess {
    /// Root → `Full`; a read-only child → `Read`; any other child → `Append`.
    pub fn for_child(child: Option<&ferrule_agents::ChildSpec>) -> Self {
        match child {
            None => MemoryAccess::Full,
            Some(spec) if spec.read_only => MemoryAccess::Read,
            Some(_) => MemoryAccess::Append,
        }
    }
}

/// The root agent's tools (what `ferrule eval`'s engineered variant gets).
pub fn tools(db: PathBuf) -> Vec<Arc<dyn Tool>> {
    tools_for(db, MemoryAccess::Full)
}

pub fn tools_for(db: PathBuf, access: MemoryAccess) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if access != MemoryAccess::Read {
        tools.push(Arc::new(RememberTool {
            db: db.clone(),
            access,
        }));
    }
    tools.push(Arc::new(RecallTool { db: db.clone() }));
    if access == MemoryAccess::Full {
        tools.push(Arc::new(UpdateMemoryTool { db: db.clone() }));
        tools.push(Arc::new(ForgetTool { db }));
    }
    tools
}

/// The session-start memory block: facts matching the session's goal,
/// then the newest, under `[Long-term memory]` (§4). A store that can't be
/// opened just means no block.
pub struct GoalRecall {
    pub db: PathBuf,
}

/// Same budget as the static block it replaced.
const RECALL_BUDGET_CHARS: usize = 2_000;

#[async_trait::async_trait]
impl SessionRecall for GoalRecall {
    async fn recall(&self, goal: &str) -> Option<String> {
        let db = self.db.clone();
        let goal = goal.to_string();
        let block = tokio::task::spawn_blocking(move || {
            MemoryStore::open(&db).and_then(|s| s.assemble_for_goal(&goal, RECALL_BUDGET_CHARS))
        })
        .await
        .ok()?
        .ok()?;
        (!block.is_empty()).then(|| format!("[Long-term memory]\n{block}"))
    }
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

fn failed(tool: &str, message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

fn content_arg(tool: &str, args: &Value) -> Result<String, CoreError> {
    let content = args["content"].as_str().unwrap_or("").trim().to_string();
    if content.is_empty() {
        return Err(failed(tool, "empty content"));
    }
    Ok(content)
}

fn tags_arg(args: &Value) -> Vec<String> {
    args["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn id_arg(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str()?.trim().trim_start_matches('#').parse().ok())
}

/// The tool result for a write: what was decided, plus any similar live
/// facts, so the model can tell a correction from a second fact (§1).
fn report(ins: &Inserted, can_correct: bool) -> String {
    let mut out = match ins.decision {
        Decision::Noop => return format!("already remembered (#{})", ins.id),
        Decision::Added => format!("remembered (#{})", ins.id),
        Decision::Updated => {
            let old: Vec<String> = ins.replaced.iter().map(|id| format!("#{id}")).collect();
            format!("remembered (#{}), replacing {}", ins.id, old.join(", "))
        }
    };
    if !ins.similar.is_empty() {
        out.push_str(". Similar live memories:");
        for m in &ins.similar {
            out.push_str(&format!("\n#{} {}", m.id, m.content));
        }
        if can_correct {
            let first = ins.similar[0].id;
            out.push_str(&format!(
                "\nIf #{} corrects one of them, call update_memory {{\"id\": {first}, \"content\": …}} \
                 for that one; if one is simply wrong now, forget it.",
                ins.id
            ));
        }
    }
    out
}

pub struct RememberTool {
    db: PathBuf,
    access: MemoryAccess,
}

#[async_trait::async_trait]
impl Tool for RememberTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        let mut properties = json!({
            "content": { "type": "string", "description": "The fact, written to make sense on its own later" },
            "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags" }
        });
        let mut description = "Save a fact to long-term memory, kept across sessions. \
                               One self-contained fact per call; facts matching a \
                               session's task are added to its system prompt. \
                               A fact already known is not stored twice."
            .to_string();
        if self.access == MemoryAccess::Full {
            properties["replaces"] = json!({
                "type": "array",
                "items": { "type": "integer" },
                "description": "Ids of live memories this fact corrects; they stop being recalled"
            });
            description.push_str(" To correct a fact, pass its id in `replaces`.");
        }
        ToolDefinition {
            name: "remember".into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": ["content"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let content = content_arg("remember", &args)?;
        let tags = tags_arg(&args);
        let replaces: Vec<i64> = args["replaces"]
            .as_array()
            .map(|a| a.iter().filter_map(id_arg).collect())
            .unwrap_or_default();
        // Enforced here, not only by leaving it out of the schema: a model
        // can send any argument.
        if !replaces.is_empty() && self.access != MemoryAccess::Full {
            return Err(failed(
                "remember",
                "a sub-agent can add memories but not replace them; remember the fact \
                 without `replaces` and report the conflict in your result",
            ));
        }
        let can_correct = self.access == MemoryAccess::Full;
        let ins = with_store("remember", &self.db, move |store| {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            store.insert(&content, &tags, &replaces)
        })
        .await?;
        Ok(ToolOutput::ok(report(&ins, can_correct)))
    }
}

pub struct UpdateMemoryTool {
    db: PathBuf,
}

#[async_trait::async_trait]
impl Tool for UpdateMemoryTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "update_memory".into(),
            description: "Correct a long-term memory: the new text replaces memory #id, \
                          which is kept as history but no longer recalled. Use this when \
                          a remembered fact has changed or was wrong."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The id of the live memory to correct (the #N shown with it)" },
                    "content": { "type": "string", "description": "The corrected fact, complete on its own" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional; the old fact's tags are kept when omitted" }
                },
                "required": ["id", "content"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let id = id_arg(&args["id"]).ok_or_else(|| failed("update_memory", "missing id"))?;
        let content = content_arg("update_memory", &args)?;
        let tags = tags_arg(&args);
        let ins = with_store("update_memory", &self.db, move |store| {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            store.supersede(id, &content, &tags)
        })
        .await?;
        Ok(ToolOutput::ok(report(&ins, true)))
    }
}

pub struct ForgetTool {
    db: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ForgetTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "forget".into(),
            description: "Delete a long-term memory for good, with its older versions. \
                          Irreversible. Use it when a fact is simply wrong or the user asks \
                          to forget it; when there is a replacement, use update_memory."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The id of the memory to delete" }
                },
                "required": ["id"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let id = id_arg(&args["id"]).ok_or_else(|| failed("forget", "missing id"))?;
        let deleted = with_store("forget", &self.db, move |store| store.forget(id)).await?;
        if deleted.is_empty() {
            return Err(failed("forget", format!("there is no memory #{id}")));
        }
        let ids: Vec<String> = deleted.iter().map(|d| format!("#{d}")).collect();
        Ok(ToolOutput::ok(format!("forgot {}", ids.join(", "))))
    }
}

pub struct RecallTool {
    db: PathBuf,
}

#[async_trait::async_trait]
impl Tool for RecallTool {
    fn changes_files(&self) -> bool {
        false
    }

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

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|t| t.definition().name).collect()
    }

    fn by_name(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .unwrap()
            .clone()
    }

    #[test]
    fn access_narrows_the_tool_set() {
        let db = PathBuf::from("unused.db");
        assert_eq!(
            names(&tools_for(db.clone(), MemoryAccess::Full)),
            ["remember", "recall", "update_memory", "forget"]
        );
        assert_eq!(
            names(&tools_for(db.clone(), MemoryAccess::Append)),
            ["remember", "recall"]
        );
        assert_eq!(
            names(&tools_for(db.clone(), MemoryAccess::Read)),
            ["recall"]
        );
        let schema = |access| {
            by_name(&tools_for(db.clone(), access), "remember")
                .definition()
                .parameters
        };
        assert!(schema(MemoryAccess::Full)["properties"]["replaces"].is_object());
        assert!(schema(MemoryAccess::Append)["properties"]["replaces"].is_null());
    }

    #[tokio::test]
    async fn noop_similar_update_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let full = tools(db.clone());
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        let remember = by_name(&full, "remember");
        let call = |tool: Arc<dyn Tool>, args: Value| {
            let ctx = ctx.clone();
            async move { tool.call(args, &ctx).await }
        };

        let out = call(
            remember.clone(),
            json!({"content": "The deploy target is fly.io"}),
        )
        .await
        .unwrap();
        assert_eq!(out.content, "remembered (#1)");
        let out = call(
            remember.clone(),
            json!({"content": "the deploy target is fly.io."}),
        )
        .await
        .unwrap();
        assert_eq!(out.content, "already remembered (#1)");

        // A contradicting fact is added, and the old one is shown back.
        let out = call(
            remember.clone(),
            json!({"content": "The deploy target is render"}),
        )
        .await
        .unwrap();
        assert!(
            out.content.starts_with("remembered (#2). Similar"),
            "{}",
            out.content
        );
        assert!(out.content.contains("#1 The deploy target is fly.io"));
        assert!(out.content.contains("update_memory {\"id\": 1"));

        // The model corrects #1 with #2's text: #2 becomes the replacement.
        let out = call(
            by_name(&full, "update_memory"),
            json!({"id": 1, "content": "The deploy target is render"}),
        )
        .await
        .unwrap();
        assert_eq!(out.content, "remembered (#2), replacing #1");
        let out = call(by_name(&full, "recall"), json!({"query": "deploy target"}))
            .await
            .unwrap();
        assert_eq!(out.content, "#2 The deploy target is render");
        let err = call(
            by_name(&full, "update_memory"),
            json!({"id": 1, "content": "x"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("update #2 instead"), "{err}");

        // A writing child can add but not replace, whatever it sends.
        let append = tools_for(db.clone(), MemoryAccess::Append);
        let err = call(
            by_name(&append, "remember"),
            json!({"content": "The deploy target is heroku", "replaces": [2]}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not replace"), "{err}");

        let out = call(by_name(&full, "forget"), json!({"id": "#2"}))
            .await
            .unwrap();
        assert_eq!(out.content, "forgot #1, #2");
        let out = call(by_name(&full, "recall"), json!({"query": "deploy"}))
            .await
            .unwrap();
        assert_eq!(out.content, "no matching memories");
        assert!(call(by_name(&full, "forget"), json!({"id": 2}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn goal_recall_heads_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let hook = GoalRecall { db: db.clone() };
        assert_eq!(hook.recall("anything").await, None);
        MemoryStore::open(&db)
            .unwrap()
            .remember("the staging database listens on port 5781", &[])
            .unwrap();
        let block = hook
            .recall("connect to the staging database")
            .await
            .unwrap();
        assert_eq!(
            block,
            "[Long-term memory]\n- #1 the staging database listens on port 5781"
        );
    }
}
