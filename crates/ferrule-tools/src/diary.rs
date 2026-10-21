use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::path::PathBuf;

/// `.ferrule/<name>` in the workspace, resolved the way the file tools
/// resolve paths. These tools run in ferrule's own process, not the
/// sandbox, and a sandboxed command can make links in the workspace: a
/// link pointing outside it is refused, not followed.
fn state_file(ctx: &ToolContext, tool: &str, name: &str) -> Result<PathBuf, CoreError> {
    let path = crate::fs_tools::resolve(&ctx.workspace, &[], &format!(".ferrule/{name}")).map_err(
        |e| match e {
            CoreError::ToolFailed { message, .. } => CoreError::ToolFailed {
                tool: tool.into(),
                message,
            },
            other => other,
        },
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    Ok(path)
}

/// `write_todos` — the agent's own planning surface: its task list in
/// `.ferrule/TODO.md` (overwritten), and its diary, `.ferrule/diary.md`
/// (a `note` appended with a timestamp). Forces explicit planning, and the
/// files double as a trajectory artifact you can diff and debug after the
/// run. M58.3 merged `log_diary` into it as `note`.
pub struct WriteTodosTool;

#[async_trait::async_trait]
impl Tool for WriteTodosTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_todos".into(),
            description: "Write the current task list as markdown (overwrites), and/or append a \
                          `note` to the diary: a decision, a blocker, a milestone, especially \
                          before and after risky operations. Use the list at the start of \
                          multi-step work and update it as tasks complete. Persisted to \
                          .ferrule/TODO.md and .ferrule/diary.md in the workspace."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "string",
                        "description": "Full markdown task list, e.g. '- [x] done item\\n- [ ] pending item'"
                    },
                    "note": { "type": "string", "description": "One concise diary entry" }
                }
            }),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let note = args["note"].as_str().map(str::trim).unwrap_or("");
        let Some(todos) = args["todos"].as_str() else {
            if note.is_empty() {
                return Err(CoreError::ToolFailed {
                    tool: "write_todos".into(),
                    message: "give `todos`, a `note`, or both".into(),
                });
            }
            append_note(ctx, "write_todos", note).await?;
            return Ok(ToolOutput::ok("note logged"));
        };
        let path = state_file(ctx, "write_todos", "TODO.md")?;
        // The rename replaces a link put there since, rather than following it.
        crate::fs_tools::write_atomic(&path, todos.as_bytes()).map_err(|e| {
            CoreError::ToolFailed {
                tool: "write_todos".into(),
                message: e.to_string(),
            }
        })?;
        let open = todos.matches("- [ ]").count();
        let done = todos.matches("- [x]").count() + todos.matches("- [X]").count();
        let mut out = format!("todo list saved ({done} done, {open} open)");
        if !note.is_empty() {
            append_note(ctx, "write_todos", note).await?;
            out.push_str("; note logged");
        }
        Ok(ToolOutput::ok(out))
    }
}

/// `log_diary`, kept callable for one release after M58.3 merged it into
/// `write_todos` (`note`): never offered, and a call logs a deprecation
/// warning. The diary is how you see the trajectory: you don't fix bugs,
/// you fix prompts, tools and trajectories.
pub struct DiaryTool;

#[async_trait::async_trait]
impl Tool for DiaryTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "log_diary".into(),
            description: "Deprecated: use write_todos with a `note`.".into(),
            parameters: json!({"type": "object", "required": ["entry"]}),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        tracing::warn!(
            tool = "log_diary",
            instead = "write_todos",
            "a deprecated tool name was called (M58.3); it goes in the next release"
        );
        let entry = args["entry"].as_str().unwrap_or("").trim();
        if entry.is_empty() {
            return Err(CoreError::ToolFailed {
                tool: "log_diary".into(),
                message: "empty entry".into(),
            });
        }
        append_note(ctx, "log_diary", entry).await?;
        Ok(ToolOutput::ok("logged"))
    }

    fn replaced_by(&self) -> Option<&'static str> {
        Some("write_todos")
    }
}

/// One timestamped line at the end of `.ferrule/diary.md`.
async fn append_note(ctx: &ToolContext, tool: &str, entry: &str) -> Result<(), CoreError> {
    let failed = |e: std::io::Error| CoreError::ToolFailed {
        tool: tool.into(),
        message: e.to_string(),
    };
    let path = state_file(ctx, tool, "diary.md")?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("- [t{ts}] {entry}\n");
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(failed)?;
    f.write_all(line.as_bytes()).await.map_err(failed)?;
    // tokio's File hands the write to a blocking task; without flush the
    // entry may not have landed when we return (or when the next append opens).
    f.flush().await.map_err(failed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn todos_overwrite_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        let out = WriteTodosTool
            .call(
                json!({"todos": "- [x] setup\n- [ ] build\n- [ ] ship"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.content.contains("1 done, 2 open"));
        let text = std::fs::read_to_string(dir.path().join(".ferrule/TODO.md")).unwrap();
        assert!(text.contains("ship"));
    }

    #[tokio::test]
    async fn diary_appends() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 1_000,
        };
        DiaryTool
            .call(json!({"entry": "started refactor"}), &ctx)
            .await
            .unwrap();
        DiaryTool
            .call(json!({"entry": "tests green"}), &ctx)
            .await
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join(".ferrule/diary.md")).unwrap();
        assert!(text.contains("started refactor"));
        assert!(text.contains("tests green"));
        assert_eq!(text.lines().count(), 2);
    }
}
