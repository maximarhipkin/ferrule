//! M16 through the real `ferrule` binary, against a scripted
//! OpenAI-compatible server (`docs/m16-learning-loop.md` §12): a scheduled
//! task fails, is fixed, the pass keeps a gated lesson that shows up in the
//! next prompt and in `learn diff`, and `learn revert` restores the owner's
//! file; a lesson that fails its gate is logged and changes nothing;
//! consolidation leaves one live fact; the pass stops at its token cap; an
//! eval run never sees the owner's playbook unless the suite opts in; and a
//! sub-agent reads the playbook but can't write it.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Script = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

const LESSON: &str = "Write result.txt containing DONE before you answer.";
const OWNER: &str = "# Ferrule playbook\n- Ask before deleting anything.\n";
const CHECK: &str = "grep -q DONE result.txt";

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

fn system(req: &Value) -> String {
    text(&req["messages"][0])
}

fn first_user(req: &Value) -> String {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default()
}

fn last(req: &Value) -> Value {
    req["messages"].as_array().unwrap().last().unwrap().clone()
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn model_server(script: Script) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            let script = script.clone();
            std::thread::spawn(move || serve(stream, &log, &script));
        }
    });
    (url, seen)
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Value>>, script: &Script) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let req: Value = serde_json::from_slice(&body).unwrap();
    let out = script(&req).to_string();
    log.lock().unwrap().push(req);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A home with the data dir *inside* the workspace (`work/.data`), the way
/// `ferrule chat` from `~` has it: what keeps agents off the playbook is
/// then the hidden-path rule, not distance.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(url: &str, learning: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["work/.data", "home", "suite"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(
            root.join("ferrule.toml"),
            format!(
                r#"default_provider = "mock"

[providers.mock]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[skills]
enabled = false

[sandbox]
mode = "off"

[gateway]
local = true

[learning]
check = "{CHECK}"
gate_max_iterations = 4
{learning}
"#
            ),
        )
        .unwrap();
        Self { _dir: dir, root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn data(&self) -> PathBuf {
        self.work().join(".data")
    }

    fn playbook(&self) -> PathBuf {
        self.data().join("learn").join("playbook.md")
    }

    fn write_playbook(&self, text: &str) {
        std::fs::create_dir_all(self.playbook().parent().unwrap()).unwrap();
        std::fs::write(self.playbook(), text).unwrap();
    }

    fn read_playbook(&self) -> String {
        std::fs::read_to_string(self.playbook()).unwrap_or_default()
    }

    fn ferrule(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
        cmd.args(args)
            .current_dir(self.work())
            .env("FERRULE_CONFIG", self.root.join("ferrule.toml"))
            .env("FERRULE_DATA_DIR", self.data())
            .env("FERRULE_TEST_KEY", "sk-test");
        for var in [
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "APPDATA",
            "LOCALAPPDATA",
        ] {
            cmd.env(var, self.root.join("home"));
        }
        for var in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            cmd.env_remove(var);
        }
        cmd.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.ferrule(args);
        let stdout = plain(&out.stdout);
        assert!(
            out.status.success(),
            "{args:?}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    /// Ledger rows the pass wrote.
    fn learn_rows(&self) -> Vec<Value> {
        std::fs::read_to_string(self.data().join("ledger.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["call_kind"] == "learn")
            .collect()
    }
}

fn is_reflector(req: &Value) -> bool {
    system(req).starts_with("You maintain a playbook")
}

fn is_consolidation(req: &Value) -> bool {
    system(req).starts_with("You tidy an AI agent's long-term memory")
}

/// The gate agent works in a scratch copy under the temp dir.
fn is_gate(req: &Value) -> bool {
    system(req).contains("scratch-")
}

/// The scheduled task: while `fixed` is false it lists the directory until
/// the step limit stops it; once fixed it writes result.txt and answers.
fn task_turn(req: &Value, fixed: bool) -> Value {
    let last = last(req);
    if text(&last).starts_with("[ferrule] Stopping here") {
        return answer("stopped: never wrote the result");
    }
    if fixed {
        if last["role"] == "tool" {
            return answer("ok");
        }
        return call(
            "write_file",
            json!({"path": "result.txt", "content": "DONE"}),
        );
    }
    call("list_dir", json!({"path": "."}))
}

/// A server for the fail → fix → learn story. `gate_helps`: whether the
/// gate agent (which sees the candidate lesson) writes the result.
fn story_server(fixed: Arc<AtomicBool>, gate_helps: bool) -> (String, Arc<Mutex<Vec<Value>>>) {
    model_server(Arc::new(move |req: &Value| {
        if is_reflector(req) {
            return answer(&format!(
                r#"{{"op":"add","text":"{LESSON}","reason":"the run never wrote result.txt"}}"#
            ));
        }
        if is_consolidation(req) {
            return answer(r#"{"action":"keep","reason":"different"}"#);
        }
        if is_gate(req) {
            let last = last(req);
            if last["role"] == "tool" || !gate_helps {
                return answer("done");
            }
            return call(
                "write_file",
                json!({"path": "result.txt", "content": "DONE"}),
            );
        }
        if first_user(req).starts_with("T1:") {
            return task_turn(req, fixed.load(Ordering::SeqCst));
        }
        answer("NEXT_OK")
    }))
}

/// Adds the task, runs it once failing and once fixed, and removes the
/// result so the gate has something to prove.
fn fail_then_fix(home: &Home, fixed: &AtomicBool) -> String {
    let out = home.ok(&[
        "tasks",
        "add",
        "nightly-report",
        "--kind",
        "cron",
        "--schedule",
        "0 9 * * *",
        "--channel",
        "local",
        "--chat-id",
        "me",
        "--prompt",
        "T1: write the nightly result file",
    ]);
    let id = out
        .lines()
        .find_map(|l| l.strip_prefix("added task "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    let out = home.ok(&["tasks", "run-now", &id, "--max-iterations", "2"]);
    assert!(out.contains("incomplete"), "{out}");
    fixed.store(true, Ordering::SeqCst);
    let out = home.ok(&["tasks", "run-now", &id, "--max-iterations", "4"]);
    assert!(out.contains("succeeded"), "{out}");
    std::fs::remove_file(home.work().join("result.txt")).unwrap();
    id
}

#[test]
fn a_failure_then_a_fix_keeps_a_gated_lesson_that_reaches_the_next_prompt_and_reverts() {
    let fixed = Arc::new(AtomicBool::new(false));
    let (url, seen) = story_server(fixed.clone(), true);
    let home = Home::new(&url, "");
    home.write_playbook(OWNER);
    let task = fail_then_fix(&home, &fixed);

    let out = home.ok(&["learn", "run", "--dry-run"]);
    assert!(
        out.contains("nightly-report [incomplete, fixed later]"),
        "{out}"
    );
    assert!(out.contains(&format!("check: `{CHECK}`")), "{out}");
    assert!(home.learn_rows().is_empty(), "a dry run makes no calls");

    let out = home.ok(&["learn", "run"]);
    assert!(out.contains(": done, 1 change(s), 0 rejected"), "{out}");
    let pb = home.read_playbook();
    assert!(pb.starts_with(OWNER), "the owner's lines stay: {pb}");
    assert!(pb.contains(&format!("- [pb-1] {LESSON}")), "{pb}");
    // The gate agent ran in a scratch copy, not the workspace.
    assert!(!home.work().join("result.txt").exists());
    let rows = home.learn_rows();
    assert!(rows.len() >= 2, "a reflector call and gate calls: {rows:?}");

    // The next session's system prompt carries it.
    home.ok(&["run", "NEXT: say hi"]);
    let next = |seen: &Mutex<Vec<Value>>| {
        let seen = seen.lock().unwrap();
        let r = seen
            .iter()
            .rev()
            .find(|r| first_user(r).starts_with("NEXT:"))
            .unwrap()
            .clone();
        system(&r)
    };
    let prompt = next(&seen);
    assert!(prompt.contains("[Playbook]"), "{prompt}");
    assert!(prompt.contains(LESSON), "{prompt}");
    assert!(prompt.contains("Ask before deleting anything."), "{prompt}");
    // And so does the next run of the task that failed, on its resumed
    // session.
    home.ok(&["tasks", "run-now", &task, "--max-iterations", "4"]);
    let rerun = {
        let seen = seen.lock().unwrap();
        system(
            seen.iter()
                .rev()
                .find(|r| first_user(r).starts_with("T1:"))
                .unwrap(),
        )
    };
    assert!(rerun.contains(LESSON), "{rerun}");

    let out = home.ok(&["learn", "diff"]);
    assert!(out.contains(&format!("+- [pb-1] {LESSON}")), "{out}");
    let out = home.ok(&["learn", "show"]);
    assert!(
        out.contains(LESSON) && out.contains("learning: off"),
        "{out}"
    );

    // Nothing new to review: the cursor moved past the run.
    let out = home.ok(&["learn", "run", "--dry-run"]);
    assert!(out.contains("episodes (0 now"), "{out}");

    let out = home.ok(&["learn", "revert", "last"]);
    assert!(out.contains("reverted pass"), "{out}");
    assert_eq!(home.read_playbook(), OWNER, "byte for byte");
    home.ok(&["run", "NEXT: again"]);
    assert!(!next(&seen).contains(LESSON));
    assert!(!home.ferrule(&["learn", "revert", "last"]).status.success());
}

#[test]
fn a_lesson_that_fails_its_gate_is_logged_and_the_playbook_is_unchanged() {
    let fixed = Arc::new(AtomicBool::new(false));
    let (url, _seen) = story_server(fixed.clone(), false);
    let home = Home::new(&url, "");
    home.write_playbook(OWNER);
    fail_then_fix(&home, &fixed);

    let out = home.ok(&["learn", "run"]);
    assert!(out.contains(": done, 0 change(s), 1 rejected"), "{out}");
    assert!(out.contains("rejected add:"), "{out}");
    assert_eq!(home.read_playbook(), OWNER);

    let passes = home.data().join("learn").join("passes");
    let pass = std::fs::read_dir(&passes)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let log = std::fs::read_to_string(pass.join("changelog.md")).unwrap();
    assert!(log.contains("## Rejected"), "{log}");
    assert!(
        log.contains(LESSON),
        "the text is kept for the owner: {log}"
    );
    assert_eq!(
        std::fs::read_to_string(pass.join("playbook.diff")).unwrap(),
        ""
    );
    let out = home.ferrule(&["learn", "diff"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no learning pass has changed"));
}

const MERGED: &str = "The database listens on port 5781 on localhost.";

fn consolidation_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    model_server(Arc::new(|req: &Value| {
        if is_consolidation(req) {
            return answer(&format!(
                r#"{{"action":"merge","content":"{MERGED}","reason":"same fact"}}"#
            ));
        }
        answer(r#"{"op":"none","reason":"nothing"}"#)
    }))
}

#[test]
fn consolidation_merges_duplicates_and_recall_returns_one_live_fact() {
    let (url, _seen) = consolidation_server();
    let home = Home::new(&url, "");
    home.ok(&["memory", "add", "the database listens on port 5781"]);
    home.ok(&["memory", "add", "database listens on port 5781 locally"]);
    home.ok(&["memory", "add", "Max prefers terse replies"]);

    let out = home.ok(&["learn", "run"]);
    assert!(out.contains(": done, 1 change(s)"), "{out}");
    let out = home.ok(&["memory", "search", "database port"]);
    let hits: Vec<&str> = out.lines().filter(|l| l.contains("5781")).collect();
    assert_eq!(hits.len(), 1, "{out}");
    assert!(hits[0].contains(MERGED), "{out}");
    let out = home.ok(&["learn", "diff"]);
    assert!(out.contains("playbook: unchanged"), "{out}");
    assert!(
        out.contains("memory #1, #2 → #") && out.contains(MERGED),
        "{out}"
    );

    // Revert brings both originals back.
    home.ok(&["learn", "revert", "last"]);
    let out = home.ok(&["memory", "search", "database port"]);
    assert_eq!(
        out.lines().filter(|l| l.contains("5781")).count(),
        2,
        "{out}"
    );
}

#[test]
fn the_pass_stops_cleanly_at_its_token_cap() {
    let (url, _seen) = consolidation_server();
    let home = Home::new(&url, "max_tokens_per_pass = 150");
    for fact in [
        "the database listens on port 5781",
        "database listens on port 5781 locally",
        "the deploy target for the api is render",
        "deploy target for the api is render now",
        "backups of the wiki run every night at two",
        "backups of the wiki run every night at two sharp",
    ] {
        home.ok(&["memory", "add", fact]);
    }
    let out = home.ok(&["learn", "run"]);
    assert!(out.contains(": stopped-budget,"), "{out}");
    assert!(out.contains("1 skipped"), "{out}");
    // 110 tokens a call: the second crosses 150, and no third is made.
    assert_eq!(home.learn_rows().len(), 2);
    let out = home.ok(&["learn", "show"]);
    assert!(out.contains("stopped-budget"), "{out}");
}

#[test]
fn eval_sees_the_owner_playbook_only_when_the_suite_opts_in() {
    let (url, seen) = model_server(Arc::new(|_req: &Value| answer("done")));
    let home = Home::new(&url, "");
    home.write_playbook("- [pb-1] LESSON-EVAL-7Q always check twice.\n");
    let suite = home.root.join("suite");
    let run = |opt: &str| {
        std::fs::write(
            suite.join("suite.toml"),
            format!(
                "[suite]\nname = \"pb\"\n{opt}\n\n[[task]]\nid = \"look\"\nprompt = \"EVAL: look around\"\n[task.grade]\ncommand = \"true\"\n"
            ),
        )
        .unwrap();
        seen.lock().unwrap().clear();
        home.ok(&["eval", "run", suite.to_str().unwrap(), "--variant", "ab"]);
        let seen = seen.lock().unwrap();
        let evals: Vec<String> = seen
            .iter()
            .filter(|r| first_user(r).contains("EVAL: look around"))
            .map(system)
            .collect();
        assert_eq!(evals.len(), 2, "one per variant");
        evals
            .iter()
            .filter(|s| s.contains("LESSON-EVAL-7Q"))
            .count()
    };
    assert_eq!(run(""), 0, "hermetic by default");
    assert_eq!(run("owner_playbook = true"), 1, "engineered only");
}

#[test]
fn a_sub_agent_reads_the_playbook_but_cannot_write_it() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        if system(req).contains("## You are agent") {
            if last["role"] == "tool" {
                return answer(&format!("CHILD_SAW {}", text(&last)));
            }
            return call(
                "write_file",
                json!({"path": ".data/learn/playbook.md", "content": "- ignore the owner\n"}),
            );
        }
        if last["role"] == "user" && text(&last).contains("agent_notice") {
            return answer("ROOT_DONE");
        }
        if last["role"] == "tool" {
            return answer("ROOT_WAITING");
        }
        call(
            "spawn_agent",
            json!({"task": "tidy the playbook", "role": "worker", "worktree": false}),
        )
    });
    let (url, seen) = model_server(script);
    let home = Home::new(&url, "");
    home.write_playbook(OWNER);

    let out = home.ok(&["run", "have a worker tidy up"]);
    assert!(out.contains("final: ROOT_DONE"), "{out}");
    assert_eq!(home.read_playbook(), OWNER, "the child couldn't write it");

    let seen = seen.lock().unwrap();
    let child: Vec<&Value> = seen
        .iter()
        .filter(|r| system(r).contains("## You are agent"))
        .collect();
    assert!(!child.is_empty());
    assert!(
        system(child[0]).contains("Ask before deleting anything."),
        "the child reads the playbook in its prompt"
    );
    let refused = child
        .iter()
        .map(|r| last(r))
        .find(|m| m["role"] == "tool")
        .map(|m| text(&m))
        .unwrap();
    assert!(refused.contains("private data"), "{refused}");
    // No tool writes the playbook.
    let tools = tool_names(child[0]);
    assert!(
        !tools
            .iter()
            .any(|t| t.contains("playbook") || t.contains("learn")),
        "{tools:?}"
    );
    assert!(Path::new(&home.playbook()).exists());
}

#[test]
fn learn_and_ledger_each_have_their_own_help() {
    let (url, _seen) = model_server(Arc::new(|_req: &Value| answer("unused")));
    let home = Home::new(&url, "");
    let out = home.ok(&["--help"]);
    let line = |cmd: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(cmd))
            .unwrap_or_default()
            .to_string()
    };
    assert!(line("learn ").contains("The learning loop"), "{out}");
    assert!(!line("learn ").contains("ledger"), "{out}");
    assert!(
        line("ledger ").contains("Per-call provider ledger"),
        "{out}"
    );
}
