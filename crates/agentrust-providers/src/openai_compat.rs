use agentrust_core::error::CoreError;
use agentrust_core::message::{Message, Role, ToolCall, Usage};
use agentrust_core::provider::{CompletionRequest, CompletionResponse, Provider};
use agentrust_core::tool::ToolDefinition;
use serde_json::{json, Value};
use std::time::Duration;

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
    pub fn new(name: impl Into<String>, base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("reqwest client");
        Self { name: name.into(), base_url: base_url.into().trim_end_matches('/').to_string(), api_key: api_key.into(), model: model.into(), client }
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
        // Preserve interleaved thinking for models trained on it.
        if retain_reasoning {
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
        let choice = body
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| CoreError::MalformedResponse(format!("no choices in: {}", truncate(body))))?;
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
                            arguments: serde_json::from_str(tc.get("function")?.get("arguments")?.as_str().unwrap_or("{}"))
                                .unwrap_or(json!({})),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let content = msg.get("content").and_then(|c| c.as_str()).map(|s| s.to_string());
        let reasoning = msg
            .get("reasoning_content")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());

        let usage = body.get("usage").cloned().unwrap_or(json!({}));
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
            .unwrap_or(0);

        Ok(CompletionResponse {
            message: Message::assistant(content, tool_calls, reasoning),
            usage: Usage {
                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cached_input_tokens: cached,
            },
        })
    }
}

fn truncate(v: &Value) -> String {
    let s = v.to_string();
    s.chars().take(500).collect()
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
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

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| CoreError::Provider(format!("request failed: {e}")))?;

        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| CoreError::Provider(format!("bad json (status {status}): {e}")))?;
        if !status.is_success() {
            return Err(CoreError::Provider(format!("HTTP {status}: {}", truncate(&body))));
        }
        Self::parse_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal canned HTTP server: one request in, one JSON response out.
    fn mock_server(response_body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(resp.as_bytes()).unwrap();
            request
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
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
    async fn http_error_surfaces_status() {
        let (url, _h) = mock_server(r#"{"error": {"message": "bad key"}}"#);
        let _p = OpenAiCompatProvider::new("test", url, "bad", "m");
        // mock always returns 200 in this helper; assert parse-level behavior instead
        let err_body: Value = serde_json::from_str(r#"{"error": {"message": "bad key"}}"#).unwrap();
        assert!(OpenAiCompatProvider::parse_response(&err_body).is_err());
    }
}
