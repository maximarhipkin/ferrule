//! A stand-in for `claude -p --output-format stream-json`, for the
//! engine's tests (never shipped: releases build `ferrule-cli` only).
//!
//! It records its argv and what matters of its environment in
//! `$CLAUDE_CONFIG_DIR/fake/`, keeps sessions there so `--resume` works,
//! and picks what to do from a `[word]` in the prompt it reads on stdin.
//! The token is recorded as a hash, never as itself.
//!
//! Its version is `fake-version` beside the binary (2.1.283 without one);
//! `update` sets it to `fake-latest` (2.2.0), or fails when
//! `fake-update-fails` is there, and records who ran it in
//! `fake-update.json`. A `[needs-update]` turn fails the way an outdated
//! claude does until the version is 2.2.0 or later.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

fn out(v: Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// The directory the binary is in, where its version lives.
fn home() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().to_path_buf()
}

fn version() -> String {
    std::fs::read_to_string(home().join("fake-version"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "2.1.283".into())
}

/// `claude update`, as the native installer's would.
fn update() -> i32 {
    let dir = home();
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0u32;
    let seen = json!({
        "uid": uid,
        "home": std::env::var("HOME").ok(),
        "autoupdater_off": std::env::var("DISABLE_AUTOUPDATER").ok(),
        "stdin_closed": std::io::stdin().read(&mut [0u8; 1]).map_or(true, |n| n == 0),
    });
    std::fs::write(dir.join("fake-update.json"), seen.to_string()).unwrap();
    if dir.join("fake-update-fails").exists() {
        eprintln!("Error: EACCES: permission denied, open '/somewhere/claude'");
        return 1;
    }
    let latest = std::fs::read_to_string(dir.join("fake-latest"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "2.2.0".into());
    let from = version();
    std::fs::write(dir.join("fake-version"), &latest).unwrap();
    println!("Successfully updated from {from} to version {latest}");
    0
}

/// `a` is at least `b`.
fn at_least(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').filter_map(|p| p.parse().ok()).collect() };
    parse(a) >= parse(b)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn result(session: &str, text: &str, is_error: bool, status: Option<u64>) -> Value {
    json!({
        "type": "result",
        "subtype": if is_error { "error_during_execution" } else { "success" },
        "is_error": is_error,
        "result": text,
        "session_id": session,
        "total_cost_usd": 0.0123,
        "api_error_status": status,
        "usage": {
            "input_tokens": 10,
            "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 6000,
            "output_tokens": 7
        },
        "modelUsage": {"claude-haiku-4-5-20251001": {}},
        "permission_denials": [],
    })
}

/// One MCP call over the bridge named in `--mcp-config`.
struct Mcp {
    write: TcpStream,
    read: BufReader<TcpStream>,
    id: u64,
}

impl Mcp {
    fn connect(config: &str) -> Mcp {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        let env = &doc["mcpServers"]["ferrule"]["env"];
        let port: u16 = env["FERRULE_BRIDGE_PORT"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let token = env["FERRULE_BRIDGE_TOKEN"].as_str().unwrap();
        let mut write = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(write, "{token}").unwrap();
        let read = BufReader::new(write.try_clone().unwrap());
        let mut m = Mcp { write, read, id: 0 };
        m.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        writeln!(
            m.write,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        m
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.write, "{msg}").unwrap();
        let mut line = String::new();
        self.read.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
    }

    fn call(&mut self, name: &str, args: Value) -> String {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        r["content"][0]["text"].as_str().unwrap_or("").to_string()
    }
}

/// `--version` and `auth status|login|logout`: a login is a file here.
fn not_a_turn(args: &[String], fake: &std::path::Path) -> Option<i32> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let marker = fake.join("logged-in");
    match words.as_slice() {
        ["--version"] => println!("{} (Claude Code)", version()),
        ["auth", "status", ..] => {
            let token = std::env::var_os("CLAUDE_CODE_OAUTH_TOKEN").is_some();
            let login = marker.exists();
            let method = match (token, login) {
                (true, _) => "oauth_token",
                (_, true) => "claude.ai",
                _ => "none",
            };
            println!(
                "{}",
                json!({"loggedIn": token || login, "authMethod": method,
                    "email": if login { Some("owner@example.com") } else { None },
                    "subscriptionType": if login { Some("max") } else { None }})
            );
            return Some(if token || login { 0 } else { 1 });
        }
        ["auth", "login", ..] => {
            std::fs::write(&marker, "").unwrap();
            println!("Login successful.");
        }
        ["auth", "logout"] => {
            let _ = std::fs::remove_file(&marker);
            println!("Successfully logged out from your Anthropic account.");
        }
        _ => return None,
    }
    Some(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["update"] {
        std::process::exit(update());
    }
    let config = PathBuf::from(std::env::var("CLAUDE_CONFIG_DIR").expect("CLAUDE_CONFIG_DIR"));
    let fake = config.join("fake");
    std::fs::create_dir_all(fake.join("sessions")).unwrap();
    if let Some(code) = not_a_turn(&args, &fake) {
        std::process::exit(code);
    }
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt).unwrap();

    let token = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
    let seen = json!({
        "args": args,
        "token_sha": token.as_deref().map(|t| ferrule_connections::seal::sha256_b64(t.as_bytes())),
        "anthropic_api_key": std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        "anthropic_base_url": std::env::var_os("ANTHROPIC_BASE_URL").is_some(),
        "claude_code_any": std::env::vars().any(|(k, _)| k.starts_with("CLAUDE_CODE_")
            && k != "CLAUDE_CODE_OAUTH_TOKEN"
            && k != "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"
            && k != "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB"),
        "autoupdater_off": std::env::var("DISABLE_AUTOUPDATER").ok(),
        "mcp_tool_timeout": std::env::var("MCP_TOOL_TIMEOUT").ok(),
        "cwd": std::env::current_dir().unwrap(),
        "prompt": prompt,
    });
    let n = std::fs::read_dir(&fake).unwrap().count();
    std::fs::write(fake.join(format!("call-{n:03}.json")), seen.to_string()).unwrap();

    let session = match arg(&args, "--resume") {
        Some(id) => {
            if !fake.join("sessions").join(&id).exists() {
                out(result(
                    &id,
                    &format!("No conversation found with session ID: {id}"),
                    true,
                    None,
                ));
                std::process::exit(1);
            }
            id
        }
        None => format!("fake-{}-{}", std::process::id(), now()),
    };
    let log = fake.join("sessions").join(&session);
    let mut turns = std::fs::read_to_string(&log).unwrap_or_default();
    turns.push_str(&prompt.replace('\n', " "));
    turns.push('\n');
    std::fs::write(&log, &turns).unwrap();

    out(
        json!({"type": "system", "subtype": "init", "session_id": session,
               "model": "claude-haiku-4-5-20251001", "tools": [], "mcp_servers": []}),
    );
    let limited = prompt.contains("[rejected]");
    out(json!({"type": "rate_limit_event", "rate_limit_info": {
        "status": if limited { "rejected" } else { "allowed" },
        "resetsAt": now() + 3600,
        "rateLimitType": "five_hour",
        "unifiedWindows": {
            "five_hour": {"utilization": if limited { 1.0 } else { 0.18 }, "resetsAt": now() + 3600},
            "seven_day": {"utilization": 0.05, "resetsAt": now() + 86400 * 3},
        }
    }}));

    let say = |text: &str| {
        for chunk in text.split_inclusive(' ') {
            out(
                json!({"type": "stream_event", "event": {"type": "content_block_delta",
                "index": 0, "delta": {"type": "text_delta", "text": chunk}}}),
            );
        }
        out(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}));
        out(result(&session, text, false, None));
    };

    if prompt.contains("[needs-update]") && !at_least(&version(), "2.2.0") {
        eprintln!(
            "Claude Code needs an update. A newer version (2.2.0 or higher) is required to continue."
        );
        std::process::exit(1);
    }
    if limited {
        out(result(
            &session,
            "Claude AI usage limit reached",
            true,
            Some(429),
        ));
        std::process::exit(1);
    } else if prompt.contains("[login]") {
        out(result(
            &session,
            "Invalid API key · Please run /login",
            true,
            Some(401),
        ));
        std::process::exit(1);
    } else if prompt.contains("[crash]") {
        eprintln!("boom: something broke");
        std::process::exit(3);
    } else if prompt.contains("[hang]") {
        // Left running on purpose: the engine must kill it with the group.
        #[cfg(unix)]
        #[allow(clippy::zombie_processes)]
        {
            let child = std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .unwrap();
            std::fs::write(fake.join("grandchild.pid"), child.id().to_string()).unwrap();
        }
        out(
            json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}}}),
        );
        std::thread::sleep(std::time::Duration::from_secs(300));
    } else if prompt.contains("[flood]") {
        let line = "x".repeat(8192);
        loop {
            out(
                json!({"type": "stream_event", "event": {"type": "content_block_delta",
                "index": 0, "delta": {"type": "text_delta", "text": line}}}),
            );
        }
    } else if prompt.contains("[approve]") {
        let mut mcp = Mcp::connect(&arg(&args, "--mcp-config").expect("--mcp-config"));
        let ls = mcp.call(
            "approve",
            json!({"tool_name": "Bash", "input": {"command": "ls"}, "tool_use_id": "t1"}),
        );
        let rm = mcp.call(
            "approve",
            json!({"tool_name": "Bash", "input": {"command": "rm -rf /x"}, "tool_use_id": "t2"}),
        );
        let ours = mcp.call(
            "approve",
            json!({"tool_name": "mcp__ferrule__remember", "input": {}, "tool_use_id": "t3"}),
        );
        say(&format!("ls={ls} rm={rm} ours={ours}"));
    } else if prompt.contains("[tool]") {
        let mut mcp = Mcp::connect(&arg(&args, "--mcp-config").expect("--mcp-config"));
        let listed = mcp.request("tools/list", json!({}));
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let said = mcp.call("remember", json!({"note": "the sky is green"}));
        say(&format!("tools={} tool said: {said}", names.join(",")));
    } else if prompt.contains("[system]") {
        let sys = arg(&args, "--append-system-prompt-file")
            .map(|f| std::fs::read_to_string(f).unwrap())
            .unwrap_or_default();
        say(&format!("system: {sys}"));
    } else {
        let count = turns.lines().count();
        let last = prompt.lines().last().unwrap_or("").trim().to_string();
        say(&format!("turn {count}: you said {last}"));
    }
}
