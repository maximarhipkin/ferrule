use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Resolve a model-supplied path against the workspace, rejecting escapes.
///
/// The check is on real paths, so a symlink inside the workspace that points
/// out of it is an escape too. The returned path is the resolved one, and the
/// tools act on that rather than re-walking the model's string. (A symlink
/// swapped in between this check and the write still gets through; the
/// sandbox, not this function, is what holds against a hostile workspace.)
fn resolve(workspace: &Path, path: &str) -> Result<PathBuf, CoreError> {
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workspace.join(path)
    };
    let ws = lexical(workspace);
    let escapes = || CoreError::ToolFailed {
        tool: "fs".into(),
        message: format!("path `{path}` escapes workspace `{}`", ws.display()),
    };
    let ws_real = ws.canonicalize().unwrap_or_else(|_| ws.clone());
    match real(&lexical(&candidate)) {
        Some(resolved) if resolved.starts_with(&ws_real) => Ok(resolved),
        _ => Err(escapes()),
    }
}

/// Drop `.` and apply `..` without touching the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut norm = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                norm.pop();
            }
            std::path::Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    norm
}

/// Canonicalize the deepest part of `path` that exists and re-append the
/// rest (the part a write is about to create). `None` when an existing part
/// can't be resolved — a dangling symlink, say, which a write would follow
/// to wherever it points.
fn real(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::new();
    while existing.symlink_metadata().is_err() {
        rest.push(existing.file_name()?);
        existing = existing.parent()?;
    }
    let mut resolved = existing.canonicalize().ok()?;
    resolved.extend(rest.iter().rev());
    Some(resolved)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_workspace_are_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s3cret").unwrap();
        let c = ctx(dir.path());
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("new"), dir.path().join("dangling")).unwrap();

        let err = ReadFileTool.call(json!({"path": "out/secret"}), &c).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"), "{err}");
        let err = WriteFileTool.call(json!({"path": "out/planted", "content": "x"}), &c).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"), "{err}");
        let err = WriteFileTool.call(json!({"path": "dangling", "content": "x"}), &c).await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"), "{err}");
        assert!(!outside.path().join("planted").exists() && !outside.path().join("new").exists());

        // `..` is applied before any lookup, so it can't be walked out of a link.
        WriteFileTool.call(json!({"path": "out/../ok.txt", "content": "x"}), &c).await.unwrap();
        assert!(dir.path().join("ok.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_within_the_workspace_still_work() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("alias")).unwrap();
        let c = ctx(dir.path());
        WriteFileTool.call(json!({"path": "alias/new/f.txt", "content": "hi"}), &c).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("real/new/f.txt")).unwrap(), "hi");
        let out = ReadFileTool.call(json!({"path": "alias/new/f.txt"}), &c).await.unwrap();
        assert_eq!(out.content, "hi");
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
