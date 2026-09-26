//! Hand-written fixtures in the Responses API's documented wire format,
//! served from 127.0.0.1.

use super::*;
use crate::common::mock::{self, body, ok, serve, status};
use ferrule_core::error::FailureClass;
use std::time::Duration;

const MODEL: &str = "gpt-5.5";

fn provider(url: &str) -> ResponsesProvider {
    ResponsesProvider::new("openai", url, "sk-test", MODEL, DriverOptions::default())
}

fn with(options: DriverOptions) -> ResponsesProvider {
    ResponsesProvider::new("openai", "http://unused/v1", "k", MODEL, options)
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("{name} a file"),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
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

const TWO_CALLS: &str = r#"{
  "id": "resp_01", "object": "response", "status": "completed", "model": "gpt-5.5",
  "output": [
    {"id": "rs_01", "type": "reasoning", "summary": [{"type": "summary_text", "text": "Need both."}],
     "encrypted_content": "gAAAAABo-ENCRYPTED"},
    {"id": "fc_01", "type": "function_call", "status": "completed", "call_id": "call_A",
     "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"},
    {"id": "fc_02", "type": "function_call", "status": "completed", "call_id": "call_B",
     "name": "list_dir", "arguments": "{\"path\":\"src\"}"}
  ],
  "usage": {"input_tokens": 2100, "input_tokens_details": {"cached_tokens": 1920},
            "output_tokens": 90, "output_tokens_details": {"reasoning_tokens": 64},
            "total_tokens": 2190},
  "error": null, "incomplete_details": null
}"#;

const DONE: &str = r#"{
  "id": "resp_02", "object": "response", "status": "completed", "model": "gpt-5.5",
  "output": [
    {"id": "msg_02", "type": "message", "role": "assistant", "status": "completed",
     "content": [{"type": "output_text", "text": "All ", "annotations": []},
                 {"type": "output_text", "text": "done.", "annotations": []}]}
  ],
  "usage": {"input_tokens": 2300, "input_tokens_details": {"cached_tokens": 2048},
            "output_tokens": 5},
  "error": null
}"#;

#[tokio::test]
async fn the_request_is_stateless_and_carries_no_temperature() {
    let (url, seen) = serve(vec![ok(DONE)]);
    let mut r = req(vec![
        Message::system("You are Ferrule."),
        Message::system("Recalled: tests."),
        Message::user("hi"),
        Message::system("[hook] note"),
    ]);
    r.temperature = Some(0.2);
    r.max_output_tokens = Some(512);
    let resp = provider(&url).complete(r).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("All done."));

    let raw = seen.join().unwrap().remove(0);
    let head = raw.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/responses "), "{head}");
    assert!(head.contains("authorization: bearer sk-test"));
    let b = body(&raw);
    assert_eq!(b["store"], false);
    assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
    assert!(b.get("previous_response_id").is_none());
    assert!(b.get("temperature").is_none());
    assert!(b.get("reasoning").is_none(), "the model's default effort");
    assert_eq!(b["instructions"], "You are Ferrule.\n\nRecalled: tests.");
    assert_eq!(
        b["input"],
        json!([
            {"role": "user", "content": "hi"},
            {"role": "developer", "content": "[hook] note"}
        ])
    );
    assert_eq!(b["max_output_tokens"], 16_000);
    assert_eq!(b["parallel_tool_calls"], true);
    assert_eq!(
        b["tools"][1],
        json!({"type": "function", "name": "list_dir", "description": "list_dir a file",
               "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
               "strict": false})
    );
}

#[test]
fn effort_goes_out_and_none_keeps_the_callers_cap() {
    let mut r = req(vec![Message::user("hi")]);
    let b = with(DriverOptions {
        effort: Some("high".into()),
        ..Default::default()
    })
    .payload(&r, false)
    .body;
    assert_eq!(b["reasoning"], json!({"effort": "high"}));
    assert!(b.get("max_output_tokens").is_none(), "unset stays unset");

    r.max_output_tokens = Some(256);
    let b = with(DriverOptions {
        effort: Some("none".into()),
        ..Default::default()
    })
    .payload(&r, false)
    .body;
    assert_eq!(b["max_output_tokens"], 256);
}

#[tokio::test]
async fn a_multi_call_turn_and_its_reasoning_go_back_within_the_loop() {
    let (url, seen) = serve(vec![ok(TWO_CALLS), ok(DONE)]);
    let p = provider(&url);
    let mut history = vec![Message::system("sys"), Message::user("look around")];
    let resp = p.complete(req(history.clone())).await.unwrap();
    let calls = &resp.message.tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(
        (calls[0].id.as_str(), calls[0].name.as_str()),
        ("call_A", "read_file")
    );
    assert_eq!(calls[1].arguments, json!({"path": "src"}));
    assert_eq!(resp.message.reasoning.as_deref(), Some("Need both."));
    // Input includes the cached part; nothing is written.
    assert_eq!(resp.usage.input_tokens, 2100);
    assert_eq!(resp.usage.cached_input_tokens, 1920);
    assert_eq!(resp.usage.cache_write_input_tokens, 0);
    assert_eq!(resp.usage.output_tokens, 90);
    let debug = format!("{:?}", resp.message);
    assert!(!debug.contains("ENCRYPTED"), "{debug}");

    history.push(resp.message);
    history.push(Message::tool_result("call_A", "fn a() {}"));
    history.push(Message::tool_result("call_B", "a.rs"));
    p.complete(req(history.clone())).await.unwrap();

    let second = body(&seen.join().unwrap()[1]);
    let output: Value = serde_json::from_str::<Value>(TWO_CALLS).unwrap()["output"].clone();
    let mut expected = vec![json!({"role": "user", "content": "look around"})];
    expected.extend(output.as_array().unwrap().iter().cloned());
    expected
        .push(json!({"type": "function_call_output", "call_id": "call_A", "output": "fn a() {}"}));
    expected.push(json!({"type": "function_call_output", "call_id": "call_B", "output": "a.rs"}));
    assert_eq!(second["input"], json!(expected));

    // After the next user message: neutral items, no reasoning, no ids.
    history.push(Message::assistant(Some("done".into()), vec![], None));
    history.push(Message::user("and b.rs?"));
    let later = with(DriverOptions::default()).payload(&req(history), false);
    assert!(!later.replayed);
    assert_eq!(
        later.body["input"][1],
        json!({"type": "function_call", "call_id": "call_A", "name": "read_file",
               "arguments": "{\"path\":\"a.rs\"}"})
    );
    assert!(!later.body.to_string().contains("ENCRYPTED"));
}

#[test]
fn reasoning_without_encrypted_content_is_not_sent_back() {
    let p = with(DriverOptions::default());
    let resp = p
        .parse_response(&json!({
            "status": "completed",
            "output": [
                {"id": "rs_9", "type": "reasoning", "summary": []},
                {"type": "function_call", "call_id": "c1", "name": "read_file", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap();
    let history = vec![
        Message::user("go"),
        resp.message,
        Message::tool_result("c1", "x"),
    ];
    let b = p.payload(&req(history), false);
    assert!(b.replayed);
    let input = b.body["input"].as_array().unwrap();
    assert!(input.iter().all(|i| i["type"] != "reasoning"), "{input:?}");
    assert_eq!(input[1]["call_id"], "c1");
}

#[tokio::test]
async fn a_rejected_replay_is_retried_once_without_reasoning() {
    let bad = r#"{"error":{"message":"Item with id 'rs_01' not found. Items are not persisted when `store` is set to false.","type":"invalid_request_error","code":null}}"#;
    let (url, seen) = serve(vec![
        status("404 Not Found", "content-type: application/json\r\n", bad),
        ok(DONE),
    ]);
    let p = provider(&url);
    let first = p
        .parse_response(&serde_json::from_str(TWO_CALLS).unwrap())
        .unwrap();
    let history = vec![
        Message::user("look"),
        first.message,
        Message::tool_result("call_A", "x"),
        Message::tool_result("call_B", "y"),
    ];
    p.complete(req(history)).await.unwrap();
    let seen = seen.join().unwrap();
    assert!(body(&seen[0]).to_string().contains("ENCRYPTED"));
    assert!(!body(&seen[1]).to_string().contains("ENCRYPTED"));
}

async fn error_for(reply: mock::Canned) -> CoreError {
    let (url, _seen) = serve(vec![reply]);
    provider(&url)
        .complete(req(vec![Message::user("hi")]))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn errors_map_to_retries_fallback_and_classes() {
    let e = error_for(status(
        "429 Too Many Requests",
        "content-type: application/json\r\nretry-after: 3\r\n",
        r#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#,
    ))
    .await;
    assert!(
        matches!(&e, CoreError::Transient { retry_after: Some(d), .. } if *d == Duration::from_secs(3)),
        "{e:?}"
    );
    assert_eq!(e.class(), FailureClass::RateLimited);

    let e = error_for(status(
        "400 Bad Request",
        "content-type: application/json\r\n",
        r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
    ))
    .await;
    assert!(!e.is_transient());
    assert_eq!(e.class(), FailureClass::ContextTooLong);

    // An error object in a 200 (a gateway passing an upstream failure on).
    let e = error_for(ok(
        r#"{"error":{"message":"upstream overloaded","code":"server_error"},"output":[]}"#,
    ))
    .await;
    assert!(e.is_transient(), "{e:?}");

    // `status: failed` with its error.
    let e = error_for(ok(r#"{
      "status": "failed", "output": [],
      "error": null,
      "usage": {"input_tokens": 0, "output_tokens": 0}
    }"#))
    .await;
    assert!(!e.is_transient());
}

#[tokio::test]
async fn incomplete_returns_the_text_so_far_or_says_why() {
    let (url, _seen) = serve(vec![ok(r#"{
      "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
      "output": [{"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "The first half"}]}],
      "usage": {"input_tokens": 10, "output_tokens": 16000}
    }"#)]);
    let resp = provider(&url)
        .complete(req(vec![Message::user("write a lot")]))
        .await
        .unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("The first half"));

    let e = error_for(ok(r#"{
      "status": "incomplete", "incomplete_details": {"reason": "content_filter"},
      "output": [], "usage": {"input_tokens": 10, "output_tokens": 0}
    }"#))
    .await;
    assert_eq!(e.class(), FailureClass::Refused);
}

#[tokio::test]
async fn a_refusal_part_is_final() {
    let e = error_for(ok(r#"{
      "status": "completed",
      "output": [{"type": "message", "role": "assistant",
                  "content": [{"type": "refusal", "refusal": "I can't help with that."}]}],
      "usage": {"input_tokens": 10, "output_tokens": 8}
    }"#))
    .await;
    assert!(!e.is_transient());
    assert_eq!(e.class(), FailureClass::Refused);
    assert!(e.to_string().contains("I can't help with that."), "{e}");
}
