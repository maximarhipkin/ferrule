use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::time::Duration;

/// Shell execution with deny-by-default-obvious-danger patterns and a hard
/// timeout. Runs inside the workspace directory. Real deployments should add
/// OS-level sandboxing (Landlock/Bubblewrap/Seatbelt) behind this tool.
pub struct ShellTool {
    pub timeout: Duration,
    pub deny_patterns: Vec<&'static str>,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            deny_patterns: vec![
                "rm -rf /", "rm -rf ~", "rm -rf *", "mkfs", ":(){", "dd if=",
                "sudo ", "doas ", "shutdown", "reboot", "> /dev/sd", "chmod -R 777 /",
            ],
        }
    }
}

impl ShellTool {
    fn is_denied(&self, cmd: &str) -> bool {
        let lower = cmd.to_lowercase();
        self.deny_patterns.iter().any(|p| lower.contains(&p.to_lowercase()))
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: "Run a shell command inside the workspace directory. \
                          Use for builds, tests, git, search. Output is truncated \
                          if very long."
                .into(),
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
            return Err(CoreError::ToolFailed { tool: "shell".into(), message: "empty command".into() });
        }
        if self.is_denied(&cmd) {
            return Err(CoreError::ToolFailed { tool: "shell".into(), message: format!("command blocked by deny list: `{cmd}`") });
        }

        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let flag = if cfg!(windows) { "/C" } else { "-c" };
        let fut = tokio::process::Command::new(shell)
            .arg(flag)
            .arg(&cmd)
            .current_dir(&ctx.workspace)
            .output();

        let out = tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| CoreError::ToolFailed { tool: "shell".into(), message: format!("timeout after {:?}", self.timeout) })?
            .map_err(|e| CoreError::ToolFailed { tool: "shell".into(), message: e.to_string() })?;

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
}
