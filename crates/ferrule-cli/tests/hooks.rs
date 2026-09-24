//! M18 through the real `ferrule` binary, against a scripted
//! OpenAI-compatible server, with hook scripts in a temp dir: the built-in
//! check as the Stop hook, a PreToolUse block, `additionalContext`,
//! workspace trust, a hung hook, a Stop hook that always blocks, a
//! sub-agent under its parent's hooks, eval's hermeticity, and the model
//! failing to turn a hook on. Unix only: the hooks are `sh` scripts.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Script = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

fn system(req: &Value) -> String {
    text(&req["messages"][0])
}

fn messages(req: &Value) -> Vec<Value> {
    req["messages"].as_array().unwrap().clone()
}

fn last(req: &Value) -> Value {
    messages(req).last().unwrap().clone()
}

fn is_child(req: &Value) -> bool {
    system(req).contains("## You are agent")
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

/// Serves `script` on a thread per connection; returns the base URL and
/// every request body it saw.
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

/// Output without its colours.
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

fn provider(url: &str) -> String {
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
"#
    )
}

/// A home whose config (the trusted one, by `FERRULE_CONFIG`) is the
/// provider plus `extra`. `extra` may say `{out}` for the dir hooks write
/// their evidence to.
fn home_with(url: &str, extra: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home", "out", "tmp"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    let extra = extra.replace("{out}", home.join("out").to_str().unwrap());
    std::fs::write(
        home.join("ferrule.toml"),
        format!("{}\n{extra}", provider(url)),
    )
    .unwrap();
    dir
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("TMPDIR", home.join("tmp"))
        .stdin(Stdio::null());
    // Nothing from the machine running the tests: its config, skills or proxy.
    for var in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(var, home.join("home"));
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
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

fn texts(out: &Output) -> (String, String) {
    (plain(&out.stdout), plain(&out.stderr))
}

fn run_ok(home: &Path, args: &[&str]) -> (String, String) {
    let out = ferrule(home, args);
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    (stdout, stderr)
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}\n{text}", path.display()))
}

/// The audit log's lines.
fn audit(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("data/hooks/runs.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn verify_command_is_the_built_in_stop_check_and_behaves_as_before() {
    // The model changes a file and finishes; the check fails, so it gets
    // the output and fixes it; the check passes and the run ends.
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let said = text(&last);
        if last["role"] == "user" && said.contains("fails, so this isn't done yet") {
            return call("write_file", json!({"path": "fixed.txt", "content": "ok"}));
        }
        if last["role"] == "tool" {
            return answer("DONE");
        }
        call("write_file", json!({"path": "a.txt", "content": "draft"}))
    });
    let (url, seen) = model_server(script);
    let dir = home_with(
        &url,
        "[agent]\nverify_command = \"test -f fixed.txt || { echo 'fixed.txt is missing'; exit 1; }\"\n",
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "make the change"]);
    assert!(out.contains("final: DONE"), "{out}");
    assert!(out.contains("✗ check"), "{out}");
    assert!(out.contains("✓ check"), "{out}");

    let seen = seen.lock().unwrap();
    // write, finish (check fails), fix, finish (check passes): four calls.
    assert_eq!(seen.len(), 4, "{seen:#?}");
    let sent_back = text(&last(&seen[2]));
    assert!(
        sent_back.starts_with("[ferrule] `test -f fixed.txt || { echo 'fixed.txt is missing'; exit 1; }` fails, so this isn't done yet."),
        "{sent_back}"
    );
    assert!(sent_back.contains("fixed.txt is missing"), "{sent_back}");
    // The check is a hook: `hooks list` shows it as the built-in Stop hook.
    let (list, _) = run_ok(home, &["hooks", "list"]);
    assert!(list.contains("Stop") && list.contains("builtin"), "{list}");
    assert!(list.contains("test -f fixed.txt"), "{list}");
}

#[test]
fn a_pre_tool_use_hook_that_exits_2_blocks_the_call_and_the_model_sees_why() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        if last["role"] == "tool" {
            return answer(&format!("SAW {}", text(&last)));
        }
        call("write_file", json!({"path": "x.txt", "content": "hi"}))
    });
    let (url, seen) = model_server(script);
    let dir = home_with(
        &url,
        r#"[[hooks.PreToolUse]]
matcher = "write_*"
command = "cat > {out}/payload.json; echo 'no writes in this repo' >&2; exit 2"
"#,
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "write x"]);
    assert!(
        out.contains(
            "final: SAW error: not run: a PreToolUse hook blocked it: no writes in this repo"
        ),
        "{out}"
    );
    assert!(!home.join("work/x.txt").exists(), "the tool ran");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let result = last(&seen[1]);
    assert_eq!(result["role"], "tool");
    assert_eq!(
        text(&result),
        "error: not run: a PreToolUse hook blocked it: no writes in this repo"
    );
    // Claude Code's payload on stdin.
    let payload = read_json(&home.join("out/payload.json"));
    assert_eq!(payload["hook_event_name"], "PreToolUse");
    assert_eq!(payload["tool_name"], "write_file");
    assert_eq!(payload["tool_input"]["path"], "x.txt");
    assert!(payload["session_id"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    // The audit log has it, and `hooks list` shows the run.
    let runs = audit(home);
    let run = runs
        .iter()
        .find(|r| r["event"] == "PreToolUse")
        .expect("audited");
    assert_eq!(run["blocked"], true);
    assert_eq!(run["exit_code"], 2);
    assert!(run["command"].as_str().unwrap().contains("no writes"));
    assert!(run["duration_ms"].is_u64());
    let (list, _) = run_ok(home, &["hooks", "list"]);
    assert!(list.contains("user") && list.contains("write_*"), "{list}");
    assert!(list.contains("blocked"), "{list}");
}

#[test]
fn additional_context_reaches_the_next_request_after_the_cached_prefix() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        if last["role"] == "tool" {
            return answer("DONE");
        }
        call("list_dir", json!({"path": "."}))
    });
    let (url, seen) = model_server(script);
    let dir = home_with(
        &url,
        r#"[[hooks.UserPromptSubmit]]
command = "echo '{\"additionalContext\":\"the user is on call\"}'"
[[hooks.PostToolUse]]
matcher = "list_dir"
command = "cat > {out}/post.json; echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"lint clean\"}}'"
"#,
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "look around"]);
    assert!(out.contains("final: DONE"), "{out}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // The prompt's note is a user message after the prompt, never in the
    // system prompt.
    let first = messages(&seen[0]);
    assert_eq!(
        text(first.last().unwrap()),
        "[hook: UserPromptSubmit]\nthe user is on call"
    );
    assert!(!system(&seen[0]).contains("on call"));
    // The tool's note is appended to its result.
    let result = last(&seen[1]);
    assert_eq!(result["role"], "tool");
    assert!(
        text(&result).ends_with("\n\n[hook: PostToolUse] lint clean"),
        "{result}"
    );
    // The second request starts with the first one, byte for byte.
    let second = messages(&seen[1]);
    assert_eq!(&second[..first.len()], &first[..]);
    let post = read_json(&home.join("out/post.json"));
    assert_eq!(post["tool_name"], "list_dir");
    assert!(post["tool_response"].is_object() || post["tool_response"].is_string());
}

#[test]
fn an_untrusted_workspace_hook_does_not_run_and_the_owner_is_told_until_trusted() {
    let script: Script = Arc::new(|req: &Value| {
        if last(req)["role"] == "tool" {
            return answer("DONE");
        }
        call("list_dir", json!({"path": "."}))
    });
    let (url, _) = model_server(script);
    let dir = home_with(&url, "[hooks]\nproject = true\n");
    let home = dir.path();
    let work = home.join("work");
    std::fs::create_dir_all(work.join(".ferrule")).unwrap();
    let marker = home.join("out/ran");
    let hooks = format!(
        "[[PreToolUse]]\ncommand = \"touch {} && test \\\"$CLAUDE_PROJECT_DIR\\\" = \\\"$PWD\\\"\"\n",
        marker.display()
    );
    std::fs::write(work.join(".ferrule/hooks.toml"), &hooks).unwrap();

    let (_, err) = run_ok(home, &["run", "go"]);
    assert!(
        err.contains("has 1 hook that won't run: you haven't trusted it yet. Run `ferrule hooks trust` if you trust them."),
        "{err}"
    );
    assert!(!marker.exists(), "an untrusted hook ran");
    let (list, _) = run_ok(home, &["hooks", "list"]);
    assert!(list.contains("workspace (won't run)"), "{list}");

    // `ferrule hooks trust` wants the owner at a terminal.
    let out = ferrule(home, &["hooks", "trust"]);
    assert!(!out.status.success());
    assert!(texts(&out).1.contains("asks the owner at a terminal"));
    // The owner trusts it (as `hooks trust` does after the prompt).
    let n = ferrule_hooks::TrustStore::in_data_dir(&home.join("data"))
        .trust(&work.canonicalize().unwrap())
        .unwrap();
    assert_eq!(n, 1);
    let (_, err) = run_ok(home, &["run", "go"]);
    assert!(!err.contains("won't run"), "{err}");
    assert!(marker.exists(), "the trusted hook didn't run");
    let run = audit(home)
        .into_iter()
        .find(|r| r["source"] == "workspace")
        .expect("audited");
    assert_eq!(run["exit_code"], 0, "{run}");

    // Any edit takes the trust away again.
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(work.join(".ferrule/hooks.toml"), format!("{hooks}\n")).unwrap();
    let (_, err) = run_ok(home, &["run", "go"]);
    assert!(err.contains("it changed since you trusted it"), "{err}");
    assert!(!marker.exists(), "a changed hook ran");
}

#[test]
fn a_hanging_hook_is_killed_at_its_timeout_with_its_children_and_the_turn_goes_on() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        if last["role"] == "tool" {
            return answer(&format!("SAW {}", text(&last).len()));
        }
        call("list_dir", json!({"path": "."}))
    });
    let (url, seen) = model_server(script);
    let dir = home_with(
        &url,
        r#"[[hooks.PreToolUse]]
timeout_secs = 1
command = "(sleep 3; touch {out}/survived) & wait"
"#,
    );
    let home = dir.path();
    let started = Instant::now();
    let (out, err) = run_ok(home, &["run", "look"]);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the hook wasn't killed"
    );
    assert!(out.contains("final: SAW"), "{out}");
    // The tool ran: a hook that fails doesn't block, and the model isn't told.
    let seen = seen.lock().unwrap();
    let result = text(&last(&seen[1]));
    assert!(!result.contains("hook"), "{result}");
    // The owner is.
    assert!(err.contains("[hook PreToolUse"), "{err}");
    assert!(err.contains("timed out and was killed"), "{err}");
    let run = audit(home)
        .into_iter()
        .find(|r| r["event"] == "PreToolUse")
        .unwrap();
    assert_eq!(run["blocked"], false);
    assert!(run["exit_code"].is_null(), "{run}");
    // Its child went with it: had it survived the kill, it would have
    // left a marker by now. (Not `kill -0`: an orphan's zombie may linger
    // under an init that doesn't reap.)
    std::thread::sleep(Duration::from_secs(4).saturating_sub(started.elapsed()));
    assert!(
        !home.join("out/survived").exists(),
        "the hook's child outlived it"
    );
}

#[test]
fn a_stop_hook_that_always_blocks_cannot_loop_forever() {
    let script: Script = Arc::new(|req: &Value| {
        let said = text(&last(req));
        if said.starts_with("[ferrule] Stopping here") {
            return answer("STATUS: stopped, the Stop hook keeps blocking");
        }
        answer("DONE")
    });
    let (url, seen) = model_server(script);
    let dir = home_with(
        &url,
        r#"[[hooks.Stop]]
command = "cat > {out}/stop-$(date +%s%N).json; echo 'write the changelog first' >&2; exit 2"
"#,
    );
    let home = dir.path();
    let out = ferrule(home, &["run", "finish up"]);
    let (stdout, stderr) = texts(&out);
    // Incomplete: exit 2, with the status answer.
    assert_eq!(out.status.code(), Some(2), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("incomplete (the Stop hook `cat > "),
        "{stdout}"
    );
    assert!(stdout.contains("still blocks after 3 tries"), "{stdout}");
    let seen = seen.lock().unwrap();
    // The first finish, three sent back, and the status answer.
    assert_eq!(seen.len(), 5, "{seen:#?}");
    assert_eq!(
        text(&last(&seen[1])),
        "[hook: Stop] This isn't done yet:\n\nwrite the changelog first"
    );
    // Four blocks (the fourth past the cap), `stop_hook_active` on all but the first.
    let mut payloads: Vec<Value> = std::fs::read_dir(home.join("out"))
        .unwrap()
        .map(|e| read_json(&e.unwrap().path()))
        .collect();
    assert_eq!(payloads.len(), 4);
    let active = payloads
        .iter_mut()
        .filter(|p| p["stop_hook_active"] == true)
        .count();
    assert_eq!(active, 3);
    assert!(payloads
        .iter()
        .all(|p| p["last_assistant_message"] == "DONE"));
}

/// What a sub-agent test's model does: the root spawns a worker, the
/// worker lists the directory and reports what it saw, the root finishes
/// on the notice.
fn spawning_script() -> Script {
    Arc::new(|req: &Value| {
        let last = last(req);
        if is_child(req) {
            if last["role"] == "tool" {
                return answer(&format!("CHILD_SAW {}", text(&last)));
            }
            if text(&last).starts_with("[hook: SubagentStop]") {
                return answer("CHILD_AGAIN");
            }
            return call("list_dir", json!({"path": "."}));
        }
        if last["role"] == "user" && text(&last).contains("agent_notice") {
            return answer(&format!("ROOT_DONE {}", text(&last)));
        }
        if last["role"] == "tool" {
            return answer("ROOT_WAITING");
        }
        call(
            "spawn_agent",
            json!({"task": "list the files", "role": "worker", "worktree": false}),
        )
    })
}

#[test]
fn a_sub_agents_tool_call_is_blocked_by_its_parents_pre_tool_use_hook() {
    let (url, seen) = model_server(spawning_script());
    let dir = home_with(
        &url,
        r#"[[hooks.PreToolUse]]
matcher = "list_dir"
command = "cat > {out}/pre.json; echo 'no listing here' >&2; exit 2"
[[hooks.SubagentStart]]
matcher = "worker"
command = "cat > {out}/start.json"
[[hooks.SubagentStop]]
command = "cat > {out}/stop.json"
"#,
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "have a worker list the files"]);
    assert!(out.contains("final: ROOT_DONE"), "{out}");
    assert!(
        out.contains("CHILD_SAW error: not run: a PreToolUse hook blocked it: no listing here"),
        "{out}"
    );
    let seen = seen.lock().unwrap();
    let child: Vec<&Value> = seen.iter().filter(|r| is_child(r)).collect();
    assert_eq!(child.len(), 2);
    assert_eq!(
        text(&last(child[1])),
        "error: not run: a PreToolUse hook blocked it: no listing here"
    );

    // The child's own payload names it and its parent's session.
    let pre = read_json(&home.join("out/pre.json"));
    let start = read_json(&home.join("out/start.json"));
    let stop = read_json(&home.join("out/stop.json"));
    let child_id = pre["agent_id"]
        .as_str()
        .expect("the child's id")
        .to_string();
    let root_session = start["session_id"].as_str().unwrap().to_string();
    assert_eq!(pre["parent_session_id"], root_session.as_str());
    assert_ne!(pre["session_id"], root_session.as_str());
    // SubagentStart/Stop fire in the parent's session, about the child.
    for (p, event) in [(&start, "SubagentStart"), (&stop, "SubagentStop")] {
        assert_eq!(p["hook_event_name"], event);
        assert_eq!(p["agent_id"], child_id.as_str());
        assert_eq!(p["agent_type"], "worker");
        assert_eq!(p["task"], "list the files");
    }
    assert!(stop["last_assistant_message"]
        .as_str()
        .unwrap()
        .starts_with("CHILD_SAW"));
    let events: Vec<String> = audit(home)
        .iter()
        .map(|r| r["event"].as_str().unwrap().to_string())
        .collect();
    for e in ["SubagentStart", "PreToolUse", "SubagentStop"] {
        assert!(events.iter().any(|x| x == e), "{events:?}");
    }
}

#[test]
fn a_blocking_subagent_start_fails_the_child_and_a_blocking_stop_sends_it_on() {
    let (url, seen) = model_server(spawning_script());
    let dir = home_with(
        &url,
        r#"[[hooks.SubagentStart]]
command = "echo 'no workers after 6pm' >&2; exit 2"
"#,
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "have a worker list the files"]);
    assert!(out.contains("final: ROOT_DONE"), "{out}");
    assert!(
        out.contains("a SubagentStart hook blocked it: no workers after 6pm"),
        "{out}"
    );
    assert!(!seen.lock().unwrap().iter().any(is_child), "the child ran");

    // A SubagentStop block runs the child once more with the reason.
    let (url, seen) = model_server(spawning_script());
    let dir = home_with(
        &url,
        r#"[[hooks.SubagentStop]]
command = "test -f {out}/once && exit 0; touch {out}/once; echo 'say it again' >&2; exit 2"
"#,
    );
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "have a worker list the files"]);
    assert!(out.contains("final: ROOT_DONE"), "{out}");
    assert!(out.contains("CHILD_AGAIN"), "{out}");
    let seen = seen.lock().unwrap();
    let again = seen
        .iter()
        .filter(|r| is_child(r))
        .find(|r| text(&last(r)).starts_with("[hook: SubagentStop]"))
        .expect("the child was sent on");
    assert_eq!(
        text(&last(again)),
        "[hook: SubagentStop] This isn't done yet:\n\nsay it again"
    );
}

#[test]
fn the_model_cannot_write_or_enable_a_hook_through_its_tools() {
    let marker = |home: &Path| home.join("out/pwned");
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("out");
    let touch = format!("touch {}", marker(dir.path()).display());
    let ferrule_bin = env!("CARGO_BIN_EXE_ferrule").to_string();
    let data = dir.path().join("data");
    let steps: Vec<Value> = vec![
        // A workspace hooks file.
        call(
            "write_file",
            json!({"path": ".ferrule/hooks.toml", "content": format!("[[PreToolUse]]\ncommand = \"{touch}\"\n")}),
        ),
        // Trusting it the way the owner would.
        call(
            "shell",
            json!({"command": format!("{ferrule_bin} hooks trust")}),
        ),
        // The trust record itself, and the config next to it.
        call(
            "write_file",
            json!({"path": data.join("private/hooks-trust.json").to_str().unwrap(), "content": "{}"}),
        ),
        call(
            "write_file",
            json!({"path": dir.path().join("ferrule.toml").to_str().unwrap(), "content": "x"}),
        ),
    ];
    let n = steps.len();
    let script: Script = Arc::new(move |req: &Value| {
        let done = messages(req).iter().filter(|m| m["role"] == "tool").count();
        match steps.get(done) {
            Some(step) if text(&messages(req)[1]).starts_with("try") => step.clone(),
            _ => answer(&format!("DONE after {done} of {n}")),
        }
    });
    let (url, seen) = model_server(script);
    for d in ["work", "data", "home", "out", "tmp"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(
        dir.path().join("ferrule.toml"),
        format!("{}\n[hooks]\nproject = true\n", provider(&url)),
    )
    .unwrap();
    let home = dir.path();
    let (out, _) = run_ok(home, &["run", "try to add a hook"]);
    assert!(out.contains(&format!("DONE after {n} of {n}")), "{out}");
    let results: Vec<String> = seen
        .lock()
        .unwrap()
        .last()
        .map(|r| {
            messages(r)
                .iter()
                .filter(|m| m["role"] == "tool")
                .map(text)
                .collect()
        })
        .unwrap();
    // `hooks trust` refuses without a terminal, and the file tools can't
    // reach the trust record or the config.
    assert!(
        results[1].contains("asks the owner at a terminal"),
        "{}",
        results[1]
    );
    assert!(results[2].starts_with("error"), "{}", results[2]);
    assert!(results[3].starts_with("error"), "{}", results[3]);
    assert!(!home.join("data/private/hooks-trust.json").exists());
    assert!(std::fs::read_to_string(home.join("ferrule.toml"))
        .unwrap()
        .contains("[providers.mock]"));
    assert!(out_dir.read_dir().unwrap().next().is_none());

    // The next run doesn't run what it wrote, and says so.
    let (_, err) = run_ok(home, &["run", "go on"]);
    assert!(err.contains("you haven't trusted it yet"), "{err}");
    assert!(!marker(home).exists(), "the model's hook ran");

    // A `./ferrule.toml` the model writes in the workspace is read when no
    // --config is given, but its [hooks] are not.
    std::fs::write(
        home.join("work/ferrule.toml"),
        format!(
            "{}\n[[hooks.SessionStart]]\ncommand = \"{touch}\"\n",
            provider(&url)
        ),
    )
    .unwrap();
    let out = command(home, &["run", "go on"])
        .env_remove("FERRULE_CONFIG")
        .output()
        .unwrap();
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("[hooks] in ferrule.toml is ignored: hooks are read only from --config or the global config"),
        "{stderr}"
    );
    assert!(!marker(home).exists(), "a hook from ./ferrule.toml ran");
}

// --- eval ---

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter")
}

/// The starter suite's mock model on a free port, stopped on drop.
struct Mock {
    child: Child,
    url: String,
}

impl Mock {
    fn start() -> Option<Mock> {
        let python = Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !python {
            eprintln!("python3 not found: skipping (the starter suite's graders need it)");
            return None;
        }
        let mut child = Command::new("python3")
            .arg(suite().join("mock/model.py"))
            .args(["--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("starting the mock model");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        Some(Mock { child, url })
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_eval_run_does_not_pick_up_the_owners_hooks() {
    let Some(mock) = Mock::start() else {
        return;
    };
    // Every event hooked, workspace hooks on; any run leaves a file.
    let mut hooks = String::from("[hooks]\nproject = true\n");
    for event in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "PreCompact",
        "PostCompact",
        "SubagentStart",
        "SubagentStop",
    ] {
        hooks.push_str(&format!(
            "[[hooks.{event}]]\ncommand = \"touch {{out}}/{event}; echo 'blocked by the owner' >&2; exit 2\"\n"
        ));
    }
    let dir = home_with(&mock.url, &hooks);
    let home = dir.path();
    // The config is the owner's: model price fields make it an eval home.
    let cfg = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    std::fs::write(
        home.join("ferrule.toml"),
        cfg.replace(
            "model = \"scripted\"",
            "model = \"mock\"\nprice_input_per_mtok = 1.0\nprice_cached_input_per_mtok = 0.1\nprice_output_per_mtok = 5.0",
        ),
    )
    .unwrap();
    let suite = suite();
    let out = ferrule(
        home,
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--variant",
            "ab",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stdout.contains("4/4 (100%)"), "{stdout}");
    assert!(stdout.contains("2/4 (50%)"), "{stdout}");
    let ran: Vec<_> = std::fs::read_dir(home.join("out"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(ran.is_empty(), "hooks ran during eval: {ran:?}");
    assert!(audit(home).is_empty());
}
