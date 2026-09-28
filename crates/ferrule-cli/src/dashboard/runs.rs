//! M37 §1.5/§4.2: a `ferrule` command run from the page, as a child
//! process. The page polls its output (the HTTP server answers one request
//! per connection, so there's no stream): `from` is how many bytes it has.
//! Output is capped, a run is stopped after [`TIMEOUT`], stdin is closed so
//! a prompt fails instead of hanging, and colours are off.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncReadExt;

/// The most output a run keeps; past it the rest is dropped, saying so.
pub const MAX_OUTPUT: usize = 256 * 1024;
pub const TIMEOUT: Duration = Duration::from_secs(600);
/// Finished runs kept for the page to read.
const KEEP: usize = 20;

pub struct Run {
    pub id: String,
    /// What the page shows for it: `ferrule doctor --json`.
    pub label: String,
    pub started: SystemTime,
    state: Mutex<State>,
    cancel: tokio::sync::Notify,
}

#[derive(Default)]
struct State {
    out: String,
    truncated: bool,
    /// The exit code once it ended; `-1` when it was stopped or timed out.
    code: Option<i32>,
    why: Option<String>,
}

impl Run {
    fn push(&self, text: &str) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.truncated {
            return;
        }
        let room = MAX_OUTPUT.saturating_sub(s.out.len());
        if text.len() <= room {
            s.out.push_str(text);
        } else {
            let mut cut = room;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            s.out.push_str(&text[..cut]);
            s.out.push_str("\n[output cut at 256 KB]\n");
            s.truncated = true;
        }
    }

    fn end(&self, code: i32, why: Option<String>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.code = Some(code);
        s.why = why;
    }

    /// The exit code once it has ended.
    pub fn code(&self) -> Option<i32> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).code
    }

    pub fn done(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .code
            .is_some()
    }

    /// Everything so far, for a caller that reads the whole output.
    pub fn output(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .out
            .clone()
    }

    /// The output after byte `from`, and where the next read starts.
    pub fn view(&self, from: usize) -> Value {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut from = from.min(s.out.len());
        while !s.out.is_char_boundary(from) {
            from -= 1;
        }
        json!({
            "id": self.id,
            "label": self.label,
            "text": &s.out[from..],
            "next": s.out.len(),
            "done": s.code.is_some(),
            "code": s.code,
            "why": s.why,
            "secs": self.started.elapsed().map_or(0, |d| d.as_secs()),
        })
    }
}

/// The runs this process started from the page.
pub struct Runs {
    program: PathBuf,
    list: Mutex<VecDeque<Arc<Run>>>,
}

impl Default for Runs {
    fn default() -> Self {
        Self::new(std::env::current_exe().unwrap_or_else(|_| "ferrule".into()))
    }
}

impl Runs {
    /// Runs of `program` (the tests' stand-in for `ferrule`).
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            list: Mutex::new(VecDeque::new()),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Run>> {
        let list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.iter().find(|r| r.id == id).cloned()
    }

    /// Every run, newest first, without its output.
    pub fn list(&self) -> Vec<Value> {
        let list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.iter()
            .rev()
            .map(|r| {
                let s = r.state.lock().unwrap_or_else(|e| e.into_inner());
                json!({ "id": r.id, "label": r.label, "done": s.code.is_some(), "code": s.code })
            })
            .collect()
    }

    /// Starts `program args…` with `env` added; the run is readable at once.
    pub fn start(
        &self,
        label: String,
        args: Vec<OsString>,
        env: Vec<(String, OsString)>,
    ) -> std::io::Result<Arc<Run>> {
        self.start_then(label, args, env, |_| {})
    }

    /// A run that's over before it starts: `--help`, or a line clap
    /// refused, answered in-process.
    pub fn finished(&self, label: String, text: &str, code: i32) -> Arc<Run> {
        let run = Arc::new(Run {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            label,
            started: SystemTime::now(),
            state: Mutex::default(),
            cancel: tokio::sync::Notify::new(),
        });
        run.push(text);
        run.end(code, None);
        self.keep(run.clone());
        run
    }

    /// Whether a run is still going.
    pub fn busy(&self) -> bool {
        let list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        list.iter().any(|r| !r.done())
    }

    fn keep(&self, run: Arc<Run>) {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        while list.len() >= KEEP {
            match list.iter().position(|r| r.done()) {
                Some(at) => drop(list.remove(at)),
                None => break,
            }
        }
        list.push_back(run);
    }

    /// [`Runs::start`], and `after` once it has ended (the audit line).
    pub fn start_then(
        &self,
        label: String,
        args: Vec<OsString>,
        env: Vec<(String, OsString)>,
        after: impl FnOnce(&Run) + Send + 'static,
    ) -> std::io::Result<Arc<Run>> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&args)
            .envs(env)
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("FERRULE_FROM_DASHBOARD", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let run = Arc::new(Run {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            label,
            started: SystemTime::now(),
            state: Mutex::default(),
            cancel: tokio::sync::Notify::new(),
        });
        self.keep(run.clone());
        let readers: Vec<_> = [
            child.stdout.take().map(|o| Box::new(o) as Box<dyn Reader>),
            child.stderr.take().map(|e| Box::new(e) as Box<dyn Reader>),
        ]
        .into_iter()
        .flatten()
        .map(|mut pipe| {
            let run = run.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut carry = Vec::new();
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    carry.extend_from_slice(&buf[..n]);
                    // Only whole characters: a split one waits for its rest.
                    let upto = match std::str::from_utf8(&carry) {
                        Ok(_) => carry.len(),
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => carry.len(),
                    };
                    run.push(&String::from_utf8_lossy(&carry[..upto]));
                    carry.drain(..upto);
                }
                if !carry.is_empty() {
                    run.push(&String::from_utf8_lossy(&carry));
                }
            })
        })
        .collect();
        let watched = run.clone();
        tokio::spawn(async move {
            let run = watched;
            let (code, why) = tokio::select! {
                status = child.wait() => match status {
                    Ok(s) => (s.code().unwrap_or(-1), None),
                    Err(e) => (-1, Some(format!("couldn't wait for it: {e}"))),
                },
                _ = run.cancel.notified() => {
                    let _ = child.kill().await;
                    (-1, Some("stopped from the page".to_string()))
                }
                _ = tokio::time::sleep(TIMEOUT) => {
                    let _ = child.kill().await;
                    (-1, Some(format!("stopped after {} minutes", TIMEOUT.as_secs() / 60)))
                }
            };
            for r in readers {
                let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
            }
            run.end(code, why);
            after(&run);
        });
        Ok(run)
    }

    /// Stops a run that's still going; false when there's none.
    pub fn cancel(&self, id: &str) -> bool {
        match self.get(id) {
            Some(run) if !run.done() => {
                run.cancel.notify_one();
                true
            }
            _ => false,
        }
    }
}

trait Reader: tokio::io::AsyncRead + Unpin + Send {}
impl<T: tokio::io::AsyncRead + Unpin + Send> Reader for T {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn finished(run: &Run) -> Value {
        for _ in 0..400 {
            if run.done() {
                return run.view(0);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never finished: {}", run.view(0));
    }

    fn sh() -> Runs {
        Runs::new("/bin/sh".into())
    }

    #[tokio::test]
    async fn output_is_read_in_pieces_and_the_exit_code_kept() {
        let runs = sh();
        let run = runs
            .start(
                "t".into(),
                vec!["-c".into(), "echo one; echo \"$X\" >&2; exit 3".into()],
                vec![("X".into(), "two".into())],
            )
            .unwrap();
        let v = finished(&run).await;
        assert_eq!(v["code"], 3);
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("one") && text.contains("two"), "{text}");
        let next = v["next"].as_u64().unwrap() as usize;
        assert_eq!(run.view(next)["text"], "");
        assert_eq!(run.view(4)["text"].as_str().unwrap().len(), next - 4);
        assert_eq!(runs.list()[0]["code"], 3);
    }

    #[tokio::test]
    async fn a_prompt_gets_no_input_and_colours_are_off() {
        let runs = sh();
        let run = runs
            .start(
                "t".into(),
                vec![
                    "-c".into(),
                    "if read x; then echo got; else echo eof; fi; echo $NO_COLOR".into(),
                ],
                vec![],
            )
            .unwrap();
        let v = finished(&run).await;
        assert_eq!(v["text"], "eof\n1\n");
    }

    #[tokio::test]
    async fn a_run_stops_from_the_page_and_output_is_capped() {
        let runs = sh();
        let run = runs
            .start("t".into(), vec!["-c".into(), "sleep 30".into()], vec![])
            .unwrap();
        assert!(runs.cancel(&run.id));
        let v = finished(&run).await;
        assert_eq!(v["code"], -1);
        assert_eq!(v["why"], "stopped from the page");
        assert!(!runs.cancel(&run.id), "already done");

        let big = runs
            .start(
                "t".into(),
                vec!["-c".into(), "head -c 400000 /dev/zero | tr '\\0' a".into()],
                vec![],
            )
            .unwrap();
        let v = finished(&big).await;
        let text = v["text"].as_str().unwrap();
        assert!(text.len() < MAX_OUTPUT + 100);
        assert!(text.ends_with("[output cut at 256 KB]\n"));
    }
}
