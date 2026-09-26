//! M34 through the real `ferrule` binary: `ferrule doctor` on a provider
//! served by a stand-in Ollama (docs/local-models.md). The server answers
//! with Ollama's own shapes; the model loads (and `/api/ps` shows its
//! 4096-token window) only once a chat call reaches it, as Ollama does.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MODEL: &str = "qwen3-coder:30b";

/// A model that writes its tool call as text: the chat template is broken.
const BROKEN: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"qwen3-coder:30b","choices":[{"index":0,"message":{"role":"assistant","content":"<tool_call>\n{\"name\": \"lookup_order\", \"arguments\": {\"order_id\": \"A-1729\"}}\n</tool_call>"},"finish_reason":"stop"}],"usage":{"prompt_tokens":150,"completion_tokens":30,"total_tokens":180}}"#;

fn handle(mut stream: TcpStream, loaded: &AtomicBool) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
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
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
    let (code, text) = match path.as_str() {
        "/api/version" => (200, r#"{"version":"0.34.4"}"#.to_string()),
        "/api/tags" => (
            200,
            format!(r#"{{"models":[{{"name":"{MODEL}","model":"{MODEL}","size":18556688736,"details":{{"family":"qwen3moe"}}}}]}}"#),
        ),
        "/api/show" => (
            200,
            r#"{"parameters":"temperature 0.7","model_info":{"general.architecture":"qwen3moe","qwen3moe.context_length":262144},"capabilities":["completion","tools"]}"#.to_string(),
        ),
        "/api/ps" if loaded.load(Ordering::SeqCst) => (
            200,
            format!(r#"{{"models":[{{"name":"{MODEL}","model":"{MODEL}","size":19000000000,"context_length":4096}}]}}"#),
        ),
        "/api/ps" => (200, r#"{"models":[]}"#.to_string()),
        "/v1/models" => (
            200,
            format!(r#"{{"object":"list","data":[{{"id":"{MODEL}","object":"model","created":1,"owned_by":"library"}}]}}"#),
        ),
        "/v1/chat/completions" => {
            loaded.store(true, Ordering::SeqCst);
            (200, BROKEN.to_string())
        }
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// The stand-in Ollama's origin.
fn ollama() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let loaded = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let loaded = loaded.clone();
            std::thread::spawn(move || handle(stream, &loaded));
        }
    });
    origin
}

fn home(origin: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(
        dir.path().join("ferrule.toml"),
        format!(
            r#"default_provider = "ollama"

[providers.ollama]
base_url = "{origin}/v1"
api_key_env = "FERRULE_TEST_OLLAMA_KEY"
profile = "generic"
model = "{MODEL}"

[skills]
enabled = false

[sandbox]
mode = "off"
"#
        ),
    )
    .unwrap();
    dir
}

fn doctor(home: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.arg("doctor")
        .args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_OLLAMA_KEY", "none")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
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
        "OLLAMA_HOST",
        "RUST_LOG",
    ] {
        cmd.env_remove(var);
    }
    let out: Output = cmd.output().unwrap();
    plain(&out.stdout)
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

#[test]
fn doctor_names_the_server_the_real_window_and_a_broken_template() {
    let origin = ollama();
    let dir = home(&origin);

    // Without --ping-models nothing is loaded, so the window isn't known.
    let out = doctor(dir.path(), &[]);
    assert!(out.contains("ollama: Ollama 0.34.4 at 127.0.0.1:"), "{out}");
    assert!(
        out.contains("the window is known once Ollama loads it"),
        "{out}"
    );

    // The probe loads it: 4096 against generic's 128000 is the failure,
    // with the fix; the tool call written as text is a broken template.
    let out = doctor(dir.path(), &["--ping-models"]);
    assert!(
        out.contains("✗ local     ollama: qwen3-coder:30b: the server gives 4096 tokens, trained 262144, but ferrule plans for 128000 — Ollama drops the front of long prompts"),
        "{out}"
    );
    assert!(out.contains("`qwen3-coder:30b-32k`"), "{out}");
    assert!(out.contains("OLLAMA_CONTEXT_LENGTH=32768"), "{out}");
    assert!(
        out.contains("! local     ollama: qwen3-coder:30b: can't call tools (template broken)"),
        "{out}"
    );
    assert!(out.contains("re-pull the model"), "{out}");

    // Offline: no server is asked.
    let out = doctor(dir.path(), &["--offline"]);
    assert!(!out.contains(" local "), "{out}");
}
