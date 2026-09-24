//! A command hook: the platform's shell runs it as the owner, outside the
//! sandbox, with the payload as JSON on stdin. On timeout the whole
//! process tree is killed, not just the shell at the top.

use ferrule_core::lifecycle::{HookHandler, HookInput, HookRun};
use ferrule_core::tool::ToolContext;
use ferrule_sandbox::Shell;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Kept per stream; the rest is read and dropped so the hook never blocks
/// on a full pipe.
pub const MAX_STREAM_BYTES: usize = 64 * 1024;

/// How long to wait for stdout/stderr to close once the shell has exited
/// (a hook may leave a child holding them open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

pub struct CommandHook {
    pub command: String,
    pub timeout: Duration,
}

impl CommandHook {
    pub fn new(command: impl Into<String>, timeout: Duration) -> CommandHook {
        CommandHook {
            command: command.into(),
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl HookHandler for CommandHook {
    fn command(&self) -> String {
        self.command.clone()
    }

    async fn run(&self, input: &HookInput, ctx: &ToolContext) -> HookRun {
        let payload = serde_json::to_vec(input).unwrap_or_default();
        let shell = Shell::get();
        let mut cmd = Command::new(&shell.program);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.args(shell.args(&self.command))
            .current_dir(&ctx.workspace)
            .env("FERRULE_PROJECT_DIR", &ctx.workspace)
            // Claude Code's name for it, so its hook scripts port over.
            .env("CLAUDE_PROJECT_DIR", &ctx.workspace)
            .env("FERRULE_HOOK_EVENT", &input.hook_event_name)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return HookRun {
                    spawn_error: Some(e.to_string()),
                    ..Default::default()
                }
            }
        };
        let pid = child.id();

        // A hook that never reads stdin must not stall on it: write from
        // a task and ignore a closed pipe.
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin.write_all(&payload).await;
                let _ = stdin.shutdown().await;
            });
        }
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let readers = [
            child
                .stdout
                .take()
                .map(|s| tokio::spawn(read_capped(s, out.clone()))),
            child
                .stderr
                .take()
                .map(|s| tokio::spawn(read_capped(s, err.clone()))),
        ];

        let (exit_code, timed_out) = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(_)) => (None, false),
            Err(_) => {
                if let Some(pid) = pid {
                    kill_tree(pid);
                }
                let _ = child.kill().await;
                (None, true)
            }
        };
        for reader in readers.into_iter().flatten() {
            let abort = reader.abort_handle();
            if tokio::time::timeout(DRAIN_GRACE, reader).await.is_err() {
                abort.abort();
            }
        }
        let text = |buf: &Arc<Mutex<Vec<u8>>>| {
            String::from_utf8_lossy(&buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
        };
        HookRun {
            exit_code,
            stdout: text(&out),
            stderr: text(&err),
            timed_out,
            spawn_error: None,
        }
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut stream: R, into: Arc<Mutex<Vec<u8>>>) {
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut kept = into.lock().unwrap_or_else(|e| e.into_inner());
                let room = MAX_STREAM_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

/// Kills the hook and everything it started.
#[cfg(unix)]
fn kill_tree(pid: u32) {
    ferrule_sandbox::kill_process_group(pid);
}

/// Kills the hook and everything it started.
#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_tree(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            workspace: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn input() -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "s1".into(),
            tool_name: Some("shell".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn the_payload_arrives_on_stdin_and_the_exit_code_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let hook = CommandHook::new(
            "cat > payload.json; echo \"$FERRULE_HOOK_EVENT\"; echo nope >&2; exit 2",
            Duration::from_secs(10),
        );
        let run = hook.run(&input(), &ctx(dir.path())).await;
        assert_eq!(run.exit_code, Some(2));
        assert_eq!(run.stdout.trim(), "PreToolUse");
        assert_eq!(run.stderr.trim(), "nope");
        let got: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("payload.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(got["tool_name"], "shell");
        assert_eq!(got["session_id"], "s1");
    }

    #[tokio::test]
    async fn a_hung_hook_and_its_children_are_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // The child writes a marker if it survives the kill.
        let hook = CommandHook::new(
            "(sleep 3; touch survived) & sleep 30",
            Duration::from_millis(300),
        );
        let started = Instant::now();
        let run = hook.run(&input(), &ctx(dir.path())).await;
        assert!(run.timed_out);
        assert_eq!(run.exit_code, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !dir.path().join("survived").exists(),
            "a child outlived the kill"
        );
    }

    #[tokio::test]
    async fn output_is_capped_and_a_hook_that_ignores_stdin_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let hook = CommandHook::new(
            "head -c 200000 /dev/zero | tr '\\0' x",
            Duration::from_secs(10),
        );
        let run = hook.run(&input(), &ctx(dir.path())).await;
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.stdout.len(), MAX_STREAM_BYTES);
    }

    #[tokio::test]
    async fn a_missing_workspace_is_a_spawn_error() {
        let hook = CommandHook::new("true", Duration::from_secs(5));
        let run = hook
            .run(&input(), &ctx(std::path::Path::new("/nonexistent/ferrule")))
            .await;
        assert!(run.spawn_error.is_some());
    }
}
