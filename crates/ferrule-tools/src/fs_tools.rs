use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Resolve a model-supplied path against the workspace, rejecting escapes.
fn resolve(workspace: &Path, path: &str) -> Result<PathBuf, CoreError> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace.join(path)
    };
    // Lexical normalization (no symlink chase needed for scope check MVP).
    let mut norm = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            std::path::Component::ParentDir => {
                norm.pop();
            }
            other => norm.push(other.as_os_str()),
        }
    }
    let ws = {
        let mut n = PathBuf::new();
        for comp in workspace.components() {
            if comp != std::path::Component::ParentDir {
                n.push(comp.as_os_str());
            }
        }
        n
    };
    if !norm.starts_with(&ws) {
        return Err(CoreError::ToolFailed {
            tool: "fs".into(),
            message: format!("path `{path}` escapes workspace `{}`", ws.display()),
        });
    }
    Ok(norm)
}

pub struct ReadFileTool;

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a UTF-8 text file inside the workspace. Returns file contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Path relative to workspace (or absolute within it)" } },
                "required": ["path"]
            }),
        }
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let path = resolve(&ctx.workspace, args["path"].as_str().unwrap_or(""))?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| CoreError::ToolFailed { tool: "read_file".into(), message: format!("{}: {e}", path.display()) })?;
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

pub struct WriteFileTool;

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: "Write text content to a file inside the workspace, creating parent dirs.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let path = resolve(&ctx.workspace, args["path"].as_str().unwrap_or(""))?;
        let content = args["content"].as_str().unwrap_or("");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| CoreError::ToolFailed { tool: "write_file".into(), message: format!("{}: {e}", path.display()) })?;
        Ok(ToolOutput::ok(format!("wrote {} bytes to {}", content.len(), path.display())))
    }
}

pub struct ListDirTool;

#[async_trait::async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".into(),
            description: "List files and directories at a path inside the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Directory path; use \".\" for workspace root" } },
                "required": ["path"]
            }),
        }
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let path = resolve(&ctx.workspace, args["path"].as_str().unwrap_or("."))?;
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| CoreError::ToolFailed { tool: "list_dir".into(), message: format!("{}: {e}", path.display()) })?;
        let mut lines = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) { "dir" } else { "file" };
            lines.push(format!("{kind}\t{name}"));
        }
        lines.sort();
        Ok(ToolOutput::capped(lines.join("\n"), ctx.max_output_chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext { workspace: dir.to_path_buf(), max_output_chars: 1_000 }
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        WriteFileTool
            .call(json!({"path": "sub/a.txt", "content": "hello"}), &c)
            .await
            .unwrap();
        let out = ReadFileTool.call(json!({"path": "sub/a.txt"}), &c).await.unwrap();
        assert_eq!(out.content, "hello");
    }

    #[tokio::test]
    async fn path_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let err = ReadFileTool.call(json!({"path": "../../etc/passwd"}), &c).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"), "{err}");
        let err2 = WriteFileTool.call(json!({"path": "/tmp/evil.txt", "content": "x"}), &c).await;
        assert!(err2.is_err());
    }

    #[tokio::test]
    async fn list_dir_shows_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        let out = ListDirTool.call(json!({"path": "."}), &ctx(dir.path())).await.unwrap();
        assert!(out.content.contains("file\tb.txt"));
        assert!(out.content.contains("dir\tadir"));
    }
}
