//! The daemon ferrule starts itself (`[gateway.signal]` without `url`):
//! signal-cli on 127.0.0.1, its output appended to a log, restarted when it
//! exits. One already answering on the port is used as it is.

use super::{Daemon, SignalChannel};
use serde_json::json;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Failures in a row before the dashboard says so.
const FAILS_TO_SAY: u32 = 3;
/// How often a daemon ferrule didn't start is checked on.
const ADOPTED_CHECK: Duration = Duration::from_secs(30);

impl SignalChannel {
    /// Keeps the daemon up; never returns.
    pub(super) async fn supervise(&self, d: &Daemon) {
        let mut fails = 0u32;
        loop {
            if self.rpc_once("version", json!({})).await.is_ok() {
                *self.daemon_problem.lock().unwrap() = None;
                fails = 0;
                tracing::info!(
                    "signal: a daemon already answers on port {}; using it",
                    d.port
                );
                loop {
                    tokio::time::sleep(ADOPTED_CHECK.min(self.timing.ping * 5)).await;
                    if self.rpc_once("version", json!({})).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            let started = Instant::now();
            let outcome = self.run_daemon(d).await;
            match outcome {
                Err(why) => {
                    tracing::warn!("signal: {why}");
                    *self.daemon_problem.lock().unwrap() = Some(why);
                    tokio::time::sleep(self.timing.backoff_max).await;
                    continue;
                }
                Ok(status) => {
                    if started.elapsed() >= self.timing.healthy {
                        fails = 1;
                    } else {
                        fails += 1;
                    }
                    tracing::warn!("signal: signal-cli exited ({status}); starting it again");
                    if fails >= FAILS_TO_SAY {
                        let log = d
                            .log
                            .as_ref()
                            .map(|l| format!("; see {}", l.display()))
                            .unwrap_or_default();
                        *self.daemon_problem.lock().unwrap() = Some(format!(
                            "signal-cli exited {fails} times in a row ({status}){log}"
                        ));
                    }
                }
            }
            let wait = self
                .timing
                .backoff_min
                .saturating_mul(1 << fails.min(6))
                .min(self.timing.backoff_max);
            tokio::time::sleep(wait).await;
        }
    }

    /// Starts signal-cli and waits for it to exit: how it did, or why it
    /// couldn't start.
    async fn run_daemon(&self, d: &Daemon) -> Result<String, String> {
        let args = d.args(&self.cfg.account);
        let mut cmd = tokio::process::Command::new(&d.program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true);
        match d.log.as_ref().and_then(|p| open_log(p, &d.program, &args)) {
            Some(f) => cmd.stderr(f),
            None => cmd.stderr(Stdio::null()),
        };
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "couldn't start {}: {e}; install signal-cli (https://github.com/AsamK/signal-cli) or set [gateway.signal] signal_cli to its path",
                d.program.display()
            )
        })?;
        // Up long enough: it started well, whatever the earlier tries did.
        let healthy = tokio::time::sleep(self.timing.healthy);
        tokio::pin!(healthy);
        let mut cleared = false;
        loop {
            tokio::select! {
                s = child.wait() => {
                    return Ok(match s {
                        Ok(s) => s.to_string(),
                        Err(e) => e.to_string(),
                    });
                }
                _ = &mut healthy, if !cleared => {
                    cleared = true;
                    *self.daemon_problem.lock().unwrap() = None;
                }
            }
        }
    }
}

/// The log, appended to, with a line saying who started what.
fn open_log(
    path: &std::path::Path,
    program: &std::path::Path,
    args: &[String],
) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let _ = writeln!(
        f,
        "--- {} ferrule starts {} {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        program.display(),
        args.join(" ")
    );
    Some(f)
}
