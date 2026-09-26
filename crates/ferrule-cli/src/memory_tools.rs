//! `remember` / `recall` / `update_memory` / `forget`: long-term memory as
//! tools, run by ferrule itself.
//!
//! The sandbox keeps shell commands out of ferrule's data dir (it also holds
//! the task store, whose gate scripts run unsandboxed), so the agent can't
//! shell out to `ferrule memory add`. These give it the writes it needs
//! there, each narrowed by [`MemoryAccess`] (`docs/m15-memory.md` §8).

use crate::embedding::{self, Embedding};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::SessionRecall;
use ferrule_embed::Purpose;
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
    tools_with(db, access, None)
}

/// With an embedder, `remember`/`update_memory` store a vector with each
/// fact and `recall` merges it with keyword search (M30). Without one they
/// are exactly the keyword tools.
pub fn tools_with(
    db: PathBuf,
    access: MemoryAccess,
    embedding: Option<Arc<Embedding>>,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if access != MemoryAccess::Read {
        tools.push(Arc::new(RememberTool {
            db: db.clone(),
            access,
            embedding: embedding.clone(),
        }));
    }
    tools.push(Arc::new(RecallTool {
        db: db.clone(),
        embedding: embedding.clone(),
    }));
    if access == MemoryAccess::Full {
        tools.push(Arc::new(UpdateMemoryTool {
            db: db.clone(),
            embedding,
        }));
        tools.push(Arc::new(ForgetTool { db }));
    }
    tools
}

/// The session-start memory block: facts matching the session's goal,
/// then the newest, under `[Long-term memory]` (§4). A store that can't be
/// opened just means no block.
pub struct GoalRecall {
    db: PathBuf,
    embedding: Option<Arc<Embedding>>,
}

impl GoalRecall {
    pub fn new(db: PathBuf) -> Self {
        Self {
            db,
            embedding: None,
        }
    }

    /// Match the goal by meaning too, and afterwards embed a batch of the
    /// facts that have no vector yet, in the background.
    pub fn with_embedding(mut self, embedding: Option<Arc<Embedding>>) -> Self {
        self.embedding = embedding;
        self
    }
}

/// Cosine at or above which a new fact's vector marks an existing live
/// fact as a likely duplicate worth showing (a paraphrase, or the same
/// fact in another language), on top of the keyword check.
const NEAR_DUPLICATE: f32 = 0.8;

/// Stores the vector of the fact a write produced and adds live facts
/// whose vectors are near it to `ins.similar`.
fn attach_vector(
    store: &MemoryStore,
    ins: &mut Inserted,
    content: &str,
    model: &str,
    vector: &[f32],
) -> Result<(), ferrule_memory::MemoryError> {
    if ins.decision == Decision::Noop {
        return Ok(());
    }
    store.set_embedding(ins.id, content, model, vector)?;
    let mut exclude = vec![ins.id];
    exclude.extend(&ins.replaced);
    exclude.extend(ins.similar.iter().map(|m| m.id));
    let near = store.similar_by_vector(
        ferrule_memory::QueryVector { model, vector },
        NEAR_DUPLICATE,
        &exclude,
        3,
    )?;
    ins.similar.extend(near);
    Ok(())
}

/// The vector for a fact about to be written, or `None` (keyword only).
async fn document_vector(
    embedding: &Option<Arc<Embedding>>,
    content: &str,
) -> Option<(String, Vec<f32>)> {
    let e = embedding.as_ref()?;
    let v = e.try_one(content, Purpose::Document).await?;
    Some((e.model().to_string(), v))
}

/// Same budget as the static block it replaced.
const RECALL_BUDGET_CHARS: usize = 2_000;

#[async_trait::async_trait]
impl SessionRecall for GoalRecall {
    async fn recall(&self, goal: &str) -> Option<String> {
        let Some(emb) = &self.embedding else {
            let db = self.db.clone();
            let goal = goal.to_string();
            let block = tokio::task::spawn_blocking(move || {
                MemoryStore::open(&db).and_then(|s| s.assemble_for_goal(&goal, RECALL_BUDGET_CHARS))
            })
            .await
            .ok()?
            .ok()?;
            return (!block.is_empty()).then(|| format!("[Long-term memory]\n{block}"));
        };
        // An empty store needs no vector (and a paid endpoint no call).
        let db = self.db.clone();
        let model = emb.model().to_string();
        let counts = tokio::task::spawn_blocking(move || {
            MemoryStore::open(&db).and_then(|s| s.embedding_counts(&model))
        })
        .await
        .ok()?
        .ok()?;
        if counts.live == 0 {
            return None;
        }
        let vector = emb.try_one(goal, Purpose::Query).await;
        let embedded = vector.is_some();
        let db = self.db.clone();
        let goal = goal.to_string();
        let hybrid = emb.hybrid;
        let model = emb.model().to_string();
        let block = tokio::task::spawn_blocking(move || {
            let qv = vector.as_deref().map(|v| ferrule_memory::QueryVector {
                model: &model,
                vector: v,
            });
            MemoryStore::open(&db)
                .and_then(|s| s.assemble_for_goal_hybrid(&goal, qv, RECALL_BUDGET_CHARS, &hybrid))
        })
        .await
        .ok()?
        .ok()?;
        if embedded && counts.live_embedded < counts.live {
            let (emb, db) = (emb.clone(), self.db.clone());
            tokio::spawn(async move { emb.catch_up(&db).await });
        }
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
    embedding: Option<Arc<Embedding>>,
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
                               session's task are recalled at its start. \
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
        let vector = document_vector(&self.embedding, &content).await;
        let ins = with_store("remember", &self.db, move |store| {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            let mut ins = store.insert(&content, &tags, &replaces)?;
            if let Some((model, v)) = vector {
                attach_vector(store, &mut ins, &content, &model, &v)?;
            }
            Ok(ins)
        })
        .await?;
        Ok(ToolOutput::ok(report(&ins, can_correct)))
    }
}

pub struct UpdateMemoryTool {
    db: PathBuf,
    embedding: Option<Arc<Embedding>>,
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
        let vector = document_vector(&self.embedding, &content).await;
        let ins = with_store("update_memory", &self.db, move |store| {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            let mut ins = store.supersede(id, &content, &tags)?;
            if let Some((model, v)) = vector {
                attach_vector(store, &mut ins, &content, &model, &v)?;
            }
            Ok(ins)
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
    embedding: Option<Arc<Embedding>>,
}

#[async_trait::async_trait]
impl Tool for RecallTool {
    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "recall".into(),
            // Unchanged without an embedder: the eval's tokens depend on it.
            description: if self.embedding.is_some() {
                "Search long-term memory by meaning and keywords (recent facts rank higher)."
            } else {
                "Search long-term memory (keyword search, recent facts rank higher)."
            }
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
        let vector = match &self.embedding {
            Some(e) => e
                .try_one(&query, Purpose::Query)
                .await
                .map(|v| (e.model().to_string(), v, e.hybrid)),
            None => None,
        };
        let found = with_store("recall", &self.db, move |store| match &vector {
            Some((model, v, hybrid)) => store.recall_hybrid(
                &query,
                Some(embedding::query_vector(model, v)),
                limit,
                hybrid,
            ),
            None => store.recall(&query, limit),
        })
        .await?;
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
        let hook = GoalRecall::new(db.clone());
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

    fn fake() -> (ferrule_embed::FakeEmbedder, Arc<Embedding>) {
        let fake = ferrule_embed::FakeEmbedder::new("t", 256);
        let emb = Embedding::new(Arc::new(fake.clone()), ferrule_memory::Hybrid::default());
        (fake, Arc::new(emb))
    }

    #[tokio::test]
    async fn with_an_embedder_writes_store_vectors_and_recall_finds_a_misspelling() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let (fake, emb) = fake();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        let full = tools_with(db.clone(), MemoryAccess::Full, Some(emb.clone()));
        by_name(&full, "remember")
            .call(
                json!({"content": "the staging database listens on port 5781"}),
                &ctx,
            )
            .await
            .unwrap();
        let store = MemoryStore::open(&db).unwrap();
        assert_eq!(
            store.embedding_counts(emb.model()).unwrap().live_embedded,
            1
        );

        // Keyword search alone misses it; the vector finds it.
        let keyword = tools(db.clone());
        let out = by_name(&keyword, "recall")
            .call(json!({"query": "stagng databse"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "no matching memories");
        let recall = by_name(&full, "recall");
        assert!(recall.definition().description.contains("by meaning"));
        assert!(!by_name(&keyword, "recall")
            .definition()
            .description
            .contains("meaning"));
        let out = recall
            .call(json!({"query": "stagng databse"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "#1 the staging database listens on port 5781");

        // A correction gets its own vector.
        by_name(&full, "update_memory")
            .call(
                json!({"id": 1, "content": "the staging database listens on port 6000"}),
                &ctx,
            )
            .await
            .unwrap();
        let c = store.embedding_counts(emb.model()).unwrap();
        assert_eq!((c.live, c.live_embedded), (1, 1));

        // The embedder goes down: keyword recall, no error.
        fake.set_failing(true);
        let out = recall
            .call(json!({"query": "staging database"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "#2 the staging database listens on port 6000");
        let out = by_name(&full, "remember")
            .call(json!({"content": "the deploy target is fly.io"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "remembered (#3)");
        assert_eq!(store.embedding_counts(emb.model()).unwrap().stale, 1);
    }

    #[tokio::test]
    async fn a_near_duplicate_by_vector_is_shown_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let (_, emb) = fake();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        let remember = by_name(
            &tools_with(db.clone(), MemoryAccess::Full, Some(emb)),
            "remember",
        );
        remember
            .call(
                json!({"content": "Deployments go to production every Thursday"}),
                &ctx,
            )
            .await
            .unwrap();
        // Too few shared words for the keyword check (2 of 10), but the
        // fake's trigrams put it at cosine 0.84.
        let out = remember
            .call(
                json!({"content": "Deploymentss go to productionn on Thursdayy"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.content
                .starts_with("remembered (#2). Similar live memories:\n#1 Deployments"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn goal_recall_embeds_the_goal_then_catches_up_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let (fake, emb) = fake();
        let hook = GoalRecall::new(db.clone()).with_embedding(Some(emb.clone()));
        assert_eq!(hook.recall("anything").await, None);
        assert_eq!(fake.calls(), 0, "an empty store needs no vector");
        let store = MemoryStore::open(&db).unwrap();
        store
            .remember("the staging database listens on port 5781", &[])
            .unwrap();
        store.remember("the deploy target is fly.io", &[]).unwrap();
        // No vectors yet: the misspelt goal matches nothing, and the
        // block is the newest facts.
        let goal = "connect to the stagng databse";
        let block = hook.recall(goal).await.unwrap();
        assert!(
            block.starts_with("[Long-term memory]\n- #2 the deploy target"),
            "{block}"
        );
        // That recall embedded them in the background; now it matches.
        let mut tries = 0;
        while store.embedding_counts(emb.model()).unwrap().stale > 0 {
            tries += 1;
            assert!(tries < 200, "the catch-up never ran");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let block = hook.recall(goal).await.unwrap();
        assert!(
            block.starts_with("[Long-term memory]\n- #1 the staging database"),
            "{block}"
        );
        // Down: the keyword block, exactly.
        fake.set_failing(true);
        assert_eq!(
            hook.recall("connect to the staging database").await,
            GoalRecall::new(db)
                .recall("connect to the staging database")
                .await
        );
    }
}
