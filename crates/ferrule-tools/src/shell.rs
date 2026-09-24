use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_sandbox::{Sandbox, Shell};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// Shell execution inside the workspace directory, with a hard timeout.
/// Containment is the `sandbox`'s job (kernel-enforced write roots, optional
/// network cut-off, secret env vars dropped); the deny-list only turns away
/// the obvious cases early with a clearer message than "Permission denied".
pub struct ShellTool {
    pub timeout: Duration,
    pub deny_patterns: Vec<&'static str>,
    pub sandbox: Arc<Sandbox>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            deny_patterns: vec![
                "rm -rf /", "rm -rf ~", "rm -rf *", "mkfs", ":(){", "dd if=",
                "sudo ", "doas ", "shutdown", "reboot", "> /dev/sd", "chmod -R 777 /",
            ],
            sandbox: Arc::new(Sandbox::off()),
        }
    }
}

impl ShellTool {
    pub fn sandboxed(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox, ..Self::default() }
    }

    fn failed(message: impl Into<String>) -> CoreError {
        CoreError::ToolFailed { tool: "shell".into(), message: message.into() }
    }

    fn is_denied(&self, cmd: &str) -> bool {
        let lower = cmd.to_lowercase();
        self.deny_patterns.iter().any(|p| lower.contains(&p.to_lowercase()))
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let mut description = "Run a shell command inside the workspace directory. \
                               Use for builds, tests, git, search. Output is truncated \
                               if very long. stdin is closed, so interactive prompts fail."
            .to_string();
        if let Some(note) = Shell::get().model_note() {
            description.push(' ');
            description.push_str(&note);
        }
        if let Some(note) = self.sandbox.model_note() {
            description.push(' ');
            description.push_str(&note);
        }
        ToolDefinition {
            name: "shell".into(),
            description,
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
            return Err(Self::failed("empty command"));
        }
        if self.is_denied(&cmd) {
            return Err(Self::failed(format!("command blocked by deny list: `{cmd}`")));
        }

        let shell = Shell::get();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut std_cmd = self
            .sandbox
            .command(&shell.program, shell.args(&cmd), &ctx.workspace)
            .map_err(|e| Self::failed(format!("sandbox setup failed: {e}")))?;
        // Its own process group, so a timeout can take down everything the
        // command started, not just the `sh` at the top.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut std_cmd, 0);
        let mut command = tokio::process::Command::from(std_cmd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| Self::failed(e.to_string()))?;
        let pid = child.id();

        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => out.map_err(|e| Self::failed(e.to_string()))?,
            Err(_) => {
                if let Some(pid) = pid {
                    ferrule_sandbox::kill_process_group(pid);
                }
                return Err(Self::failed(format!("timeout after {:?}", self.timeout)));
            }
        };

        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !stderr.trim().is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(&stderr);
        }
        text.push_str(&format!("\n[exit code: {}]", out.status.code().unwrap_or(-1)));
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext { workspace: std::env::temp_dir().canonicalize().unwrap_or(PathBuf::from("/tmp")), max_output_chars: 10_000 }
    }

    #[tokio::test]
    async fn runs_command_and_reports_exit_code() {
        let out = ShellTool::default().call(json!({"command": "echo hello"}), &ctx()).await.unwrap();
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("[exit code: 0]"));
    }

    #[tokio::test]
    async fn deny_list_blocks_dangerous_commands() {
        let t = ShellTool::default();
        assert!(t.is_denied("sudo rm -rf /"));
        assert!(t.is_denied("RM -RF /home"));
        let err = t.call(json!({"command": "sudo apt update"}), &ctx()).await.unwrap_err();
        assert!(err.to_string().contains("blocked"));
    }

    #[tokio::test]
    async fn stdin_is_closed_and_secrets_are_scrubbed() {
        std::env::set_var("FERRULE_SHELL_TEST_TOKEN", "leak-me");
        let out = ShellTool::default()
            .call(json!({"command": "cat; echo \"[${FERRULE_SHELL_TEST_TOKEN:-scrubbed}]\""}), &ctx())
            .await
            .unwrap();
        assert!(out.content.contains("[scrubbed]"), "{}", out.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolContext { workspace: dir.path().to_path_buf(), max_output_chars: 1_000 };
        let t = ShellTool { timeout: Duration::from_millis(500), ..ShellTool::default() };
        // A grandchild that would write a marker after the timeout fires.
        let err = t
            .call(json!({"command": "(sleep 2; touch late) & sleep 5"}), &c)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timeout"));
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(!dir.path().join("late").exists(), "background child survived the timeout");
    }

    #[tokio::test]
    async fn sandboxed_shell_cannot_write_outside_the_workspace() {
        let sb = Sandbox::new(ferrule_sandbox::Policy { tmp: false, ..Default::default() }).unwrap();
        if !sb.is_active() {
            eprintln!("skipping: {}", sb.degraded().unwrap_or("no sandbox"));
            return;
        }
        let t = ShellTool::sandboxed(Arc::new(sb));
        assert!(t.definition().description.contains("sandbox"));
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let c = ToolContext { workspace: ws.path().canonicalize().unwrap(), max_output_chars: 1_000 };
        let target = outside.path().join("x");
        let out = t
            .call(json!({"command": format!("echo in > ok && echo out > {}", target.display())}), &c)
            .await
            .unwrap();
        assert!(ws.path().join("ok").exists());
        assert!(!target.exists());
        assert!(out.content.contains("Permission denied"), "{}", out.content);
    }
}
