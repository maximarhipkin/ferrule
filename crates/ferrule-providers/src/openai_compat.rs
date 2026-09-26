use crate::common::{self, truncate};
use ferrule_core::error::CoreError;
use ferrule_core::error::FailureClass;
use ferrule_core::message::{Message, Role, ToolCall, Usage};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Delta, DeltaSink, Provider};
use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Value};

/// OpenAI-compatible chat-completions driver. Handles the dialect details:
/// tool schemas as `function` objects, arguments as JSON strings, and
/// reasoning content under `reasoning_content` (Kimi K2 Thinking, DeepSeek).
pub struct OpenAiCompatProvider {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let client = common::client();
        Self {
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client,
        }
    }

    fn to_wire(msg: &Message, retain_reasoning: bool) -> Value {
        let mut m = json!({ "role": match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }});
        if let Some(c) = &msg.content {
            m["content"] = json!(c);
        } else if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
            m["content"] = Value::Null;
        }
        if !msg.tool_calls.is_empty() {
            m["tool_calls"] = json!(msg
                .tool_calls
                .iter()
                .map(|tc| json!({
                    "id": tc.id,
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments.to_string() }
                }))
                .collect::<Vec<_>>());
        }
        if let Some(id) = &msg.tool_call_id {
            m["tool_call_id"] = json!(id);
        }
        // Preserve interleaved thinking for models trained on it — but not
        // another driver's (a native Anthropic or Responses turn carries
        // `native`; its thinking was never this model's, M23).
        if retain_reasoning && msg.native.is_none() {
            if let Some(r) = &msg.reasoning {
                m["reasoning_content"] = json!(r);
            }
        }
        m
    }

    fn tools_to_wire(tools: &[ToolDefinition]) -> Value {
        json!(tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .collect::<Vec<_>>())
    }

    fn parse_response(body: &Value) -> Result<CompletionResponse, CoreError> {
        let choice = body.get("choices").and_then(|c| c.get(0)).ok_or_else(|| {
            CoreError::MalformedResponse(format!("no choices in: {}", truncate(body)))
        })?;
        let msg = &choice["message"];

        let tool_calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        Some(ToolCall {
                            id: tc.get("id")?.as_str()?.to_string(),
                            name: tc.get("function")?.get("name")?.as_str()?.to_string(),
                            arguments: serde_json::from_str(
                                tc.get("function")?
                                    .get("arguments")?
                                    .as_str()
                                    .unwrap_or("{}"),
                            )
                            .unwrap_or(json!({})),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
        let reasoning = msg
            .get("reasoning_content")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());

        let usage = body.get("usage").cloned().unwrap_or(json!({}));
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .or_else(|| {
                usage
                    .get("prompt_cache_hit_tokens")
                    .and_then(|v| v.as_u64())
            })
            .unwrap_or(0);

        Ok(CompletionResponse {
            message: Message::assistant(content, tool_calls, reasoning),
            usage: Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cached_input_tokens: cached,
                cache_write_input_tokens: 0,
            },
        })
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let payload = self.payload(&req);
        if let Some(sink) = &req.stream {
            match self.stream(&payload, sink).await {
                // A compatible server that won't stream, or won't take
                // `stream_options`: once more, plainly.
                Err(e) if e.class() == FailureClass::BadRequest => {
                    tracing::debug!(provider = %self.name, "streaming refused ({e}), asking plainly");
                }
                done => return done,
            }
        }
        let reply = common::send(self.post(&payload)).await?;
        Self::read_reply(reply)
    }
}

impl OpenAiCompatProvider {
    fn payload(&self, req: &CompletionRequest) -> Value {
        let mut payload = json!({
            "model": self.model,
            "messages": req.messages.iter().map(|m| Self::to_wire(m, true)).collect::<Vec<_>>(),
        });
        if !req.tools.is_empty() {
            payload["tools"] = Self::tools_to_wire(&req.tools);
            payload["tool_choice"] = json!("auto");
        }
        if let Some(t) = req.temperature {
            payload["temperature"] = json!(t);
        }
        if let Some(m) = req.max_output_tokens {
            payload["max_tokens"] = json!(m);
        }
        payload
    }

    fn post(&self, payload: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(payload)
    }

    fn read_reply(reply: common::Reply) -> Result<CompletionResponse, CoreError> {
        if let Some(err) = reply.body.get("error").filter(|e| !e.is_null()) {
            return Err(common::error_in_body(err, reply.wait));
        }
        Self::parse_response(&reply.body)
    }

    /// The streaming call: the chunks reassembled into the body a plain
    /// call returns, then parsed by the same code.
    async fn stream(
        &self,
        payload: &Value,
        sink: &DeltaSink,
    ) -> Result<CompletionResponse, CoreError> {
        let mut payload = payload.clone();
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({"include_usage": true});
        let mut events = match common::open(self.post(&payload)).await? {
            common::Opened::Json(reply) => return Self::read_reply(reply),
            common::Opened::Events(events) => events,
        };
        let mut chat = ChatStream::default();
        while let Some(event) = events.next().await? {
            if event.data.trim() == "[DONE]" {
                chat.done = true;
                break;
            }
            chat.take(&event.json()?, sink)?;
        }
        // Some servers close after the last chunk without `[DONE]`; a
        // finish reason says the reply was whole.
        if !chat.done && chat.finish.is_none() {
            return Err(common::ended_early(events.seen()));
        }
        if chat.usage.is_null() {
            tracing::warn!(provider = %self.name, "the stream carried no usage: recording zeros");
        }
        Self::parse_response(&chat.body())
    }
}

/// A Chat Completions stream as it comes in.
#[derive(Default)]
struct ChatStream {
    text: String,
    reasoning: String,
    /// By the chunk's `index`: id, name, arguments so far.
    calls: std::collections::BTreeMap<u64, (String, String, String)>,
    finish: Option<String>,
    usage: Value,
    done: bool,
}

impl ChatStream {
    fn take(&mut self, chunk: &Value, sink: &DeltaSink) -> Result<(), CoreError> {
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(common::error_in_body(err, None));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = usage.clone();
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.text.push_str(text);
                sink.send(Delta::Text(text.to_string()));
            }
        }
        let thought = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|k| delta.get(*k).and_then(Value::as_str));
        if let Some(r) = thought {
            self.reasoning.push_str(r);
            sink.send(Delta::Progress);
        }
        for tc in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
            let call = self.calls.entry(index).or_default();
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                call.0 = id.to_string();
            }
            let f = &tc["function"];
            if let Some(name) = f.get("name").and_then(Value::as_str) {
                call.1.push_str(name);
            }
            if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                call.2.push_str(args);
            }
            sink.send(Delta::Progress);
        }
        Ok(())
    }

    /// The body a plain call would have returned.
    fn body(self) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": (!self.text.is_empty()).then_some(self.text),
        });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = json!(self
                .calls
                .into_values()
                .map(|(id, name, args)| json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": if args.is_empty() { "{}".to_string() } else { args }},
                }))
                .collect::<Vec<_>>());
        }
        json!({
            "choices": [{"message": message, "finish_reason": self.finish}],
            "usage": self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// Minimal canned HTTP server: one request in, one JSON response out.
    fn mock_server(response_body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        mock_response(
            "200 OK",
            "content-type: application/json\r\n",
            response_body,
        )
    }

    #[test]
    fn another_drivers_thinking_is_not_replayed_as_reasoning_content() {
        let own = Message::assistant(Some("a".into()), vec![], Some("mine".into()));
        assert_eq!(
            OpenAiCompatProvider::to_wire(&own, true)["reasoning_content"],
            "mine"
        );
        // A turn a native driver served (M21 fell back mid-conversation).
        let foreign = own.clone().with_native(ferrule_core::NativeBlocks {
            api: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            items: vec![json!({"type": "thinking", "thinking": "mine", "signature": "s"})],
        });
        let wire = OpenAiCompatProvider::to_wire(&foreign, true);
        assert!(wire.get("reasoning_content").is_none(), "{wire}");
        assert_eq!(wire["content"], "a");
    }

    /// One request in, the given status line, extra headers and body out.
    fn mock_response(
        status: &'static str,
        headers: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!("HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            stream.write_all(resp.as_bytes()).unwrap();
            request
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_output_tokens: None,
            temperature: None,
            stream: None,
        }
    }

    async fn error_for(
        status: &'static str,
        headers: &'static str,
        body: &'static str,
    ) -> CoreError {
        let (url, _h) = mock_response(status, headers, body);
        OpenAiCompatProvider::new("test", url, "sk", "m")
            .complete(request())
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn sends_tools_and_parses_tool_call() {
        let body = r#"{
            "choices": [{"message": {"content": null, "reasoning_content": "let me think",
                "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\": \"a.txt\"}"}}]}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 80}}
        }"#;
        let (url, handle) = mock_server(body);
        let p = OpenAiCompatProvider::new("test", url, "sk-test", "kimi-k2.6");

        let req = CompletionRequest {
            messages: vec![Message::system("sys"), Message::user("read a.txt")],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            max_output_tokens: None,
            temperature: None,
            stream: None,
        };
        let resp = p.complete(req).await.unwrap();
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].name, "read_file");
        assert_eq!(resp.message.tool_calls[0].arguments["path"], "a.txt");
        assert_eq!(resp.message.reasoning.as_deref(), Some("let me think"));
        assert_eq!(resp.usage.cached_input_tokens, 80);

        let request_text = handle.join().unwrap().to_lowercase();
        assert!(request_text.contains("authorization: bearer sk-test"));
        assert!(request_text.contains("\"model\":\"kimi-k2.6\""));
        assert!(request_text.contains("\"read_file\""));
    }

    #[tokio::test]
    async fn a_client_error_is_final() {
        let err = error_for(
            "401 Unauthorized",
            "content-type: application/json\r\n",
            r#"{"error": {"message": "bad key"}}"#,
        )
        .await;
        assert!(
            matches!(&err, CoreError::Provider(m) if m.contains("401") && m.contains("bad key")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rate_limits_and_server_errors_are_transient() {
        let err = error_for(
            "429 Too Many Requests",
            "retry-after: 7\r\ncontent-type: application/json\r\n",
            r#"{"error": {"message": "slow down"}}"#,
        )
        .await;
        assert!(
            matches!(&err, CoreError::Transient { retry_after: Some(d), .. } if *d == Duration::from_secs(7)),
            "{err:?}"
        );

        // A proxy's HTML error page: not JSON, still a 502.
        let err = error_for(
            "502 Bad Gateway",
            "content-type: text/html\r\n",
            "<html><h1>502 Bad Gateway</h1></html>",
        )
        .await;
        assert!(
            matches!(&err, CoreError::Transient { message, retry_after: None } if message.contains("502 Bad Gateway")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_error_object_in_a_200_is_classified_too() {
        let err = error_for(
            "200 OK",
            "",
            r#"{"error": {"code": 502, "message": "upstream provider error"}}"#,
        )
        .await;
        assert!(err.is_transient(), "{err:?}");
        let err = error_for(
            "200 OK",
            "",
            r#"{"error": {"code": 400, "message": "context too long"}}"#,
        )
        .await;
        assert!(
            matches!(&err, CoreError::Provider(m) if m.contains("context too long")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn openrouters_own_errors_come_out_in_plain_words() {
        let err = error_for(
            "404 Not Found",
            "content-type: application/json\r\n",
            r#"{"error":{"message":"No endpoints found that support tool use. To learn more about provider routing, visit: https://openrouter.ai/docs/provider-routing","code":404}}"#,
        )
        .await;
        let words = err.plain_words().unwrap_or_default();
        assert!(
            words.starts_with("this model has no endpoint on OpenRouter that supports tools"),
            "{err:?}"
        );

        let err = error_for(
            "429 Too Many Requests",
            "content-type: application/json\r\n",
            r#"{"error":{"message":"Rate limit exceeded: free-models-per-min. ","code":429,"metadata":{"headers":{"X-RateLimit-Limit":"20","X-RateLimit-Remaining":"0","X-RateLimit-Reset":"1758790860000"}}}}"#,
        )
        .await;
        assert!(err.is_transient(), "{err:?}");
        let words = err.plain_words().unwrap_or_default();
        assert!(words.contains("shared pool for free models"), "{err:?}");

        // An upstream provider's 429, passed on inside a 200.
        let err = error_for(
            "200 OK",
            "",
            r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"qwen/qwen3.8-27b:free is temporarily rate-limited upstream. Please retry shortly.","provider_name":"Chutes"}}}"#,
        )
        .await;
        assert!(err.is_transient(), "{err:?}");
        let words = err.plain_words().unwrap_or_default();
        assert!(
            words.starts_with("the model provider is rate-limiting us"),
            "{err:?}"
        );
        assert!(words.contains("shared pool"), "{err:?}");
    }

    #[tokio::test]
    async fn no_connection_is_transient() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port(); // closed again
        let p = OpenAiCompatProvider::new("test", format!("http://127.0.0.1:{port}/v1"), "sk", "m");
        let err = p.complete(request()).await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
    }
}
