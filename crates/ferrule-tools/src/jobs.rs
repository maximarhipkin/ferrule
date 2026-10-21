//! M57.6: commands the shell runs in the background (`shell {background:
//! true}`), and `shell_job` to look at them and stop them
//! (docs/m57-context-hygiene.md, "Background processes").
//!
//! A job runs in the same sandbox as any shell command, in its own process
//! group, with stdout and stderr going straight to a new file under
//! `.ferrule/spill/`. The table is the agent's (one per [`ShellTool`]): its
//! jobs are killed when the session ends ([`Jobs::stop_all`], from
//! `Tool::session_ended`) or the table is dropped with its agent.
//!
//! [`ShellTool`]: crate::shell::ShellTool

use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Jobs one agent may have running at once.
pub const MAX_RUNNING: usize = 4;
/// Finished jobs kept for `status` and `tail`; older ones are forgotten
/// (their files stay).
const KEEP_FINISHED: usize = 16;
/// Lines `tail` returns when not told, and the most it returns.
const TAIL_LINES: usize = 40;
const TAIL_MAX: usize = 400;
/// How far back from the end of a job's file `tail` reads.
const TAIL_BYTES: u64 = 256 * 1024;

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "shell_job".into(),
        message: message.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The exit code (-1: ended by a signal).
    Exited(i32),
    Killed,
}

/// One background command.
pub struct Job {
    id: u32,
    command: String,
    /// Workspace-relative, as the model sees it.
    log: String,
    path: PathBuf,
    /// The file the job was started with ([`identity`]).
    made: Option<(u64, u64)>,
    started: Instant,
    /// The process (and, on Unix, its group's) id.
    pid: Option<u32>,
    killed: AtomicBool,
    end: Mutex<Option<(End, Duration)>>,
    stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Job {
    fn running(&self) -> bool {
        self.end.lock().unwrap().is_none()
    }

    /// Kills the job's process group (Unix) and asks its watcher to kill
    /// the process itself and record the end.
    fn kill(&self) {
        if !self.running() {
            return;
        }
        self.killed.store(true, Ordering::Relaxed);
        if let Some(pid) = self.pid {
            ferrule_sandbox::kill_process_group(pid);
        }
        if let Some(stop) = self.stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
    }

    fn finish(&self, code: Option<i32>) {
        let end = if self.killed.load(Ordering::Relaxed) {
            End::Killed
        } else {
            End::Exited(code.unwrap_or(-1))
        };
        *self.end.lock().unwrap() = Some((end, self.started.elapsed()));
    }

    /// `job 2 (`npm run dev`): running for 1m 05s, 4210 bytes of output in …`.
    fn line(&self) -> String {
        // Same rule as `tail`: a log swapped for a link (or any other file)
        // must not leak that file's size.
        let size = match std::fs::symlink_metadata(&self.path) {
            Ok(m) if !m.is_symlink() && self.is_ours(&m).is_ok() => {
                format!("{} bytes of output", m.len())
            }
            _ => "output unavailable (the log isn't the job's own file)".to_string(),
        };
        let state = match *self.end.lock().unwrap() {
            None => format!("running for {}", human(self.started.elapsed())),
            Some((End::Exited(code), took)) => {
                format!("exited with code {code} after {}", human(took))
            }
            Some((End::Killed, took)) => format!("killed after {}", human(took)),
        };
        format!(
            "job {} (`{}`): {state}, {size} in {}",
            self.id, self.command, self.log
        )
    }

    /// Whether `meta` describes the file the job was started with.
    fn is_ours(&self, meta: &std::fs::Metadata) -> std::io::Result<()> {
        if !meta.is_file() || identity(meta) != self.made {
            return Err(std::io::Error::other(
                "it isn't the file the job was started with",
            ));
        }
        Ok(())
    }

    /// The last `n` lines of the job's output.
    fn tail(&self, n: usize) -> String {
        let read = || -> std::io::Result<String> {
            // This reads in ferrule's own process, and a sandboxed command
            // can swap the log for a link to a file it can't read itself
            // (the keys): only the file the job was started with is read.
            if std::fs::symlink_metadata(&self.path)?.is_symlink() {
                return Err(std::io::Error::other("it was replaced by a link"));
            }
            let mut f = std::fs::File::open(&self.path)?;
            let meta = f.metadata()?;
            self.is_ours(&meta)?;
            let len = meta.len();
            let from = len.saturating_sub(TAIL_BYTES);
            f.seek(SeekFrom::Start(from))?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        };
        match read() {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(n)..].join("\n")
            }
            Err(e) => format!("[could not read {}: {e}]", self.log),
        }
    }
}

/// A file's device and inode, so `tail` reads only the log it made (Unix;
/// elsewhere `None`, and only a link is refused).
pub(crate) fn identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// `1h 02m`, `3m 05s`, `12s`.
fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

/// One agent's background jobs.
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Arc<Job>>>,
    next: AtomicU32,
}

impl Jobs {
    pub fn running(&self) -> usize {
        self.list
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.running())
            .count()
    }

    fn get(&self, id: u32) -> Option<Arc<Job>> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// Starts watching `child`, already spawned with its output going to
    /// `path` (`log` as shown; `made`: its [`identity`]), and returns the
    /// new job's id.
    pub(crate) fn add(
        &self,
        command: &str,
        log: String,
        path: PathBuf,
        made: Option<(u64, u64)>,
        mut child: tokio::process::Child,
    ) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let job = Arc::new(Job {
            id,
            command: command.to_string(),
            log,
            path,
            made,
            started: Instant::now(),
            pid: child.id(),
            killed: AtomicBool::new(false),
            end: Mutex::new(None),
            stop: Mutex::new(Some(tx)),
        });
        let watched = job.clone();
        tokio::spawn(async move {
            let code = tokio::select! {
                status = child.wait() => status.ok().and_then(|s| s.code()),
                _ = rx => {
                    // The group is gone on Unix; this ends the process
                    // itself (on Windows, the sandbox's job object takes
                    // the tree with it).
                    let _ = child.start_kill();
                    child.wait().await.ok().and_then(|s| s.code())
                }
            };
            watched.finish(code);
        });
        let mut list = self.list.lock().unwrap();
        list.push(job);
        // Forget the oldest finished jobs past the limit.
        let finished = list.iter().filter(|j| !j.running()).count();
        let mut excess = finished.saturating_sub(KEEP_FINISHED);
        list.retain(|j| {
            if excess > 0 && !j.running() {
                excess -= 1;
                return false;
            }
            true
        });
        id
    }

    /// Kills every job still running.
    pub fn stop_all(&self) {
        for job in self.list.lock().unwrap().iter() {
            job.kill();
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// `shell_job`: the background jobs' status and output, and a way to stop
/// one. Made by [`crate::ShellTool::job_tool`], sharing its table.
pub struct ShellJobTool {
    pub(crate) jobs: Arc<Jobs>,
}

#[async_trait::async_trait]
impl Tool for ShellJobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell_job".into(),
            description: format!(
                "Check on or stop a command started with shell {{\"background\": true}}. \
                 status: how each job (or the one `id`) is doing; tail: the last lines of its output; \
                 kill: stop it and everything it started. At most {MAX_RUNNING} jobs run at once, and \
                 all of them are stopped when this session ends."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The job id shell returned. Leave it out for the status of every job." },
                    "action": { "type": "string", "enum": ["status", "tail", "kill"] },
                    "lines": { "type": "integer", "description": format!("For tail: how many lines (default {TAIL_LINES}, at most {TAIL_MAX}).") }
                },
                "required": ["action"]
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    fn deferrable(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let action = args["action"].as_str().unwrap_or("status");
        let id = args["id"]
            .as_u64()
            .or_else(|| args["id"].as_str().and_then(|s| s.trim().parse().ok()))
            .map(|n| n as u32);
        let job = match id {
            Some(id) => Some(self.jobs.get(id).ok_or_else(|| {
                failed(format!(
                    "no job {id}; shell_job {{\"action\": \"status\"}} lists them"
                ))
            })?),
            None => None,
        };
        match (action, job) {
            ("status", Some(job)) => Ok(ToolOutput::ok(job.line())),
            ("status", None) => {
                let list = self.jobs.list.lock().unwrap();
                if list.is_empty() {
                    return Ok(ToolOutput::ok("no background jobs in this session"));
                }
                let lines: Vec<String> = list.iter().map(|j| j.line()).collect();
                Ok(ToolOutput::ok(lines.join("\n")))
            }
            ("tail", Some(job)) => {
                let n = args["lines"]
                    .as_u64()
                    .map_or(TAIL_LINES, |n| (n as usize).clamp(1, TAIL_MAX));
                let text = format!("{}\n--- last lines ---\n{}", job.line(), job.tail(n));
                // The end is what matters: keep the status and the last
                // characters when it's over the cap.
                let max = ctx.max_output_chars;
                let count = text.chars().count();
                if count <= max {
                    return Ok(ToolOutput::ok(text));
                }
                let rest: String = text.chars().skip(count - max).collect();
                Ok(ToolOutput::ok(format!(
                    "{}\n[... earlier lines cut]\n{rest}",
                    job.line()
                )))
            }
            ("kill", Some(job)) => {
                if !job.running() {
                    return Ok(ToolOutput::ok(format!("{}; nothing to stop", job.line())));
                }
                job.kill();
                // Give the watcher a moment to record the end.
                for _ in 0..20 {
                    if !job.running() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok(ToolOutput::ok(job.line()))
            }
            ("tail" | "kill", None) => Err(failed(format!("`{action}` needs the job's `id`"))),
            (other, _) => Err(failed(format!(
                "unknown action {other:?}; use status, tail or kill"
            ))),
        }
    }
}
