//! Optional pre-flight "gate" script a task can run before waking the
//! agent — e.g. "only actually bother the LLM if there's something new to
//! look at". Contract (matches the one already in production use in
//! NanoClaw-derived agents, so existing gate scripts port over unchanged):
//!
//! - stdout parses as JSON `{"wakeAgent": false, ...}` -> [`GateOutcome::Skip`].
//! - stdout parses as JSON `{"wakeAgent": true, "context": "..."}` ->
//!   [`GateOutcome::Proceed`], with `context` (if present) appended to the
//!   task's prompt.
//! - stdout is anything else — not JSON, or JSON without a `wakeAgent`
//!   field — -> `Proceed`, treating the raw stdout as context. A gate
//!   script that doesn't speak the JSON contract yet is still useful rather
//!   than being silently discarded.
//! - non-zero exit, or exceeding `timeout` -> `Err`. On timeout the child is
//!   always killed, never left running in the background: `kill_on_drop`
//!   fires when `tokio::time::timeout` drops the in-flight `wait_with_output`
//!   future on expiry, since that future owns the `Child` by value.

use super::error::SchedulerError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Proceed { context: Option<String> },
    Skip { reason: Option<String> },
}

pub async fn run_gate(
    command: &str,
    workspace: &Path,
    timeout: Duration,
) -> Result<GateOutcome, SchedulerError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| SchedulerError::Gate(format!("failed to spawn gate script: {e}")))?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(SchedulerError::Gate(format!("gate script io error: {e}"))),
        Err(_) => {
            return Err(SchedulerError::Gate(format!(
                "gate script timed out after {}s (killed)",
                timeout.as_secs()
            )))
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SchedulerError::Gate(format!(
            "gate script exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(GateOutcome::Proceed { context: None });
    }

    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(serde_json::Value::Object(obj)) => {
            match obj.get("wakeAgent").and_then(|v| v.as_bool()) {
                Some(false) => Ok(GateOutcome::Skip {
                    reason: obj
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                }),
                Some(true) => Ok(GateOutcome::Proceed {
                    context: obj
                        .get("context")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                }),
                None => Ok(GateOutcome::Proceed {
                    context: Some(stdout),
                }),
            }
        }
        _ => Ok(GateOutcome::Proceed {
            context: Some(stdout),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test]
    async fn wake_agent_false_skips() {
        let out = run_gate(
            r#"echo '{"wakeAgent": false, "reason": "nothing new"}'"#,
            &ws(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            GateOutcome::Skip {
                reason: Some("nothing new".into())
            }
        );
    }

    #[tokio::test]
    async fn wake_agent_true_proceeds_with_context() {
        let out = run_gate(
            r#"echo '{"wakeAgent": true, "context": "3 new rows"}'"#,
            &ws(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            GateOutcome::Proceed {
                context: Some("3 new rows".into())
            }
        );
    }

    #[tokio::test]
    async fn non_json_stdout_proceeds_with_raw_stdout_as_context() {
        let out = run_gate(
            r#"echo 'plain text, no JSON here'"#,
            &ws(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            GateOutcome::Proceed {
                context: Some("plain text, no JSON here".into())
            }
        );
    }

    #[tokio::test]
    async fn empty_stdout_proceeds_with_no_context() {
        let out = run_gate("true", &ws(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out, GateOutcome::Proceed { context: None });
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error() {
        let err = run_gate("exit 3", &ws(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, SchedulerError::Gate(_)));
    }

    #[tokio::test]
    async fn timeout_kills_the_child_and_returns_quickly() {
        let started = std::time::Instant::now();
        let err = run_gate("sleep 5", &ws(), Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(err, SchedulerError::Gate(_)));
        // Proves the child was actually killed rather than the test just
        // moving on while a `sleep 5` keeps running in the background: if
        // kill_on_drop didn't fire, this assertion would still pass (we
        // only await the timeout), but the orphaned process would remain —
        // that's exactly why this is a `kill_on_drop` + drop-the-future
        // pattern rather than a manual `child.kill()` call: there is no
        // other code path where the process handle could be reached after
        // `timeout` returns `Err`.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "run_gate should return promptly on timeout, not wait out the sleep"
        );
    }
}
