//! `shell`, `read_file`, `write_file`, `edit_file` and `list_dir` on the
//! remote: the same names, schemas and answers as the local tools, so the
//! model (and the eval) sees no difference but the description.

use crate::link::{tail, Data, Link, Remote, Status};
use crate::script::{Op, READ_CAP};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::Verifier;
use ferrule_tools::edit::{edit_bytes, parse_hunks, Edited};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Shell output past this is dropped before the context cap even looks at
/// it: it only protects ferrule's memory.
const SHELL_STREAM_CAP: usize = 16 * 1024 * 1024;

fn failed(tool: &str, message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

/// `path` against the remote workspace, `.` and `..` folded. A relative
/// path that climbs out is refused here, before any round trip; the
/// remote checks the real path (symlinks) either way.
pub fn join_lexical(ws: &str, path: &str) -> Option<String> {
    let full = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{ws}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for c in full.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let out = format!("/{}", parts.join("/"));
    let ws = ws.trim_end_matches('/');
    let inside = out == ws || out.starts_with(&format!("{ws}/")) || ws.is_empty();
    (path.starts_with('/') || inside).then_some(out)
}

/// The message for a script's refusal, worded as the local tools word it.
fn refusal(tool: &str, code: &str, shown: &str, remote: &Remote) -> CoreError {
    match code {
        "escape" => failed(
            "fs",
            format!("path `{shown}` escapes workspace `{}`", remote.workspace),
        ),
        "hidden" => failed(
            "fs",
            format!(
                "`{shown}` is off limits: a path the sandbox's read policy denies (credentials, browser profiles, `deny_read`); the file tools don't touch it"
            ),
        ),
        "missing" => failed(tool, format!("{shown}: No such file or directory")),
        "dir" => failed(tool, format!("{shown}: Is a directory")),
        "notdir" => failed(tool, format!("{shown}: Not a directory")),
        "changed" => failed(
            tool,
            format!("`{shown}` changed on the remote while it was being edited; nothing was written. Read it again and redo the edit"),
        ),
        other => failed(tool, format!("{shown}: refused ({other})")),
    }
}

/// Connect, and resolve the model's path.
async fn prepare(link: &Link, tool: &str, shown: &str) -> Result<(Remote, String), CoreError> {
    let remote = link.connect().await.map_err(|e| failed(tool, e))?;
    let path = join_lexical(&remote.workspace, shown).ok_or_else(|| {
        failed(
            "fs",
            format!("path `{shown}` escapes workspace `{}`", remote.workspace),
        )
    })?;
    Ok((remote, path))
}

/// The file's bytes, or `None` if it doesn't exist.
async fn read_bytes(
    link: &Link,
    tool: &str,
    remote: &Remote,
    path: &str,
    shown: &str,
) -> Result<Option<Vec<u8>>, CoreError> {
    let ran = link
        .read_file(remote, path)
        .await
        .map_err(|e| failed(tool, e))?;
    match &ran.status {
        Status::Exit(0) if ran.stdout.len() > READ_CAP => Err(failed(
            tool,
            format!(
                "{shown}: larger than {} MiB; read part of it with the shell tool (head, sed -n)",
                READ_CAP >> 20
            ),
        )),
        Status::Exit(0) => Ok(Some(ran.stdout)),
        Status::Fail(code) if code == "missing" => Ok(None),
        Status::Fail(code) => Err(refusal(tool, code, shown, remote)),
        other => Err(failed(
            tool,
            link.unexpected(&format!("reading {shown}"), other, &ran),
        )),
    }
}

/// Write `bytes`, guarded: `guard` is `""` (whatever is there), `missing`
/// (nothing may be), or a `cksum` the file must still have. Returns the
/// real path written.
async fn write_bytes(
    link: &Link,
    tool: &str,
    remote: &Remote,
    path: &str,
    shown: &str,
    bytes: Vec<u8>,
    guard: &str,
) -> Result<String, CoreError> {
    let mut params = link.file_params(remote);
    params.push(path.to_string());
    params.push(guard.to_string());
    let ran = link
        .exec(
            Op::Write,
            params,
            Data::Close(bytes),
            1 << 16,
            Some(Duration::from_secs(120)),
            None,
        )
        .await
        .map_err(|e| failed(tool, e))?;
    match &ran.status {
        Status::Exit(0) => Ok(String::from_utf8_lossy(&ran.stdout).trim_end().to_string()),
        Status::Fail(code) => Err(refusal(tool, code, shown, remote)),
        Status::Interrupted => Err(failed(
            tool,
            format!("{}: the connection dropped while writing `{shown}`; the file is either the old one or the new one (the write is a rename), read it to see which", link.target().label),
        )),
        other => Err(failed(tool, link.unexpected(&format!("writing {shown}"), other, &ran))),
    }
}

pub struct RemoteReadFile(pub Arc<Link>);
pub struct RemoteWriteFile(pub Arc<Link>);
pub struct RemoteEditFile(pub Arc<Link>);
pub struct RemoteListDir(pub Arc<Link>);

fn where_(link: &Link) -> String {
    format!("the remote workspace ({})", link.target().describe())
}

#[async_trait::async_trait]
impl Tool for RemoteReadFile {
    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".into(),
            description: format!(
                "Read a UTF-8 text file inside {}. Returns file contents.",
                where_(&self.0)
            ),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Path relative to workspace (or absolute within it)" } },
                "required": ["path"]
            }),
        }
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let shown = args["path"].as_str().unwrap_or("");
        let (remote, path) = prepare(&self.0, "read_file", shown).await?;
        let bytes = read_bytes(&self.0, "read_file", &remote, &path, shown)
            .await?
            .ok_or_else(|| refusal("read_file", "missing", &path, &remote))?;
        let text = String::from_utf8(bytes).map_err(|_| {
            failed(
                "read_file",
                format!("{path}: stream did not contain valid UTF-8"),
            )
        })?;
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

#[async_trait::async_trait]
impl Tool for RemoteWriteFile {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".into(),
            description: format!(
                "Create a new file, or replace a whole file, inside {} (parent dirs are created). \
                 To change part of an existing file use edit_file: cheaper and safer.",
                where_(&self.0)
            ),
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
    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let shown = args["path"].as_str().unwrap_or("");
        let (remote, path) = prepare(&self.0, "write_file", shown).await?;
        let content = args["content"].as_str().unwrap_or("").to_string();
        let n = content.len();
        let real = write_bytes(
            &self.0,
            "write_file",
            &remote,
            &path,
            shown,
            content.into_bytes(),
            "",
        )
        .await?;
        Ok(ToolOutput::ok(format!("wrote {n} bytes to {real}")))
    }
}

#[async_trait::async_trait]
impl Tool for RemoteEditFile {
    fn definition(&self) -> ToolDefinition {
        let mut def = ferrule_tools::edit::EditFileTool::default().definition();
        def.description = def.description.replacen(
            "Change part of an existing file",
            &format!("Change part of an existing file in {}", where_(&self.0)),
            1,
        );
        def
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let fail = |m: String| failed("edit_file", m);
        let shown = args["path"].as_str().unwrap_or("").to_string();
        if shown.is_empty() {
            return Err(fail("`path` is required".into()));
        }
        let hunks = parse_hunks(&args).map_err(fail)?;
        let (remote, path) = prepare(&self.0, "edit_file", &shown).await?;
        let existing = read_bytes(&self.0, "edit_file", &remote, &path, &shown).await?;
        let guard = match &existing {
            Some(b) => crate::script::cksum(b),
            None => "missing".into(),
        };
        match edit_bytes(existing.as_deref(), &shown, &hunks).map_err(fail)? {
            Edited::Unchanged(msg) => Ok(ToolOutput::ok(msg)),
            Edited::Write { bytes, summary } => {
                write_bytes(&self.0, "edit_file", &remote, &path, &shown, bytes, &guard).await?;
                Ok(ToolOutput::capped(summary, ctx.max_output_chars))
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for RemoteListDir {
    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".into(),
            description: format!(
                "List files and directories at a path inside {}.",
                where_(&self.0)
            ),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Directory path; use \".\" for workspace root" } },
                "required": ["path"]
            }),
        }
    }
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let shown = args["path"].as_str().unwrap_or(".");
        let (remote, path) = prepare(&self.0, "list_dir", shown).await?;
        let mut params = self.0.file_params(&remote);
        params.push(path.clone());
        let ran = self
            .0
            .exec(
                Op::List,
                params,
                Data::Close(Vec::new()),
                16 << 20,
                Some(Duration::from_secs(120)),
                None,
            )
            .await
            .map_err(|e| failed("list_dir", e))?;
        match &ran.status {
            Status::Exit(0) => {}
            Status::Fail(code) => return Err(refusal("list_dir", code, &path, &remote)),
            other => {
                return Err(failed(
                    "list_dir",
                    self.0.unexpected(&format!("listing {shown}"), other, &ran),
                ))
            }
        }
        let text = String::from_utf8_lossy(&ran.stdout);
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        lines.sort();
        Ok(ToolOutput::capped(lines.join("\n"), ctx.max_output_chars))
    }
}

/// `shell` on the remote: same deny list, timeout and output format as the
/// local one. Nothing of ferrule's local sandbox applies there: the remote
/// account is the boundary.
pub struct RemoteShellTool {
    pub link: Arc<Link>,
    pub timeout: Duration,
    pub deny_patterns: Vec<&'static str>,
}

impl RemoteShellTool {
    pub fn new(link: Arc<Link>) -> Self {
        let local = ferrule_tools::shell::ShellTool::default();
        Self {
            link,
            timeout: local.timeout,
            deny_patterns: local.deny_patterns,
        }
    }

    fn is_denied(&self, cmd: &str) -> bool {
        let lower = cmd.to_lowercase();
        self.deny_patterns
            .iter()
            .any(|p| lower.contains(&p.to_lowercase()))
    }
}

/// Run `cmd` in the remote workspace: its exit code and its stdout, then
/// any stderr. An interrupted command is an error, never a result.
pub async fn run_remote(
    link: &Link,
    cmd: &str,
    timeout: Duration,
) -> Result<(i32, String), String> {
    let remote = link.connect().await?;
    let rport = link.forward_port().await?;
    let env = link.forward_env(rport).await?;
    let mut params = vec![
        remote.workspace.clone(),
        cmd.to_string(),
        env.len().to_string(),
    ];
    for (name, value) in env {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        params.push(format!("{name}={value}"));
    }
    // A skipped name would leave the count wrong.
    let n = params.len() - 3;
    params[2] = n.to_string();
    let ran = link
        .exec(
            Op::Shell,
            params,
            Data::HoldOpen,
            SHELL_STREAM_CAP,
            Some(timeout),
            rport,
        )
        .await?;
    let mut text = String::from_utf8_lossy(&ran.stdout).into_owned();
    if ran.stdout_cut {
        text.push_str(&format!(
            "\n…[output past {} MiB dropped]",
            SHELL_STREAM_CAP >> 20
        ));
    }
    if !ran.stderr.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&ran.stderr);
    }
    match ran.status {
        Status::Exit(code) => Ok((code, text)),
        Status::Fail(code) if code == "nows" => Err(format!(
            "{}: the remote workspace `{}` is gone",
            link.target().label,
            remote.workspace
        )),
        Status::Fail(code) => Err(format!("{}: refused ({code})", link.target().label)),
        Status::Interrupted => Err(format!(
            "interrupted: the connection to {} dropped while the command ran, so it did NOT finish \
             (it may have partly run; check before re-running it). Output so far:\n{}",
            link.target().label,
            tail(&text, 4000)
        )),
    }
}

#[async_trait::async_trait]
impl Tool for RemoteShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: format!(
                "Run a shell command (POSIX sh) in {} over SSH. \
                 Use for builds, tests, git, search. Output is truncated if very long. \
                 stdin is closed, so interactive prompts fail. \
                 The command runs as the remote account, with that account's rights: ferrule's local sandbox doesn't reach there.",
                where_(&self.link)
            ),
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "Shell command to execute" } },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let cmd = args["command"].as_str().unwrap_or("").to_string();
        if cmd.trim().is_empty() {
            return Err(failed("shell", "empty command"));
        }
        if self.is_denied(&cmd) {
            return Err(failed(
                "shell",
                format!("command blocked by deny list: `{cmd}`"),
            ));
        }
        let (code, mut text) = run_remote(&self.link, &cmd, self.timeout)
            .await
            .map_err(|e| failed("shell", e))?;
        text.push_str(&format!("\n[exit code: {code}]"));
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

/// The owner's `verify_command`, run in the remote workspace.
pub struct RemoteVerifier {
    pub link: Arc<Link>,
    pub command: String,
    pub timeout: Duration,
}

#[async_trait::async_trait]
impl Verifier for RemoteVerifier {
    fn describe(&self) -> String {
        format!("{} (on {})", self.command, self.link.target().label)
    }

    async fn verify(&self, ctx: &ToolContext) -> Result<(), String> {
        let (code, output) = run_remote(&self.link, &self.command, self.timeout).await?;
        if code == 0 {
            return Ok(());
        }
        Err(format!(
            "{}\n[exit code: {code}]",
            tail(&output, ctx.max_output_chars)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_join_folds_and_refuses_climbing_out() {
        assert_eq!(
            join_lexical("/srv/app", "src/./a.rs").unwrap(),
            "/srv/app/src/a.rs"
        );
        assert_eq!(join_lexical("/srv/app", ".").unwrap(), "/srv/app");
        assert_eq!(join_lexical("/srv/app", "a/../b").unwrap(), "/srv/app/b");
        assert!(join_lexical("/srv/app", "../app2/x").is_none());
        assert!(join_lexical("/srv/app", "../../etc/passwd").is_none());
        // Absolute paths go to the remote's real-path check.
        assert_eq!(
            join_lexical("/srv/app", "/etc/../srv/app/x").unwrap(),
            "/srv/app/x"
        );
        assert_eq!(join_lexical("/", "etc").unwrap(), "/etc");
    }
}
