//! The native Anthropic Messages driver (M23, `api = "anthropic"`).
//!
//! What it adds over the compatible endpoint: prompt caching (explicit
//! breakpoints, cache reads and writes in the ledger) and thinking carried
//! through a tool loop verbatim. The neutral transcript stays the source of
//! truth; this driver's own content blocks ride along in `Message::native`
//! and go back only inside the loop that made them (design §2).

use crate::common::{self, last_user, replayable};
use crate::{DriverOptions, Thinking};
use ferrule_core::error::CoreError;
use ferrule_core::message::{Message, NativeBlocks, Role, ToolCall, Usage};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Delta, DeltaSink, Provider};
use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Value};
use tracing::warn;

/// `NativeBlocks::api` for this driver.
pub const API: &str = "anthropic";
const VERSION: &str = "2023-06-01";
/// `max_tokens` caps thinking plus text, and the 5-series think by
/// default: a request asking for less is raised to this (it's only a cap).
pub const MAX_TOKENS_FLOOR: u32 = 16_000;

pub struct AnthropicProvider {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    options: DriverOptions,
    client: reqwest::Client,
}

/// A request body, and whether it carries anything the thinking-400 retry
/// would take out.
struct Payload {
    body: Value,
    /// Native blocks were replayed, or a thinking or effort field went with
    /// a tool loop in progress.
    loop_extras: bool,
}

type WireMessage = (&'static str, Vec<Value>);

impl AnthropicProvider {
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

    /// The request body. `plain` is the retry after a thinking 400: no
    /// native blocks, no `thinking`, no `output_config`.
    fn payload(&self, req: &CompletionRequest, plain: bool) -> Payload {
        let msgs = &req.messages;
        let lead = msgs.iter().take_while(|m| m.role == Role::System).count();
        let mut system: Vec<Value> = msgs[..lead]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .filter(|t| !t.trim().is_empty())
            .map(text_block)
            .collect();

        let latest_user = last_user(msgs);
        let mut wire: Vec<WireMessage> = Vec::new();
        // Where each user message's text ended up: (message, block).
        let mut user_marks: Vec<(usize, usize)> = Vec::new();
        let mut replayed = false;
        let mut loop_tool_use = false;
        let mut loop_native = false;
        for (i, m) in msgs.iter().enumerate().skip(lead) {
            let in_loop = latest_user.is_none_or(|u| i > u);
            match m.role {
                // Mid-conversation system text (a hook note, a resumed
                // session's reminder): the system role there is beta.
                Role::System => {
                    if let Some(t) = nonempty(&m.content) {
                        push(
                            &mut wire,
                            "user",
                            vec![text_block(&format!("[system] {t}"))],
                        );
                    }
                }
                Role::User => {
                    if let Some(t) = nonempty(&m.content) {
                        push(&mut wire, "user", vec![text_block(t)]);
                        let at = wire.len() - 1;
                        let mark = (at, wire[at].1.len() - 1);
                        // Text right after the goal (recalled memory, a
                        // hook's note) joins its wire message: the mark
                        // moves to its end, where that request's own
                        // breakpoint was (M27).
                        match user_marks.last_mut() {
                            Some(last) if last.0 == at => *last = mark,
                            _ => user_marks.push(mark),
                        }
                    }
                }
                Role::Tool => {
                    let mut block = json!({
                        "type": "tool_result",
                        "tool_use_id": sanitize_id(m.tool_call_id.as_deref().unwrap_or("")),
                    });
                    if let Some(t) = nonempty(&m.content) {
                        block["content"] = json!(t);
                    }
                    push(&mut wire, "user", vec![block]);
                }
                Role::Assistant => {
                    if in_loop {
                        loop_tool_use |= !m.tool_calls.is_empty();
                        loop_native |= m
                            .native
                            .as_ref()
                            .is_some_and(|n| n.api == API && n.model == self.model);
                    }
                    let own = if plain {
                        None
                    } else {
                        replayable(m, API, &self.model, in_loop)
                    };
                    let blocks = match own {
                        Some(items) => {
                            replayed = true;
                            items.to_vec()
                        }
                        None => neutral_assistant(m),
                    };
                    if !blocks.is_empty() {
                        push(&mut wire, "assistant", blocks);
                    }
                }
            }
        }
        // The API wants a user message first; a compaction tail can start
        // with an assistant turn.
        if wire.first().is_none_or(|(role, _)| *role != "user") {
            wire.insert(0, ("user", vec![text_block("[continued]")]));
            for mark in &mut user_marks {
                mark.0 += 1;
            }
        }

        let mut tools = tools_to_wire(&req.tools);
        // Breakpoint 1: tools and system render first, so one breakpoint on
        // the last of them covers both.
        if let Some(last) = system.last_mut() {
            last["cache_control"] = ephemeral();
        } else if let Some(last) = tools.last_mut() {
            last["cache_control"] = ephemeral();
        }
        // Breakpoint 2: the end of the request, where the next one reads.
        // Breakpoint 3: the end of the previous user message, the prefix the
        // previous loop's first request wrote and that still matches after
        // that loop's turns lose their thinking (design §3).
        let end = wire.len() - 1;
        let mut marks = vec![(end, wire[end].1.len() - 1)];
        if user_marks.len() >= 2 {
            marks.push(user_marks[user_marks.len() - 2]);
        }
        for (m, b) in marks {
            // Only on blocks Ferrule built: a replayed block goes back as it came.
            if wire[m].0 == "user" {
                wire[m].1[b]["cache_control"] = ephemeral();
            }
        }

        let mut body = json!({
            "model": self.model,
            "messages": wire
                .into_iter()
                .map(|(role, content)| json!({"role": role, "content": content}))
                .collect::<Vec<_>>(),
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            // Forced choice is a 400 on Opus 5.5 and Fable 5.1; auto is
            // what every caller means anyway.
            body["tool_choice"] = json!({"type": "auto"});
        }

        // The in-loop rule (design §4): a loop that started on another
        // model has no thinking of ours to show, so don't ask for it now.
        let mut thinking = self.options.thinking.filter(|_| !plain);
        if loop_tool_use && !loop_native {
            thinking = None;
        }
        let effort = self.options.effort.as_deref().filter(|_| !plain);
        match thinking {
            Some(Thinking::Adaptive) => body["thinking"] = json!({"type": "adaptive"}),
            Some(Thinking::Disabled) => body["thinking"] = json!({"type": "disabled"}),
            Some(Thinking::Budget(n)) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": n})
            }
            None => {}
        }
        if let Some(e) = effort {
            body["output_config"] = json!({ "effort": e });
        }

        let mut max_tokens = req
            .max_output_tokens
            .or(self.options.max_tokens)
            .unwrap_or(MAX_TOKENS_FLOOR);
        if self.options.thinking != Some(Thinking::Disabled) {
            max_tokens = max_tokens.max(MAX_TOKENS_FLOOR);
        }
        if let Some(Thinking::Budget(n)) = thinking {
            max_tokens = max_tokens.max(n.saturating_add(4096));
        }
        body["max_tokens"] = json!(max_tokens);
        // `temperature` is never sent: a 400 on Opus 4.7+, Sonnet 5 and Fable.

        Payload {
            body,
            loop_extras: replayed || (loop_tool_use && (thinking.is_some() || effort.is_some())),
        }
    }

    fn request(&self, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", VERSION)
            .json(body)
    }

    /// Send `body` and return the reply's message. With a sink, it asks
    /// for a stream and rebuilds that message from the events.
    async fn post(&self, body: &Value, sink: Option<&DeltaSink>) -> Result<Value, CoreError> {
        let reply = match sink {
            None => common::send(self.request(body)).await?,
            Some(sink) => {
                let mut body = body.clone();
                body["stream"] = json!(true);
                match common::open(self.request(&body)).await? {
                    common::Opened::Json(reply) => reply,
                    common::Opened::Events(events) => return read_stream(events, sink).await,
                }
            }
        };
        if reply.body.get("type").and_then(Value::as_str) == Some("error") {
            return Err(common::error_in_body(&reply.body["error"], reply.wait));
        }
        Ok(reply.body)
    }

    fn parse_response(&self, body: &Value) -> Result<CompletionResponse, CoreError> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::MalformedResponse(format!("no content in: {}", common::truncate(body)))
            })?;
        if body.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            let category = body
                .pointer("/stop_details/category")
                .and_then(Value::as_str)
                .unwrap_or("unspecified");
            return Err(CoreError::Provider(format!("refused: {category}")));
        }

        let mut text = String::new();
        let mut thinking: Vec<&str> = Vec::new();
        let mut signed = false;
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or("").to_string(),
                    name: block["name"].as_str().unwrap_or("").to_string(),
                    arguments: block.get("input").cloned().unwrap_or(json!({})),
                }),
                Some("thinking") => {
                    signed = true;
                    if let Some(t) = block["thinking"].as_str().filter(|t| !t.is_empty()) {
                        thinking.push(t);
                    }
                }
                Some("redacted_thinking") => signed = true,
                _ => {}
            }
        }

        let usage = body.get("usage").cloned().unwrap_or(json!({}));
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (fresh, write, read) = (
            n("input_tokens"),
            n("cache_creation_input_tokens"),
            n("cache_read_input_tokens"),
        );

        let content = (!text.is_empty()).then_some(text);
        let reasoning = (!thinking.is_empty()).then(|| thinking.join("\n\n"));
        // The blocks themselves are kept only when there's thinking to carry;
        // an empty list still says which model served the turn.
        let items = if signed { blocks.clone() } else { Vec::new() };
        let message =
            Message::assistant(content, tool_calls, reasoning).with_native(NativeBlocks {
                api: API.into(),
                model: self.model.clone(),
                items,
            });
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: fresh + write + read,
                output_tokens: n("output_tokens"),
                cached_input_tokens: read,
                cache_write_input_tokens: write,
            },
        })
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let first = self.payload(&req, false);
        let sink = req.stream.as_ref();
        let body = match self.post(&first.body, sink).await {
            // Safety net: thinking bound to a prefix that moved under it.
            // Once, without our blocks; a second 400 is a plain BadRequest.
            Err(CoreError::Provider(m)) if first.loop_extras && thinking_rejected(&m) => {
                warn!(
                    provider = %self.name,
                    "thinking blocks rejected, retrying once without them: {}",
                    m.chars().take(200).collect::<String>()
                );
                self.post(&self.payload(&req, true).body, sink).await?
            }
            other => other?,
        };
        self.parse_response(&body)
    }
}

/// A Messages stream rebuilt into the message a plain call returns:
/// `message_start` gives the frame and the input usage, each block is
/// rebuilt from its start and deltas, `message_delta` adds the stop reason
/// and the final counts.
async fn read_stream(mut events: common::Events, sink: &DeltaSink) -> Result<Value, CoreError> {
    let mut message = json!({});
    let mut blocks: Vec<Value> = Vec::new();
    // Tool input JSON as it comes, by block index.
    let mut inputs: Vec<String> = Vec::new();
    while let Some(event) = events.next().await? {
        let data = event.json()?;
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match data.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => message = data["message"].clone(),
            "content_block_start" => {
                if blocks.len() <= index {
                    blocks.resize(index + 1, Value::Null);
                    inputs.resize(index + 1, String::new());
                }
                blocks[index] = data["content_block"].clone();
            }
            "content_block_delta" => {
                let Some(block) = blocks.get_mut(index) else {
                    continue;
                };
                let delta = &data["delta"];
                let piece = |k: &str| delta.get(k).and_then(Value::as_str).unwrap_or("");
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        append(block, "text", piece("text"));
                        sink.send(Delta::Text(piece("text").to_string()));
                    }
                    "input_json_delta" => {
                        inputs[index].push_str(piece("partial_json"));
                        sink.send(Delta::Progress);
                    }
                    "thinking_delta" => {
                        append(block, "thinking", piece("thinking"));
                        sink.send(Delta::Progress);
                    }
                    "signature_delta" => append(block, "signature", piece("signature")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let (Some(block), Some(input)) = (blocks.get_mut(index), inputs.get(index)) else {
                    continue;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && !input.is_empty()
                {
                    block["input"] = serde_json::from_str(input).unwrap_or_else(|_| {
                        warn!("a streamed tool input that isn't JSON: {input:.200}");
                        json!({})
                    });
                }
            }
            "message_delta" => {
                if let Some(delta) = data["delta"].as_object() {
                    for (k, v) in delta {
                        message[k] = v.clone();
                    }
                }
                if let Some(usage) = data["usage"].as_object() {
                    for (k, v) in usage.iter().filter(|(_, v)| !v.is_null()) {
                        message["usage"][k] = v.clone();
                    }
                }
            }
            "message_stop" => {
                message["content"] = json!(blocks
                    .into_iter()
                    .filter(|b| !b.is_null())
                    .collect::<Vec<_>>());
                return Ok(message);
            }
            "error" => return Err(common::stream_error(&data["error"])),
            _ => {} // ping, and whatever comes next
        }
    }
    Err(common::ended_early(events.seen()))
}

fn append(block: &mut Value, key: &str, piece: &str) {
    let now = block.get(key).and_then(Value::as_str).unwrap_or("");
    block[key] = json!(format!("{now}{piece}"));
}

fn thinking_rejected(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.starts_with("http 400") && (m.contains("thinking") || m.contains("signature"))
}

fn text_block(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

fn nonempty(content: &Option<String>) -> Option<&str> {
    content.as_deref().filter(|t| !t.trim().is_empty())
}

/// Append to the last message when the role repeats: the API wants
/// alternating turns, and parallel results in one user message.
fn push(wire: &mut Vec<WireMessage>, role: &'static str, blocks: Vec<Value>) {
    match wire.last_mut() {
        Some((last, content)) if *last == role => content.extend(blocks),
        _ => wire.push((role, blocks)),
    }
}

/// An assistant turn from the neutral fields: text, then its calls.
fn neutral_assistant(m: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(t) = nonempty(&m.content) {
        blocks.push(text_block(t));
    }
    for tc in &m.tool_calls {
        let input = if tc.arguments.is_object() {
            tc.arguments.clone()
        } else {
            json!({})
        };
        blocks.push(json!({
            "type": "tool_use",
            "id": sanitize_id(&tc.id),
            "name": tc.name,
            "input": input,
        }));
    }
    blocks
}

/// Tool ids as Anthropic accepts them (`[A-Za-z0-9_-]+`). Kimi's
/// `functions.read:0` becomes `functions_read_0`, on the call and on its
/// result alike; the transcript keeps the original.
pub fn sanitize_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "call".into()
    } else {
        s
    }
}

fn tools_to_wire(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            // `input_schema` must say it's an object.
            let mut schema = match &t.parameters {
                Value::Object(_) => t.parameters.clone(),
                _ => json!({"properties": {}}),
            };
            if schema.get("type").is_none() {
                schema["type"] = json!("object");
            }
            json!({"name": t.name, "description": t.description, "input_schema": schema})
        })
        .collect()
}

#[cfg(test)]
mod tests;
