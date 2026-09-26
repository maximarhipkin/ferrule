//! Streaming (M27): SSE fixtures for the three drivers, in each API's
//! documented event format, served from 127.0.0.1 in small pieces (some
//! cutting an event in half), plus the non-streaming fallback and the
//! failures a stream can end in.

use crate::common::mock::{body, event, ok, serve, sse, sse_cut, status};
use crate::{AnthropicProvider, DriverOptions, OpenAiCompatProvider, ResponsesProvider};
use ferrule_core::error::{CoreError, FailureClass};
use ferrule_core::message::Message;
use ferrule_core::provider::{CompletionRequest, Delta, DeltaSink, Provider};
use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// A sink that keeps what it got.
fn sink() -> (DeltaSink, Arc<Mutex<Vec<Delta>>>) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let keep = got.clone();
    (DeltaSink::new(move |d| keep.lock().unwrap().push(d)), got)
}

fn texts(got: &Arc<Mutex<Vec<Delta>>>) -> Vec<String> {
    got.lock()
        .unwrap()
        .iter()
        .filter_map(|d| match d {
            Delta::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn progress(got: &Arc<Mutex<Vec<Delta>>>) -> usize {
    got.lock()
        .unwrap()
        .iter()
        .filter(|d| **d == Delta::Progress)
        .count()
}

fn req(stream: Option<DeltaSink>) -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::system("sys"), Message::user("read a.txt")],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
        max_output_tokens: None,
        temperature: None,
        stream,
    }
}

/// `text` cut into pieces of `n` bytes: events split across chunks.
fn pieces(text: &str, n: usize) -> Vec<String> {
    text.as_bytes()
        .chunks(n)
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect()
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn assert_server_transient(err: &CoreError, words: &str) {
    assert!(err.is_transient(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Server, "{err:?}");
    assert!(err.to_string().contains(words), "{err:?}");
}

// ---- Chat Completions ----

fn chat(url: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new("test", url, "sk", "kimi-k2.6")
}

fn chat_stream() -> String {
    let chunk = |delta: Value, finish: Value| {
        event(
            "",
            &json!({"id": "c1", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}),
        )
    };
    [
        chunk(
            json!({"role": "assistant", "reasoning_content": "look first"}),
            Value::Null,
        ),
        chunk(json!({"content": "Let me "}), Value::Null),
        chunk(json!({"content": "read it."}), Value::Null),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"pa"}}]}),
            Value::Null,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\": \"a.txt\"}"}}]}),
            Value::Null,
        ),
        chunk(json!({}), json!("tool_calls")),
        event(
            "",
            &json!({"id": "c1", "choices": [], "usage": {"prompt_tokens": 100,
                "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 80}}}),
        ),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

#[tokio::test]
async fn chat_streams_text_and_rebuilds_a_split_tool_call() {
    let wire = pieces(&chat_stream(), 37);
    let (url, seen) = serve(vec![sse(&refs(&wire))]);
    let (s, got) = sink();
    let resp = chat(&url).complete(req(Some(s))).await.unwrap();

    assert_eq!(texts(&got), ["Let me ", "read it."]);
    assert!(
        progress(&got) >= 3,
        "reasoning and argument bytes are progress"
    );
    assert_eq!(resp.message.content.as_deref(), Some("Let me read it."));
    assert_eq!(resp.message.reasoning.as_deref(), Some("look first"));
    assert_eq!(resp.message.tool_calls.len(), 1);
    assert_eq!(resp.message.tool_calls[0].id, "call_1");
    assert_eq!(
        resp.message.tool_calls[0].arguments,
        json!({"path": "a.txt"})
    );
    assert_eq!(resp.usage.input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 20);
    assert_eq!(resp.usage.cached_input_tokens, 80);

    let sent = body(&seen.join().unwrap()[0]);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn chat_without_a_sink_sends_todays_request() {
    let (url, seen) = serve(vec![ok(
        r#"{"choices": [{"message": {"content": "hi"}}], "usage": {}}"#,
    )]);
    chat(&url).complete(req(None)).await.unwrap();
    let sent = body(&seen.join().unwrap()[0]);
    assert!(sent.get("stream").is_none(), "{sent}");
    assert!(sent.get("stream_options").is_none(), "{sent}");
}

#[tokio::test]
async fn a_server_that_ignores_stream_answers_in_plain_json() {
    let (url, _seen) = serve(vec![ok(
        r#"{"choices": [{"message": {"content": "plain"}}], "usage": {"prompt_tokens": 3, "completion_tokens": 1}}"#,
    )]);
    let (s, got) = sink();
    let resp = chat(&url).complete(req(Some(s))).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("plain"));
    assert_eq!(resp.usage.input_tokens, 3);
    assert!(texts(&got).is_empty());
}

#[tokio::test]
async fn a_server_that_refuses_to_stream_is_asked_again_plainly() {
    let (url, seen) = serve(vec![
        status(
            "400 Bad Request",
            "content-type: application/json\r\n",
            r#"{"error": {"message": "unknown field stream_options"}}"#,
        ),
        ok(r#"{"choices": [{"message": {"content": "plain"}}], "usage": {}}"#),
    ]);
    let (s, _got) = sink();
    let resp = chat(&url).complete(req(Some(s))).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("plain"));
    let seen = seen.join().unwrap();
    assert_eq!(body(&seen[0])["stream"], true);
    assert!(body(&seen[1]).get("stream").is_none());
}

#[tokio::test]
async fn chat_stream_failures_are_classified() {
    // Cut mid-way: the connection promised more.
    let whole = chat_stream();
    let half = &whole[..whole.len() / 2];
    let (url, _s) = serve(vec![sse_cut(&[half])]);
    let err = chat(&url).complete(req(Some(sink().0))).await.unwrap_err();
    assert_server_transient(&err, "stream broke after");

    // Closed cleanly, but with no `[DONE]` and no finish reason.
    let early = event(
        "",
        &json!({"choices": [{"index": 0, "delta": {"content": "hal"}}]}),
    );
    let (url, _s) = serve(vec![sse(&[&early])]);
    let err = chat(&url).complete(req(Some(sink().0))).await.unwrap_err();
    assert_server_transient(&err, "stream ended early");

    // An upstream's rate limit passed on inside the stream.
    let limited = event("", &json!({"error": {"code": 429, "message": "slow down"}}));
    let (url, _s) = serve(vec![sse(&[&limited])]);
    let err = chat(&url).complete(req(Some(sink().0))).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    assert_eq!(err.class(), FailureClass::RateLimited, "{err:?}");

    // A non-2xx before any event: exactly as without streaming.
    let (url, _s) = serve(vec![status(
        "429 Too Many Requests",
        "retry-after: 7\r\ncontent-type: application/json\r\n",
        r#"{"error": {"message": "slow down"}}"#,
    )]);
    let err = chat(&url).complete(req(Some(sink().0))).await.unwrap_err();
    assert!(
        matches!(&err, CoreError::Transient { retry_after: Some(d), .. } if d.as_secs() == 7),
        "{err:?}"
    );
}

// ---- Anthropic Messages ----

fn anthropic(url: &str) -> AnthropicProvider {
    AnthropicProvider::new(
        "anthropic",
        url,
        "k",
        "claude-sonnet-5",
        DriverOptions::default(),
    )
}

fn anthropic_stream() -> String {
    let e = |name: &str, data: Value| event(name, &data);
    [
        e("message_start", json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
            "content": [], "stop_reason": null,
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5,
                      "cache_read_input_tokens": 100, "output_tokens": 1}}})),
        e("content_block_start", json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "Read it "}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "first."}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "signature_delta", "signature": "sig=="}})),
        e("content_block_stop", json!({"type": "content_block_stop", "index": 0})),
        ": a comment line\n\n".to_string(),
        e("ping", json!({"type": "ping"})),
        e("content_block_start", json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "text", "text": ""}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "text_delta", "text": "Reading "}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "text_delta", "text": "a.txt."}})),
        e("content_block_stop", json!({"type": "content_block_stop", "index": 1})),
        e("content_block_start", json!({"type": "content_block_start", "index": 2,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\": "}})),
        e("content_block_delta", json!({"type": "content_block_delta", "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}})),
        e("content_block_stop", json!({"type": "content_block_stop", "index": 2})),
        e("message_delta", json!({"type": "message_delta",
            "delta": {"stop_reason": "tool_use", "stop_sequence": null},
            "usage": {"output_tokens": 42}})),
        e("message_stop", json!({"type": "message_stop"})),
    ]
    .concat()
    // CRLF line endings, as some proxies re-emit them.
    .replace('\n', "\r\n")
}

#[tokio::test]
async fn anthropic_rebuilds_blocks_and_keeps_the_cache_counts() {
    let wire = pieces(&anthropic_stream(), 53);
    let (url, seen) = serve(vec![sse(&refs(&wire))]);
    let (s, got) = sink();
    let resp = anthropic(&url).complete(req(Some(s))).await.unwrap();

    assert_eq!(texts(&got), ["Reading ", "a.txt."]);
    assert_eq!(resp.message.content.as_deref(), Some("Reading a.txt."));
    assert_eq!(resp.message.reasoning.as_deref(), Some("Read it first."));
    assert_eq!(resp.message.tool_calls[0].id, "toolu_1");
    assert_eq!(
        resp.message.tool_calls[0].arguments,
        json!({"path": "a.txt"})
    );
    // The thinking block, signature and all, is kept for the loop's replay.
    let native = resp.message.native.as_ref().unwrap();
    assert_eq!(
        native.items[0],
        json!({"type": "thinking", "thinking": "Read it first.", "signature": "sig=="})
    );
    assert_eq!(native.items.len(), 3);
    // Input from `message_start`, output from `message_delta`.
    assert_eq!(resp.usage.input_tokens, 115);
    assert_eq!(resp.usage.cached_input_tokens, 100);
    assert_eq!(resp.usage.cache_write_input_tokens, 5);
    assert_eq!(resp.usage.output_tokens, 42);

    let sent = body(&seen.join().unwrap()[0]);
    assert_eq!(sent["stream"], true);
}

#[tokio::test]
async fn anthropic_streamed_and_plain_calls_parse_the_same() {
    let plain = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
        "content": [
            {"type": "thinking", "thinking": "Read it first.", "signature": "sig=="},
            {"type": "text", "text": "Reading a.txt."},
            {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.txt"}},
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5,
                  "cache_read_input_tokens": 100, "output_tokens": 42},
    });
    let (url, seen) = serve(vec![ok(plain.to_string()), sse(&[&anthropic_stream()])]);
    let a = anthropic(&url).complete(req(None)).await.unwrap();
    let b = anthropic(&url).complete(req(Some(sink().0))).await.unwrap();
    let json = |v: &ferrule_core::provider::CompletionResponse| {
        serde_json::to_value((&v.message, &v.usage)).unwrap()
    };
    assert_eq!(json(&a), json(&b));
    let seen = seen.join().unwrap();
    assert!(body(&seen[0]).get("stream").is_none());
    // Otherwise the two requests are the same bytes.
    let mut streamed = body(&seen[1]);
    streamed.as_object_mut().unwrap().remove("stream");
    assert_eq!(body(&seen[0]), streamed);
}

#[tokio::test]
async fn anthropic_stream_errors_class_like_their_status() {
    let start = event(
        "message_start",
        &json!({"type": "message_start", "message": {"content": [], "usage": {"input_tokens": 1}}}),
    );
    for (kind, class) in [
        ("overloaded_error", FailureClass::Overloaded),
        ("rate_limit_error", FailureClass::RateLimited),
        ("api_error", FailureClass::Server),
    ] {
        let err_event = event(
            "error",
            &json!({"type": "error", "error": {"type": kind, "message": "try later"}}),
        );
        let (url, _s) = serve(vec![sse(&[&start, &err_event])]);
        let err = anthropic(&url)
            .complete(req(Some(sink().0)))
            .await
            .unwrap_err();
        assert!(err.is_transient(), "{kind}: {err:?}");
        assert_eq!(err.class(), class, "{kind}: {err:?}");
    }

    // No `message_stop`.
    let (url, _s) = serve(vec![sse(&[&start])]);
    let err = anthropic(&url)
        .complete(req(Some(sink().0)))
        .await
        .unwrap_err();
    assert_server_transient(&err, "stream ended early");

    // Dropped mid-way.
    let (url, _s) = serve(vec![sse_cut(&[&start])]);
    let err = anthropic(&url)
        .complete(req(Some(sink().0)))
        .await
        .unwrap_err();
    assert_server_transient(&err, "stream broke after 1 events");
}

// ---- Responses ----

fn responses(url: &str) -> ResponsesProvider {
    ResponsesProvider::new("openai", url, "k", "gpt-5.5", DriverOptions::default())
}

fn final_response(status: &str) -> Value {
    json!({
        "id": "resp_1", "object": "response", "status": status, "model": "gpt-5.5",
        "output": [
            {"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "Hello there.", "annotations": []}]},
        ],
        "usage": {"input_tokens": 50, "input_tokens_details": {"cached_tokens": 40},
                  "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 0},
                  "total_tokens": 57},
    })
}

fn responses_stream(last: &str, response: Value) -> String {
    let e = |name: &str, data: Value| event(name, &data);
    [
        e(
            "response.created",
            json!({"type": "response.created", "sequence_number": 0,
            "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
        ),
        e(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta",
            "sequence_number": 1, "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "Hello "}),
        ),
        e(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta",
            "sequence_number": 2, "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": "there."}),
        ),
        e(
            last,
            json!({"type": last, "sequence_number": 3, "response": response}),
        ),
    ]
    .concat()
}

#[tokio::test]
async fn responses_streams_text_and_reads_the_final_object() {
    let wire = pieces(
        &responses_stream("response.completed", final_response("completed")),
        41,
    );
    let (url, seen) = serve(vec![sse(&refs(&wire))]);
    let (s, got) = sink();
    let resp = responses(&url).complete(req(Some(s))).await.unwrap();
    assert_eq!(texts(&got), ["Hello ", "there."]);
    assert_eq!(resp.message.content.as_deref(), Some("Hello there."));
    assert_eq!(resp.usage.input_tokens, 50);
    assert_eq!(resp.usage.cached_input_tokens, 40);
    assert_eq!(resp.usage.output_tokens, 7);
    assert_eq!(body(&seen.join().unwrap()[0])["stream"], true);
}

#[tokio::test]
async fn responses_stream_endings_go_through_the_status_rules() {
    // Out of tokens: the text so far, as without streaming.
    let mut cut_short = final_response("incomplete");
    cut_short["incomplete_details"] = json!({"reason": "max_output_tokens"});
    let (url, _s) = serve(vec![sse(&[&responses_stream(
        "response.incomplete",
        cut_short,
    )])]);
    let resp = responses(&url).complete(req(Some(sink().0))).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("Hello there."));

    // Failed, with a server error.
    let mut failed = final_response("failed");
    failed["output"] = json!([]);
    failed["error"] = json!({"code": "server_error", "message": "boom"});
    let (url, _s) = serve(vec![sse(&[&responses_stream("response.failed", failed)])]);
    let err = responses(&url)
        .complete(req(Some(sink().0)))
        .await
        .unwrap_err();
    assert!(err.is_transient(), "{err:?}");

    // An error event: a rate limit.
    let limited = event(
        "error",
        &json!({"type": "error", "code": "rate_limit_exceeded", "message": "slow down"}),
    );
    let (url, _s) = serve(vec![sse(&[&limited])]);
    let err = responses(&url)
        .complete(req(Some(sink().0)))
        .await
        .unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    assert_eq!(err.class(), FailureClass::RateLimited, "{err:?}");

    // No final event.
    let whole = responses_stream("response.completed", final_response("completed"));
    let head = &whole[..whole.find("event: response.completed").unwrap()];
    let (url, _s) = serve(vec![sse(&[head])]);
    let err = responses(&url)
        .complete(req(Some(sink().0)))
        .await
        .unwrap_err();
    assert_server_transient(&err, "stream ended early, after 3 events");

    // A plain JSON reply to a streaming request.
    let (url, _s) = serve(vec![ok(final_response("completed").to_string())]);
    let resp = responses(&url).complete(req(Some(sink().0))).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("Hello there."));
}
