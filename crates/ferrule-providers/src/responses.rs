//! The OpenAI Responses driver (M23, `api = "responses"`).
//!
//! Stateless on purpose (design §6): every call sends the full input with
//! `store: false`, never `previous_response_id`, and asks for encrypted
//! reasoning so a reasoning model's chain survives a tool loop without the
//! server keeping anything. That one path serves api.openai.com and
//! stateless-only hosts such as OpenRouter alike.

use crate::common::{self, last_user, replayable};
use crate::DriverOptions;
use ferrule_core::error::CoreError;
use ferrule_core::message::{Message, NativeBlocks, Role, ToolCall, Usage};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Provider};
use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Value};
use tracing::warn;

/// `NativeBlocks::api` for this driver.
pub const API: &str = "responses";
/// The output cap covers reasoning too: a request asking for less is
/// raised to this, unless reasoning is off (`effort = "none"`).
pub const MAX_TOKENS_FLOOR: u32 = 16_000;

pub struct ResponsesProvider {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    options: DriverOptions,
    client: reqwest::Client,
}

struct Payload {
    body: Value,
    /// Reasoning items were replayed: the retry after a 400 leaves them out.
    replayed: bool,
}

impl ResponsesProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        options: DriverOptions,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            options,
            client: common::client(),
        }
    }

    /// The request body. `plain` is the retry after a reasoning 400: no
    /// native items.
    fn payload(&self, req: &CompletionRequest, plain: bool) -> Payload {
        let msgs = &req.messages;
        let lead = msgs.iter().take_while(|m| m.role == Role::System).count();
        let instructions: Vec<&str> = msgs[..lead]
            .iter()
            .filter_map(|m| nonempty(&m.content))
            .collect();

        let latest_user = last_user(msgs);
        let mut input: Vec<Value> = Vec::new();
        let mut replayed = false;
        for (i, m) in msgs.iter().enumerate().skip(lead) {
            let in_loop = latest_user.is_none_or(|u| i > u);
            match m.role {
                Role::System => {
                    if let Some(t) = nonempty(&m.content) {
                        input.push(json!({"role": "developer", "content": t}));
                    }
                }
                Role::User => {
                    if let Some(t) = nonempty(&m.content) {
                        input.push(json!({"role": "user", "content": t}));
                    }
                }
                Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "output": m.content.as_deref().unwrap_or(""),
                })),
                Role::Assistant => {
                    let own = if plain {
                        None
                    } else {
                        replayable(m, API, &self.model, in_loop)
                    };
                    match own {
                        Some(items) => {
                            replayed = true;
                            input.extend(items.iter().filter(|i| sendable(i)).cloned());
                        }
                        None => input.extend(neutral_assistant(m)),
                    }
                }
            }
        }

        let mut body = json!({
            "model": self.model,
            "input": input,
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions.join("\n\n"));
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(tools_to_wire(&req.tools));
            body["parallel_tool_calls"] = json!(true);
        }
        if let Some(e) = &self.options.effort {
            body["reasoning"] = json!({ "effort": e });
        }
        if let Some(mut cap) = req.max_output_tokens.or(self.options.max_tokens) {
            if self.options.effort.as_deref() != Some("none") {
                cap = cap.max(MAX_TOKENS_FLOOR);
            }
            body["max_output_tokens"] = json!(cap);
        }
        // `temperature` is never sent: reasoning models reject it.
        Payload { body, replayed }
    }

    async fn post(&self, body: &Value) -> Result<Value, CoreError> {
        let reply = common::send(
            self.client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&self.api_key)
                .json(body),
        )
        .await?;
        if let Some(err) = reply.body.get("error").filter(|e| !e.is_null()) {
            return Err(common::error_in_body(err, reply.wait));
        }
        Ok(reply.body)
    }

    fn parse_response(&self, body: &Value) -> Result<CompletionResponse, CoreError> {
        let output = body
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::MalformedResponse(format!("no output in: {}", common::truncate(body)))
            })?;

        let mut text = String::new();
        let mut summary: Vec<&str> = Vec::new();
        let mut reasoned = false;
        let mut tool_calls = Vec::new();
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item["content"].as_array().into_iter().flatten() {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                text.push_str(part["text"].as_str().unwrap_or(""))
                            }
                            Some("refusal") => {
                                let why = part["refusal"].as_str().unwrap_or("unspecified");
                                return Err(CoreError::Provider(format!(
                                    "refused: {}",
                                    why.chars().take(300).collect::<String>()
                                )));
                            }
                            _ => {}
                        }
                    }
                }
                Some("function_call") => {
                    let raw = item["arguments"].as_str().unwrap_or("{}");
                    tool_calls.push(ToolCall {
                        id: item["call_id"].as_str().unwrap_or("").to_string(),
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        // Malformed JSON becomes `{}`, as on the chat driver.
                        arguments: serde_json::from_str(raw).unwrap_or(json!({})),
                    });
                }
                Some("reasoning") => {
                    reasoned = true;
                    for s in item["summary"].as_array().into_iter().flatten() {
                        if let Some(t) = s["text"].as_str().filter(|t| !t.is_empty()) {
                            summary.push(t);
                        }
                    }
                }
                _ => {}
            }
        }

        match body.get("status").and_then(Value::as_str) {
            Some("failed") => {
                let err = body.get("error").cloned().unwrap_or(json!({}));
                return Err(common::error_in_body(&err, None));
            }
            Some("incomplete") => {
                let reason = body
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified");
                // Out of tokens: the text so far, like `max_tokens`.
                if reason == "content_filter" {
                    return Err(CoreError::Provider(format!("refused: {reason}")));
                }
                if reason != "max_output_tokens" {
                    return Err(CoreError::Provider(format!(
                        "response incomplete: {reason}"
                    )));
                }
            }
            _ => {}
        }

        let usage = body.get("usage").cloned().unwrap_or(json!({}));
        let n = |p: &str| usage.pointer(p).and_then(Value::as_u64).unwrap_or(0);
        let content = (!text.is_empty()).then_some(text);
        let reasoning = (!summary.is_empty()).then(|| summary.join("\n\n"));
        // Items are kept only when there's reasoning to carry; an empty list
        // still says which model served the turn.
        let items = if reasoned { output.clone() } else { Vec::new() };
        let message =
            Message::assistant(content, tool_calls, reasoning).with_native(NativeBlocks {
                api: API.into(),
                model: self.model.clone(),
                items,
            });
        Ok(CompletionResponse {
            message,
            usage: Usage {
                // Already includes the cached part.
                input_tokens: n("/input_tokens"),
                output_tokens: n("/output_tokens"),
                cached_input_tokens: n("/input_tokens_details/cached_tokens"),
                cache_write_input_tokens: 0,
            },
        })
    }
}

#[async_trait::async_trait]
impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let first = self.payload(&req, false);
        let body = match self.post(&first.body).await {
            // A host that can't read our reasoning back: once, without it.
            Err(CoreError::Provider(m)) if first.replayed && reasoning_rejected(&m) => {
                warn!(
                    provider = %self.name,
                    "reasoning items rejected, retrying once without them: {}",
                    m.chars().take(200).collect::<String>()
                );
                self.post(&self.payload(&req, true).body).await?
            }
            other => other?,
        };
        self.parse_response(&body)
    }
}

fn reasoning_rejected(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    (m.starts_with("http 400") || m.starts_with("http 404"))
        && (m.contains("reasoning") || m.contains("encrypted") || m.contains("rs_"))
}

/// A stored output item that can go back without `store`: a reasoning item
/// only with its encrypted content (without it, the host would look the id
/// up and fail).
fn sendable(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) != Some("reasoning")
        || item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.is_empty())
}

fn nonempty(content: &Option<String>) -> Option<&str> {
    content.as_deref().filter(|t| !t.trim().is_empty())
}

/// An assistant turn from the neutral fields: its text, then one
/// `function_call` per call (no `id`: nothing is stored).
fn neutral_assistant(m: &Message) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(t) = nonempty(&m.content) {
        items.push(json!({"role": "assistant", "content": t}));
    }
    for tc in &m.tool_calls {
        let arguments = match &tc.arguments {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        items.push(json!({
            "type": "function_call",
            "call_id": tc.id,
            "name": tc.name,
            "arguments": arguments,
        }));
    }
    items
}

fn tools_to_wire(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "strict": false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
