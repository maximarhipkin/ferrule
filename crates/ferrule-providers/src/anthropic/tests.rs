//! Hand-written fixtures in the Messages API's documented wire format,
//! served from 127.0.0.1.

use super::*;
use crate::common::mock::{self, body, ok, serve, status};
use ferrule_core::error::FailureClass;
use std::time::Duration;

const MODEL: &str = "claude-sonnet-5";

fn provider(url: &str) -> AnthropicProvider {
    AnthropicProvider::new(
        "anthropic",
        url,
        "sk-ant-test",
        MODEL,
        DriverOptions::default(),
    )
}

fn with(options: DriverOptions) -> AnthropicProvider {
    AnthropicProvider::new("anthropic", "http://unused/v1", "k", MODEL, options)
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} a file"),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

fn call(id: &str, name: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({ "path": path }),
    }
}

fn req(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        messages,
        tools: vec![tool("read_file"), tool("list_dir")],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    }
}

/// Every `cache_control` taken out, to compare prefixes byte for byte.
fn unmarked(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| *k != "cache_control")
                .map(|(k, v)| (k.clone(), unmarked(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(unmarked).collect()),
        other => other.clone(),
    }
}

/// The paths of every `cache_control` in a request body.
fn breakpoints(b: &Value) -> Vec<String> {
    fn walk(v: &Value, at: String, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if o.contains_key("cache_control") {
                    out.push(at.clone());
                }
                for (k, v) in o {
                    walk(v, format!("{at}/{k}"), out);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    walk(v, format!("{at}/{i}"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(b, String::new(), &mut out);
    out
}

const TWO_TOOLS: &str = r#"{
  "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
  "content": [
    {"type": "text", "text": "Reading both."},
    {"type": "tool_use", "id": "toolu_01A", "name": "read_file", "input": {"path": "a.rs"}},
    {"type": "tool_use", "id": "toolu_01B", "name": "list_dir", "input": {"path": "src"}}
  ],
  "stop_reason": "tool_use", "stop_sequence": null,
  "usage": {"input_tokens": 50, "cache_creation_input_tokens": 2000,
            "cache_read_input_tokens": 0, "output_tokens": 70}
}"#;

const THINKING_TOOL: &str = r#"{
  "id": "msg_02", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
  "content": [
    {"type": "thinking", "thinking": "", "signature": "EqQBCkYIBxgCKkDsig=="},
    {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix"},
    {"type": "tool_use", "id": "toolu_02", "name": "read_file", "input": {"path": "a.rs"}}
  ],
  "stop_reason": "tool_use",
  "usage": {"input_tokens": 12, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 1900, "output_tokens": 40}
}"#;

const DONE: &str = r#"{
  "id": "msg_03", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
  "content": [{"type": "text", "text": "All done."}],
  "stop_reason": "end_turn",
  "usage": {"input_tokens": 60, "cache_creation_input_tokens": 300,
            "cache_read_input_tokens": 2000, "output_tokens": 5}
}"#;

#[tokio::test]
async fn the_request_has_system_on_top_tools_and_headers_and_no_temperature() {
    let (url, seen) = serve(vec![ok(DONE)]);
    let mut r = req(vec![
        Message::system("You are Ferrule."),
        Message::system("Recalled: the owner likes tests."),
        Message::user("hi"),
    ]);
    r.temperature = Some(0.0);
    r.max_output_tokens = Some(1024);
    let resp = provider(&url).complete(r).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("All done."));

    let raw = seen.join().unwrap().remove(0);
    let head = raw.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/messages "), "{head}");
    assert!(head.contains("x-api-key: sk-ant-test"));
    assert!(head.contains("anthropic-version: 2023-06-01"));
    assert!(!head.contains("authorization:"));
    let b = body(&raw);
    assert_eq!(b["model"], MODEL);
    assert_eq!(
        b["system"],
        json!([
            {"type": "text", "text": "You are Ferrule."},
            {"type": "text", "text": "Recalled: the owner likes tests.", "cache_control": {"type": "ephemeral"}}
        ])
    );
    assert_eq!(b["messages"][0]["role"], "user");
    assert!(b.get("temperature").is_none(), "{b}");
    // 1 024 asked, raised: a thinking model could spend it all thinking.
    assert_eq!(b["max_tokens"], 16_000);
    assert!(b.get("thinking").is_none() && b.get("output_config").is_none());
    assert_eq!(b["tool_choice"], json!({"type": "auto"}));
    assert_eq!(
        b["tools"][0],
        json!({"name": "read_file", "description": "read_file a file",
               "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}})
    );
}

#[test]
fn messages_are_shaped_the_way_the_api_wants_them() {
    let kimi_id = "functions.read_file:0";
    let r = req(vec![
        Message::system("sys"),
        // A compaction tail can start with the assistant.
        Message::assistant(Some("Earlier answer.".into()), vec![], None),
        Message::user("first"),
        Message::user("second"),
        Message::assistant(
            None,
            vec![
                call(kimi_id, "read_file", "a.rs"),
                call("c2", "list_dir", "src"),
            ],
            None,
        ),
        Message::tool_result(kimi_id, "fn main() {}"),
        Message::tool_result("c2", ""),
        Message::system("[hook: PostToolUse] note"),
        Message::user("and now?"),
    ]);
    let b = with(DriverOptions::default()).payload(&r, false).body;
    let m = unmarked(&b["messages"]);
    assert_eq!(
        m,
        json!([
            {"role": "user", "content": [{"type": "text", "text": "[continued]"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Earlier answer."}]},
            {"role": "user", "content": [
                {"type": "text", "text": "first"}, {"type": "text", "text": "second"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "functions_read_file_0", "name": "read_file", "input": {"path": "a.rs"}},
                {"type": "tool_use", "id": "c2", "name": "list_dir", "input": {"path": "src"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "functions_read_file_0", "content": "fn main() {}"},
                {"type": "tool_result", "tool_use_id": "c2"},
                {"type": "text", "text": "[system] [hook: PostToolUse] note"},
                {"type": "text", "text": "and now?"}]}
        ])
    );
}

#[tokio::test]
async fn a_multi_tool_turn_comes_back_as_calls_and_goes_back_as_one_user_message() {
    let (url, seen) = serve(vec![ok(TWO_TOOLS), ok(DONE)]);
    let p = provider(&url);
    let mut history = vec![Message::system("sys"), Message::user("look around")];
    let resp = p.complete(req(history.clone())).await.unwrap();
    let calls = &resp.message.tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(
        (calls[0].id.as_str(), calls[0].name.as_str()),
        ("toolu_01A", "read_file")
    );
    assert_eq!(calls[1].arguments, json!({"path": "src"}));
    assert_eq!(resp.message.content.as_deref(), Some("Reading both."));
    // No thinking came back: nothing to carry but which model served it.
    let native = resp.message.native.as_ref().unwrap();
    assert_eq!((native.api.as_str(), native.model.as_str()), (API, MODEL));
    assert!(native.items.is_empty());

    history.push(resp.message.clone());
    history.push(Message::tool_result("toolu_01A", "fn a() {}"));
    history.push(Message::tool_result("toolu_01B", "a.rs b.rs"));
    p.complete(req(history)).await.unwrap();

    let second = body(&seen.join().unwrap()[1]);
    let msgs = second["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let results = unmarked(&msgs[2]);
    assert_eq!(
        results,
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "fn a() {}"},
            {"type": "tool_result", "tool_use_id": "toolu_01B", "content": "a.rs b.rs"}]})
    );
}

#[tokio::test]
async fn cache_reads_and_writes_are_counted_and_the_prefix_stays_byte_identical() {
    let (url, seen) = serve(vec![ok(TWO_TOOLS), ok(DONE)]);
    let p = provider(&url);
    let mut history = vec![
        Message::system("A long, stable system prompt."),
        Message::user("look around"),
    ];
    let first = p.complete(req(history.clone())).await.unwrap();
    // Ferrule's convention: input includes the cached part.
    assert_eq!(first.usage.input_tokens, 50 + 2000);
    assert_eq!(first.usage.cache_write_input_tokens, 2000);
    assert_eq!(first.usage.cached_input_tokens, 0);
    assert_eq!(first.usage.output_tokens, 70);

    history.push(first.message);
    history.push(Message::tool_result("toolu_01A", "x"));
    history.push(Message::tool_result("toolu_01B", "y"));
    let second = p.complete(req(history)).await.unwrap();
    assert_eq!(second.usage.input_tokens, 60 + 300 + 2000);
    assert_eq!(second.usage.cached_input_tokens, 2000);
    assert_eq!(second.usage.cache_write_input_tokens, 300);

    // The hit rate, as the ledger reads it: cached over all input.
    let cached = first.usage.cached_input_tokens + second.usage.cached_input_tokens;
    let input = first.usage.input_tokens + second.usage.input_tokens;
    let hit = cached as f64 / input as f64;
    assert!((hit - 0.4535).abs() < 1e-3, "{hit}");

    let seen = seen.join().unwrap();
    let (a, b) = (body(&seen[0]), body(&seen[1]));
    // Breakpoints: the last system block, and the end of each request.
    assert_eq!(breakpoints(&a), ["/messages/0/content/0", "/system/0"]);
    assert_eq!(breakpoints(&b), ["/messages/2/content/1", "/system/0"]);
    // Request 2 starts with request 1, byte for byte.
    assert_eq!(
        unmarked(&a["system"]).to_string(),
        unmarked(&b["system"]).to_string()
    );
    assert_eq!(a["tools"].to_string(), b["tools"].to_string());
    let (am, bm) = (unmarked(&a["messages"]), unmarked(&b["messages"]));
    assert_eq!(am[0].to_string(), bm[0].to_string());
}

#[test]
fn the_previous_user_message_keeps_a_breakpoint_for_the_next_turn() {
    let r = CompletionRequest {
        tools: vec![],
        ..req(vec![
            Message::user("one"),
            Message::assistant(Some("ok".into()), vec![], None),
            Message::user("two"),
            Message::assistant(Some("ok".into()), vec![], None),
            Message::user("three"),
        ])
    };
    let b = with(DriverOptions::default()).payload(&r, false).body;
    assert_eq!(
        breakpoints(&b),
        ["/messages/2/content/0", "/messages/4/content/0"]
    );
    // The goal with its memory and a hook's note is one wire message: the
    // next turn's breakpoint goes where this turn's request ended.
    let turn = |n: &str| {
        vec![
            Message::user(n),
            Message::user("[Long-term memory]\n- #1 a fact"),
            Message::user("[hook: UserPromptSubmit]\nthe build is at /out"),
        ]
    };
    let mut msgs = turn("one");
    let a = with(DriverOptions::default())
        .payload(&req(msgs.clone()), false)
        .body;
    msgs.push(Message::assistant(Some("ok".into()), vec![], None));
    msgs.extend(turn("two"));
    let b = with(DriverOptions::default())
        .payload(&req(msgs), false)
        .body;
    assert_eq!(breakpoints(&a), ["/messages/0/content/2", "/tools/1"]);
    assert_eq!(
        breakpoints(&b),
        ["/messages/0/content/2", "/messages/2/content/2", "/tools/1"]
    );
    // With no system prompt, the last tool carries the first breakpoint.
    let b = with(DriverOptions::default())
        .payload(&req(vec![Message::user("one")]), false)
        .body;
    assert_eq!(breakpoints(&b), ["/messages/0/content/0", "/tools/1"]);
}

#[tokio::test]
async fn thinking_goes_back_verbatim_inside_the_loop_and_nowhere_else() {
    let (url, seen) = serve(vec![ok(THINKING_TOOL), ok(DONE)]);
    let p = provider(&url);
    let mut history = vec![Message::system("sys"), Message::user("read a.rs")];
    let resp = p.complete(req(history.clone())).await.unwrap();
    let native = resp.message.native.clone().unwrap();
    assert_eq!(native.items.len(), 3);
    // Omitted thinking text: nothing to show, and nothing shown anyway.
    assert_eq!(resp.message.reasoning, None);
    let debug = format!("{:?}", resp.message);
    assert!(!debug.contains("EqQBCkYIBxgCKkDsig"), "{debug}");
    assert!(!debug.contains("EmwKAhgBEgy3va3pzix"), "{debug}");

    history.push(resp.message.clone());
    history.push(Message::tool_result("toolu_02", "fn main() {}"));
    p.complete(req(history.clone())).await.unwrap();
    let sent = body(&seen.join().unwrap()[1]);
    let expected: Value = serde_json::from_str::<Value>(THINKING_TOOL).unwrap()["content"].clone();
    assert_eq!(sent["messages"][1]["content"], expected);

    // After the next user message, the old loop goes back neutral.
    history.push(Message::assistant(Some("done".into()), vec![], None));
    history.push(Message::user("thanks, and b.rs?"));
    let later = with(DriverOptions::default()).payload(&req(history.clone()), false);
    assert_eq!(
        later.body["messages"][1]["content"],
        json!([{"type": "tool_use", "id": "toolu_02", "name": "read_file", "input": {"path": "a.rs"}}])
    );
    assert!(!later.loop_extras);

    // And another model never gets them, even inside the loop.
    history.truncate(4);
    let other = AnthropicProvider::new(
        "a",
        "http://x/v1",
        "k",
        "claude-opus-5-5",
        DriverOptions::default(),
    );
    let b = other.payload(&req(history), false).body;
    assert!(!b.to_string().contains("signature"), "{b}");
}

#[test]
fn thinking_and_effort_go_out_only_when_set_and_only_on_our_own_loop() {
    let adaptive = DriverOptions {
        thinking: Some(Thinking::Adaptive),
        effort: Some("high".into()),
        max_tokens: None,
    };
    let fresh = req(vec![Message::user("hi")]);
    let b = with(adaptive.clone()).payload(&fresh, false).body;
    assert_eq!(b["thinking"], json!({"type": "adaptive"}));
    assert_eq!(b["output_config"], json!({"effort": "high"}));

    // A loop that began on another provider (M21 fell back mid-loop).
    let foreign = req(vec![
        Message::user("hi"),
        Message::assistant(None, vec![call("call_1", "read_file", "a")], None),
        Message::tool_result("call_1", "x"),
    ]);
    let p = with(adaptive.clone()).payload(&foreign, false);
    assert!(p.body.get("thinking").is_none(), "{}", p.body);
    assert!(p.loop_extras, "effort still went with a loop in progress");
    // Our own turn in the loop: thinking stays on.
    let mut own = foreign.clone();
    own.messages[1] = own.messages[1].clone().with_native(NativeBlocks {
        api: API.into(),
        model: MODEL.into(),
        items: vec![],
    });
    assert_eq!(
        with(adaptive).payload(&own, false).body["thinking"]["type"],
        "adaptive"
    );

    // A budget (older models) raises max_tokens above it.
    let budget = DriverOptions {
        thinking: Some(Thinking::Budget(20_000)),
        ..Default::default()
    };
    let b = with(budget).payload(&fresh, false).body;
    assert_eq!(
        b["thinking"],
        json!({"type": "enabled", "budget_tokens": 20_000})
    );
    assert_eq!(b["max_tokens"], 24_096);

    // Disabled: the caller's cap stands.
    let off = DriverOptions {
        thinking: Some(Thinking::Disabled),
        ..Default::default()
    };
    let mut small = fresh.clone();
    small.max_output_tokens = Some(1024);
    let b = with(off).payload(&small, false).body;
    assert_eq!(b["thinking"], json!({"type": "disabled"}));
    assert_eq!(b["max_tokens"], 1024);
}

#[tokio::test]
async fn a_thinking_400_is_retried_once_without_the_blocks() {
    let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#;
    let (url, seen) = serve(vec![
        status("400 Bad Request", "content-type: application/json\r\n", bad),
        ok(DONE),
    ]);
    let p = AnthropicProvider::new(
        "anthropic",
        &url,
        "k",
        MODEL,
        DriverOptions {
            thinking: Some(Thinking::Adaptive),
            ..Default::default()
        },
    );
    let first: CompletionResponse = serde_json::from_str::<Value>(THINKING_TOOL)
        .map(|v| p.parse_response(&v).unwrap())
        .unwrap();
    let history = vec![
        Message::user("read a.rs"),
        first.message,
        Message::tool_result("toolu_02", "x"),
    ];
    let resp = p.complete(req(history)).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("All done."));
    let seen = seen.join().unwrap();
    assert!(body(&seen[0]).to_string().contains("signature"));
    let retry = body(&seen[1]);
    assert!(!retry.to_string().contains("signature"), "{retry}");
    assert!(retry.get("thinking").is_none());
}

#[tokio::test]
async fn a_400_without_our_blocks_is_not_retried() {
    let bad = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking.type: disabled is not supported for this model"}}"#;
    let (url, seen) = serve(vec![status(
        "400 Bad Request",
        "content-type: application/json\r\n",
        bad,
    )]);
    let p = AnthropicProvider::new(
        "anthropic",
        &url,
        "k",
        MODEL,
        DriverOptions {
            thinking: Some(Thinking::Disabled),
            ..Default::default()
        },
    );
    let err = p
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::BadRequest);
    assert!(
        err.to_string().contains("not supported for this model"),
        "{err}"
    );
    assert_eq!(seen.join().unwrap().len(), 1);
}

async fn error_for(reply: mock::Canned) -> CoreError {
    let (url, _seen) = serve(vec![reply]);
    provider(&url)
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap_err()
}

const JSON: &str = "content-type: application/json\r\n";

#[tokio::test]
async fn errors_map_to_retries_fallback_and_classes() {
    let e = error_for(status(
        "529 Overloaded",
        JSON,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    ))
    .await;
    assert!(e.is_transient(), "{e:?}");
    assert_eq!(e.class(), FailureClass::Overloaded);

    let e = error_for(status(
        "429 Too Many Requests",
        "content-type: application/json\r\nretry-after: 7\r\n",
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#,
    ))
    .await;
    assert!(
        matches!(&e, CoreError::Transient { retry_after: Some(d), .. } if *d == Duration::from_secs(7)),
        "{e:?}"
    );
    assert_eq!(e.class(), FailureClass::RateLimited);

    let e = error_for(status(
        "400 Bad Request",
        JSON,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 212000 tokens > 200000 maximum"}}"#,
    ))
    .await;
    assert!(!e.is_transient());
    assert_eq!(e.class(), FailureClass::ContextTooLong);

    let e = error_for(status(
        "401 Unauthorized",
        JSON,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    ))
    .await;
    assert_eq!(e.class(), FailureClass::Auth);

    let e = error_for(status(
        "404 Not Found",
        JSON,
        r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-nope"}}"#,
    ))
    .await;
    assert_eq!(e.class(), FailureClass::ModelNotFound);

    let e = error_for(status(
        "502 Bad Gateway",
        "content-type: text/html\r\n",
        "<html>bad gateway</html>",
    ))
    .await;
    assert!(e.is_transient());
    assert_eq!(e.class(), FailureClass::Server);
}

#[tokio::test]
async fn a_refusal_is_final_and_says_why() {
    let e = error_for(ok(r#"{
      "id": "msg_r", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
      "content": [], "stop_reason": "refusal",
      "stop_details": {"type": "refusal", "category": "cyber", "explanation": "…"},
      "usage": {"input_tokens": 10, "output_tokens": 0}
    }"#))
    .await;
    assert!(!e.is_transient(), "no retry, no fallback");
    assert_eq!(e.class(), FailureClass::Refused);
    assert!(e.to_string().contains("refused: cyber"), "{e}");
}

#[tokio::test]
async fn max_tokens_returns_the_text_so_far() {
    let (url, _seen) = serve(vec![ok(r#"{
      "id": "msg_m", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
      "content": [{"type": "text", "text": "The first half"}],
      "stop_reason": "max_tokens",
      "usage": {"input_tokens": 10, "output_tokens": 16000}
    }"#)]);
    let resp = provider(&url)
        .complete(req(vec![Message::user("write a lot")]))
        .await
        .unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("The first half"));
    assert_eq!(resp.usage.input_tokens, 10);
}

#[test]
fn visible_thinking_text_goes_to_reasoning() {
    let p = with(DriverOptions::default());
    let resp = p
        .parse_response(&json!({
            "content": [
                {"type": "thinking", "thinking": "Step one.", "signature": "sig"},
                {"type": "text", "text": "Answer."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
    assert_eq!(resp.message.reasoning.as_deref(), Some("Step one."));
    assert_eq!(resp.message.content.as_deref(), Some("Answer."));
    assert_eq!(resp.message.native.unwrap().items.len(), 2);
}

#[test]
fn ids_are_sanitized_the_same_way_on_both_sides() {
    assert_eq!(
        sanitize_id("functions.read_file:0"),
        "functions_read_file_0"
    );
    assert_eq!(sanitize_id("toolu_01-A"), "toolu_01-A");
    assert_eq!(sanitize_id(""), "call");
}
