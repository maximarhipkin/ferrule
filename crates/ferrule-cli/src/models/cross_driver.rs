//! M23: a model falling over mid-conversation to one on another driver,
//! through the real agent loop, the real drivers and [`RoutedProvider`],
//! against canned servers on 127.0.0.1. What one driver kept for itself
//! (thinking and its signature, encrypted reasoning, provider ids) never
//! reaches the other.

use super::*;
use ferrule_core::tool::ToolDefinition;
use ferrule_core::{Agent, AgentConfig, AgentEvent, RetryPolicy};
use ferrule_core::{Tool, ToolContext, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use tokio::sync::mpsc;

type Seen = std::thread::JoinHandle<Vec<(String, Value)>>;

/// Serve `(status, body)` in order, one connection each; the handle yields
/// every request's line (`POST /v1/messages HTTP/1.1`) and body.
fn serve(replies: Vec<(&'static str, String)>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            seen.push(read_request(&mut stream));
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
        }
        seen
    });
    (format!("http://127.0.0.1:{port}/v1"), handle)
}

/// Every request body the server saw.
fn bodies(seen: Seen) -> Vec<Value> {
    seen.join().unwrap().into_iter().map(|(_, b)| b).collect()
}

fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            let len = text[..end]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                let line = text.lines().next().unwrap_or_default().to_string();
                return (line, serde_json::from_str(&text[end + 4..]).unwrap());
            }
        }
        if n == 0 {
            panic!("the connection closed mid-request");
        }
    }
}

struct ReadNote;

#[async_trait::async_trait]
impl Tool for ReadNote {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_note".into(),
            description: "Read a note".into(),
            parameters: json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        }
    }
    async fn call(&self, args: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!("note {}: buy milk", args["name"])))
    }
}

/// The agent on the config's default, falling over as configured, and the
/// fallback events it sent.
async fn run(config: &str) -> (String, Vec<(String, String)>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ferrule.toml");
    std::fs::write(&path, config).unwrap();
    let cfg: Config = toml::from_str(config).unwrap();
    let models = Arc::new(Models::new(path, None, &cfg));
    let provider = Arc::new(RoutedProvider::new(
        models,
        Scope::default(),
        "configured".into(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ReadNote));
    let config = AgentConfig {
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            budget: Duration::from_secs(5),
        },
        ..Default::default()
    };
    let mut agent = Agent::new(
        provider,
        tools,
        HarnessProfile::generic(),
        config,
        ToolContext::default(),
        None,
    )
    .with_system_prompt("You are Ferrule.");
    let (tx, mut rx) = mpsc::channel(256);
    let answer = agent.run("read the shopping note", tx).await.unwrap();
    let mut fallbacks = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let AgentEvent::ModelFallback { from, to, .. } = e {
            fallbacks.push((from, to));
        }
    }
    (answer, fallbacks)
}

fn chat_answer(text: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 3},
    })
    .to_string()
}

#[tokio::test]
async fn anthropic_with_thinking_falls_over_to_chat_mid_loop_without_its_blocks() {
    let tool_turn = json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
        "content": [
            {"type": "thinking", "thinking": "The note is called shopping.", "signature": "SIG-OPAQUE"},
            {"type": "tool_use", "id": "toolu_01", "name": "read_note", "input": {"name": "shopping"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 40, "cache_creation_input_tokens": 1200,
                  "cache_read_input_tokens": 0, "output_tokens": 30}
    })
    .to_string();
    let overloaded =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            .to_string();
    let (claude, claude_seen) = serve(vec![
        ("200 OK", tool_turn),
        ("529 Overloaded", overloaded.clone()),
        ("529 Overloaded", overloaded.clone()),
        ("529 Overloaded", overloaded),
    ]);
    let (kimi, kimi_seen) = serve(vec![("200 OK", chat_answer("Buy milk."))]);
    let config = format!(
        r#"
default_provider = "claude"

[models]
fallback = ["kimi"]

[providers.claude]
base_url = "{claude}"
api_key_env = "PATH"
model = "claude-sonnet-5"
api = "anthropic"
thinking = "adaptive"

[providers.kimi]
base_url = "{kimi}"
api_key_env = "PATH"
model = "kimi-k2.6"
"#
    );
    let (answer, fallbacks) = run(&config).await;
    assert_eq!(answer, "Buy milk.");
    assert_eq!(
        fallbacks,
        [(
            "claude/claude-sonnet-5".to_string(),
            "kimi/kimi-k2.6".to_string()
        )]
    );

    let claude_seen = bodies(claude_seen);
    assert_eq!(claude_seen.len(), 4, "the first turn, then three tries");
    assert_eq!(claude_seen[0]["thinking"], json!({"type": "adaptive"}));
    // Within the loop, on its own driver: the block goes back as it came.
    let retried = claude_seen[1]["messages"][1]["content"][0].clone();
    assert_eq!(retried["signature"], "SIG-OPAQUE");

    let kimi_body = bodies(kimi_seen).remove(0);
    let text = kimi_body.to_string();
    for private in ["SIG-OPAQUE", "signature", "thinking", "native", "reasoning"] {
        assert!(!text.contains(private), "{private} leaked: {text}");
    }
    let messages = kimi_body["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("the tool turn is in the history");
    assert_eq!(call["tool_calls"][0]["id"], "toolu_01");
    assert_eq!(call["tool_calls"][0]["function"]["name"], "read_note");
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(result["tool_call_id"], "toolu_01");
    assert!(result["content"].as_str().unwrap().contains("buy milk"));
}

#[tokio::test]
async fn responses_with_encrypted_reasoning_falls_over_to_anthropic_with_sanitized_ids() {
    let tool_turn = json!({
        "id": "resp_01", "object": "response", "status": "completed", "model": "gpt-5.5",
        "output": [
            {"id": "rs_01", "type": "reasoning", "summary": [{"type": "summary_text", "text": "Read it."}],
             "encrypted_content": "gAAAA-ENCRYPTED"},
            {"id": "fc_01", "type": "function_call", "status": "completed", "call_id": "call.A:1",
             "name": "read_note", "arguments": "{\"name\":\"shopping\"}"}
        ],
        "usage": {"input_tokens": 300, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 20},
        "error": null
    })
    .to_string();
    let down =
        r#"{"error":{"message":"The server had an error","type":"server_error"}}"#.to_string();
    let (openai, openai_seen) = serve(vec![
        ("200 OK", tool_turn),
        ("500 Internal Server Error", down.clone()),
        ("500 Internal Server Error", down.clone()),
        ("500 Internal Server Error", down),
    ]);
    let done = json!({
        "id": "msg_02", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "Buy milk."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 60, "output_tokens": 4}
    })
    .to_string();
    let (claude, claude_seen) = serve(vec![("200 OK", done)]);
    let config = format!(
        r#"
default_provider = "openai"

[models]
fallback = ["claude"]

[providers.openai]
base_url = "{openai}"
api_key_env = "PATH"
model = "gpt-5.5"
api = "responses"
effort = "medium"

[providers.claude]
base_url = "{claude}"
api_key_env = "PATH"
model = "claude-sonnet-5"
api = "anthropic"
"#
    );
    let (answer, fallbacks) = run(&config).await;
    assert_eq!(answer, "Buy milk.");
    assert_eq!(
        fallbacks,
        [(
            "openai/gpt-5.5".to_string(),
            "claude/claude-sonnet-5".to_string()
        )]
    );
    let openai_seen = bodies(openai_seen);
    assert!(
        openai_seen[1].to_string().contains("gAAAA-ENCRYPTED"),
        "replayed on its own driver"
    );

    let body = bodies(claude_seen).remove(0);
    let text = body.to_string();
    for private in ["ENCRYPTED", "rs_01", "fc_01", "reasoning", "thinking"] {
        assert!(!text.contains(private), "{private} leaked: {text}");
    }
    let messages = body["messages"].as_array().unwrap();
    let tool_use = messages[1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap();
    assert_eq!(tool_use["id"], "call_A_1");
    assert_eq!(tool_use["input"], json!({"name": "shopping"}));
    let result = messages[2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_result")
        .unwrap();
    assert_eq!(result["tool_use_id"], "call_A_1");
}

/// M22's `model test` goes through the model's own driver: one call each,
/// on its own path, and a refused key read plainly.
#[tokio::test]
async fn model_test_calls_each_model_on_its_own_driver() {
    let anthropic_ok = json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "OK"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 1}
    })
    .to_string();
    let responses_ok = json!({
        "id": "resp_1", "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "OK"}]}],
        "usage": {"input_tokens": 12, "output_tokens": 1}
    })
    .to_string();
    let refused =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            .to_string();
    let (a, a_seen) = serve(vec![
        ("200 OK", anthropic_ok),
        ("401 Unauthorized", refused),
    ]);
    let (r, r_seen) = serve(vec![("200 OK", responses_ok)]);
    let (c, c_seen) = serve(vec![("200 OK", chat_answer("OK"))]);
    let config = format!(
        r#"
default_provider = "claude"

[providers.claude]
base_url = "{a}"
api_key_env = "PATH"
model = "claude-sonnet-5"
api = "anthropic"

[providers.openai]
base_url = "{r}"
api_key_env = "PATH"
model = "gpt-5.5"
api = "responses"

[providers.local]
base_url = "{c}"
api_key_env = "PATH"
model = "qwen3-coder"
"#
    );
    let cat = Catalog::from_config(&toml::from_str(&config).unwrap());
    for reference in ["claude", "openai", "local"] {
        let out = test_entry(cat.resolve(reference).unwrap()).await;
        assert!(out.ok, "{reference}: {}", out.said);
    }
    let out = test_entry(cat.resolve("claude").unwrap()).await;
    assert!(!out.ok);
    assert!(out.said.contains("401"), "{}", out.said);

    let line = |seen: Seen| seen.join().unwrap()[0].0.clone();
    assert!(line(a_seen).starts_with("POST /v1/messages "));
    assert!(line(r_seen).starts_with("POST /v1/responses "));
    assert!(line(c_seen).starts_with("POST /v1/chat/completions "));
}
