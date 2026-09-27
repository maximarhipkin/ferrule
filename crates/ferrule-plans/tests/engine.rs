//! The Claude Code engine against a stand-in `claude`
//! (`src/bin/fake-claude.rs`): resume, streaming, the permission prompt, a
//! bridged tool, limits, errors, and killing the whole process group.

use ferrule_core::guard::{Guard, GuardedCall, Verdict};
use ferrule_core::message::{Message, Role};
use ferrule_core::provider::{
    with_call_context, CallContext, CompletionRequest, Delta, DeltaSink, Provider,
};
use ferrule_core::tool::ToolDefinition;
use ferrule_core::CoreError;
use ferrule_plans::claude::token::TokenStore;
use ferrule_plans::claude::{ClaudeCode, EngineConfig};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FAKE: &str = env!("CARGO_BIN_EXE_ferrule-fake-claude");

struct Setup {
    _tmp: tempfile::TempDir,
    config_dir: PathBuf,
    data: PathBuf,
    engine: ClaudeCode,
}

fn setup(tweak: impl FnOnce(&mut EngineConfig)) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("claude-code");
    let data = tmp.path().join("data");
    let workspace = tmp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut cfg = EngineConfig::new(PathBuf::from(FAKE), config_dir.clone(), workspace);
    cfg.data_dir = Some(data.clone());
    cfg.private_dir = Some(tmp.path().join("private"));
    cfg.bridge_command = Some((PathBuf::from("ferrule"), vec!["claude-mcp".into()]));
    tweak(&mut cfg);
    Setup {
        _tmp: tmp,
        config_dir,
        data,
        engine: ClaudeCode::new("claude-code", "haiku", cfg),
    }
}

fn req(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        messages,
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    }
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object"}),
    }
}

/// What the fake saw on each call, oldest first.
fn calls(config_dir: &Path) -> Vec<Value> {
    let mut files: Vec<_> = std::fs::read_dir(config_dir.join("fake"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
        .collect()
}

fn has_flag(call: &Value, flag: &str) -> bool {
    call["args"].as_array().unwrap().iter().any(|a| a == flag)
}

fn flag(call: &Value, flag: &str) -> Option<String> {
    let args = call["args"].as_array().unwrap();
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).and_then(|v| v.as_str()).map(str::to_string)
}

#[tokio::test]
async fn a_turn_streams_resumes_its_session_and_reports_usage() {
    let s = setup(|_| {});
    let seen = Arc::new(Mutex::new(String::new()));
    let sink_seen = seen.clone();
    let mut first = req(vec![
        Message::system("You are Devi."),
        Message::user("hello"),
    ]);
    first.stream = Some(DeltaSink::new(move |d| {
        if let Delta::Text(t) = d {
            sink_seen.lock().unwrap().push_str(&t);
        }
    }));
    let r = s.engine.complete(first).await.unwrap();
    let text = r.message.content.clone().unwrap();
    assert_eq!(text, "turn 1: you said hello");
    assert_eq!(
        *seen.lock().unwrap(),
        text,
        "the answer streamed as it came"
    );
    let native = r
        .message
        .native
        .clone()
        .expect("the session rides on native");
    assert_eq!(native.api, "claude-code");
    assert_eq!(native.model, "claude-haiku-4-5-20251001");
    let session = native.items[0]["session_id"].as_str().unwrap().to_string();

    // Usage: cache reads and writes inside input, claude's cost as notional.
    assert_eq!(r.usage.input_tokens, 6110);
    assert_eq!(r.usage.cached_input_tokens, 6000);
    assert_eq!(r.usage.cache_write_input_tokens, 100);
    assert_eq!(r.usage.output_tokens, 7);
    assert_eq!(r.usage.notional_usd, Some(0.0123));

    // The rate-limit reading went to the usage file.
    let reading = ferrule_plans::UsageFile::new(&s.data)
        .get("claude-code")
        .unwrap();
    assert_eq!(reading.windows[0].name, "5h");
    assert_eq!(reading.windows[0].used_percent, 18.0);
    assert_eq!(reading.limited_until, None);

    // The second turn resumes and sends only the new message.
    let r2 = s
        .engine
        .complete(req(vec![
            Message::system("You are Devi."),
            Message::user("hello"),
            r.message.clone(),
            Message::user("again"),
        ]))
        .await
        .unwrap();
    assert_eq!(
        r2.message.content.as_deref(),
        Some("turn 2: you said again")
    );
    let seen = calls(&s.config_dir);
    assert_eq!(seen.len(), 2);
    assert_eq!(
        flag(&seen[1], "--resume").as_deref(),
        Some(session.as_str())
    );
    assert_eq!(seen[1]["prompt"], "again");
    assert!(!has_flag(&seen[0], "--resume"));
    // The flags that keep the child on the plan and inside ferrule's gates.
    for f in [
        "--strict-mcp-config",
        "--include-partial-messages",
        "--append-system-prompt-file",
        "--mcp-config",
    ] {
        assert!(has_flag(&seen[0], f), "{f}");
    }
    assert!(!has_flag(&seen[0], "--bare"));
    assert_eq!(flag(&seen[0], "--setting-sources").as_deref(), Some("user"));
    assert_eq!(
        flag(&seen[0], "--permission-prompt-tool").as_deref(),
        Some("mcp__ferrule__approve")
    );
    assert_eq!(seen[0]["autoupdater_off"], "1");
    assert_eq!(seen[0]["mcp_tool_timeout"], "1200000");
    // The turn's files are gone with the turn.
    let turns = s.config_dir.join("ferrule-turns");
    assert_eq!(std::fs::read_dir(&turns).unwrap().count(), 0);
}

#[tokio::test]
async fn the_persona_is_appended_and_a_lost_session_restarts_with_a_recap() {
    let s = setup(|_| {});
    let r = s
        .engine
        .complete(req(vec![
            Message::system("You are Devi."),
            Message::user("[system]"),
        ]))
        .await
        .unwrap();
    assert_eq!(r.message.content.as_deref(), Some("system: You are Devi."));

    // The session claude had is gone (a new config dir, say).
    let mut stale = r.message.clone();
    stale.native.as_mut().unwrap().items[0] = json!({"session_id": "gone"});
    let r = s
        .engine
        .complete(req(vec![
            Message::user("my name is Max"),
            stale,
            Message::user("what's my name?"),
        ]))
        .await
        .unwrap();
    assert!(
        r.message.content.unwrap().starts_with("turn 1: you said"),
        "a fresh session"
    );
    let seen = calls(&s.config_dir);
    let fresh = seen.last().unwrap();
    assert!(!has_flag(fresh, "--resume"));
    let prompt = fresh["prompt"].as_str().unwrap();
    assert!(prompt.contains("User: my name is Max"), "{prompt}");
    assert!(prompt.ends_with("what's my name?"), "{prompt}");
}

/// A guard that refuses `rm` and records what it was asked.
struct RefuseRm(Mutex<Vec<(String, Value, bool)>>);

#[async_trait::async_trait]
impl Guard for RefuseRm {
    fn before_model_call(&self) -> Option<String> {
        None
    }
    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict {
        self.0
            .lock()
            .unwrap()
            .push((call.tool.to_string(), call.args.clone(), call.changes_files));
        if call.args["command"]
            .as_str()
            .is_some_and(|c| c.starts_with("rm"))
        {
            Verdict::Refuse("rm needs the owner's approval".into())
        } else {
            Verdict::Allow
        }
    }
    async fn halted(&self) -> String {
        std::future::pending().await
    }
}

#[tokio::test]
async fn claudes_permission_prompt_asks_the_runs_guard() {
    let s = setup(|_| {});
    let guard = Arc::new(RefuseRm(Mutex::default()));
    let ctx = CallContext {
        guard: Some(guard.clone()),
        workspace: None,
    };
    let r = with_call_context(
        ctx,
        s.engine.complete(req(vec![Message::user("[approve]")])),
    )
    .await
    .unwrap();
    let text = r.message.content.unwrap();
    let parts: Vec<Value> = ["ls=", " rm=", " ours="]
        .windows(2)
        .map(|w| {
            let a = text.find(w[0]).unwrap() + w[0].len();
            let b = text.find(w[1]).unwrap();
            serde_json::from_str(&text[a..b]).unwrap()
        })
        .collect();
    assert_eq!(parts[0]["behavior"], "allow");
    assert_eq!(parts[0]["updatedInput"]["command"], "ls");
    assert_eq!(parts[1]["behavior"], "deny");
    assert_eq!(parts[1]["message"], "rm needs the owner's approval");
    let ours: Value = serde_json::from_str(&text[text.find(" ours=").unwrap() + 6..]).unwrap();
    assert_eq!(ours["behavior"], "allow");
    // Bash is judged as ferrule's shell; the bridge's own tool isn't asked.
    let asked = guard.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].0, "shell");
    assert_eq!(asked[0].1, json!({"command": "ls"}));
    assert!(asked[0].2);
}

#[tokio::test]
async fn a_bridged_ferrule_tool_pauses_the_turn_until_the_agent_has_run_it() {
    let s = setup(|_| {});
    let mut first = req(vec![Message::user("[tool]")]);
    first.tools = vec![tool("remember"), tool("shell"), tool("read_file")];
    let r = s.engine.complete(first.clone()).await.unwrap();
    let call = r.message.tool_calls.first().expect("a tool call").clone();
    assert_eq!(call.name, "remember");
    assert_eq!(call.arguments["note"], "the sky is green");
    assert_eq!(r.usage.output_tokens, 0, "usage comes with the answer");

    // The agent runs the tool and comes back with its result.
    let mut again = first;
    again.messages.push(r.message);
    again
        .messages
        .push(Message::tool_result(call.id.clone(), "kept it"));
    let r = s.engine.complete(again).await.unwrap();
    assert!(r.message.tool_calls.is_empty());
    assert_eq!(
        r.message.content.as_deref(),
        Some("tools=approve,remember tool said: kept it"),
        "only the table's tools are bridged"
    );
    assert_eq!(r.usage.notional_usd, Some(0.0123));
    assert_eq!(
        calls(&s.config_dir).len(),
        1,
        "one claude for the whole turn"
    );
}

#[tokio::test]
async fn a_rejected_limit_waits_for_the_reset_and_is_recorded() {
    let s = setup(|_| {});
    let e = s
        .engine
        .complete(req(vec![Message::user("[rejected]")]))
        .await
        .unwrap_err();
    let CoreError::Transient {
        message,
        retry_after,
    } = e
    else {
        panic!("{e:?}")
    };
    assert!(
        message.starts_with(
            "usage_limit_reached: the Claude plan's usage limit is reached (5h); it resets in"
        ),
        "{message}"
    );
    let wait = retry_after.unwrap().as_secs();
    assert!((3500..=3600).contains(&wait), "{wait}");
    let reading = ferrule_plans::UsageFile::new(&s.data)
        .get("claude-code")
        .unwrap();
    assert!(reading.limited_until.is_some());
}

#[tokio::test]
async fn errors_say_what_happened() {
    let s = setup(|_| {});
    let e = s
        .engine
        .complete(req(vec![Message::user("[login]")]))
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        CoreError::Provider(ferrule_plans::claude::stream::NOT_SIGNED_IN.into()).to_string()
    );

    let e = s
        .engine
        .complete(req(vec![Message::user("[crash]")]))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("ended without an answer (exit 3)"), "{e}");
    assert!(e.contains("boom"), "{e}");

    let missing = setup(|c| c.binary = PathBuf::from("/nonexistent/claude"));
    let e = missing
        .engine
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("npm install -g @anthropic-ai/claude-code"),
        "{e}"
    );

    let capped = setup(|c| c.max_output_bytes = 64 * 1024);
    let e = capped
        .engine
        .complete(req(vec![Message::user("[flood]")]))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("wrote more than"), "{e}");

    let slow = setup(|c| c.turn_timeout = Duration::from_millis(1500));
    let e = slow
        .engine
        .complete(req(vec![Message::user("[hang]")]))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("ran past"), "{e}");
}

#[tokio::test]
async fn a_pasted_token_reaches_only_the_child() {
    let s = setup(|_| {});
    let private = s.config_dir.parent().unwrap().join("private");
    let token = "sk-ant-oat01-fake-token-for-tests";
    TokenStore::new(&private).save(token, 1).unwrap();
    let r = s
        .engine
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap();
    let seen = calls(&s.config_dir);
    assert_eq!(
        seen[0]["token_sha"],
        ferrule_connections::seal::sha256_b64(token.as_bytes())
    );
    // Not in the argv, the answer, or anything left on disk.
    assert!(!seen[0]["args"].to_string().contains(token));
    assert!(!format!("{r:?}").contains(token));
    for entry in walk(s.config_dir.parent().unwrap()) {
        let bytes = std::fs::read(&entry).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(token),
            "{}",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// `/stop` drops the turn: claude and what it started go with it.
#[cfg(unix)]
#[tokio::test]
async fn stopping_a_turn_kills_claudes_whole_process_group() {
    let s = setup(|_| {});
    let pid_file = s.config_dir.join("fake").join("grandchild.pid");
    let mut turn = Box::pin(s.engine.complete(req(vec![Message::user("[hang]")])));
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::select! {
            _ = &mut turn => panic!("the turn should hang"),
            _ = async {
                while !pid_file.exists() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => {}
        }
    })
    .await;
    assert!(waited.is_ok(), "the fake never started its grandchild");
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let alive = || {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
        !stat.is_empty() && !stat.starts_with('Z')
    };
    assert!(alive(), "the grandchild runs while the turn does");
    // What /stop does: the run's future is dropped.
    drop(turn);
    for _ in 0..100 {
        if !alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("claude's grandchild {pid} outlived the stopped turn");
}

#[tokio::test]
async fn a_read_only_run_takes_claudes_writing_tools_away() {
    let s = setup(|_| {});
    s.engine
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap();
    let seen = calls(&s.config_dir);
    let disallowed = flag(&seen[0], "--disallowedTools").unwrap();
    for t in ["Edit", "Write", "Bash"] {
        assert!(disallowed.split(',').any(|d| d == t), "{disallowed}");
    }
    assert_eq!(
        flag(&seen[0], "--allowedTools").as_deref(),
        Some("Read,Glob,Grep,LS,TodoWrite,mcp__ferrule__*")
    );
    assert_eq!(
        seen[0]["cwd"].as_str().map(|c| c.ends_with("ws")),
        Some(true)
    );
    let _ = Role::User;
}
