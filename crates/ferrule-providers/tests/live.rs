//! Live smoke tests for the M23 native drivers: one two-step tool call
//! against the real API each. Ignored by default (they cost a few cents and
//! need a key); run them with
//!
//! ```text
//! ANTHROPIC_API_KEY=… cargo test -p ferrule-providers --test live -- --ignored --nocapture
//! OPENAI_API_KEY=…    cargo test -p ferrule-providers --test live -- --ignored --nocapture
//! ```
//!
//! `FERRULE_LIVE_ANTHROPIC_MODEL`, `FERRULE_LIVE_OPENAI_MODEL` and
//! `FERRULE_LIVE_OPENAI_BASE_URL` pick another model or a compatible host.
//! Without a key a test says so and passes.

use ferrule_core::message::Message;
use ferrule_core::provider::{CompletionRequest, Provider};
use ferrule_core::tool::ToolDefinition;
use ferrule_providers::{build, Api, DriverOptions, Thinking};
use serde_json::json;
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn weather() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "The current weather in a city.".into(),
        parameters: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }
}

/// Ask for the weather, answer the tool call, and expect the answer to use
/// the tool's result: two calls, the second replaying the first's native
/// blocks.
async fn two_step(provider: Arc<dyn Provider>) {
    let mut messages = vec![
        Message::system("You are a terse assistant. Use tools when they help."),
        Message::user(
            "What's the weather in Paris right now? Use the tool, then answer in one line.",
        ),
    ];
    let req = |messages: &Vec<Message>| CompletionRequest {
        messages: messages.clone(),
        tools: vec![weather()],
        max_output_tokens: None,
        temperature: None,
    };
    let first = provider.complete(req(&messages)).await.unwrap();
    eprintln!("first: {:?} · {:?}", first.message.tool_calls, first.usage);
    let call = first
        .message
        .tool_calls
        .first()
        .expect("the model called the tool")
        .clone();
    assert_eq!(call.name, "get_weather");
    assert!(first.usage.input_tokens > 0 && first.usage.output_tokens > 0);
    messages.push(first.message);
    messages.push(Message::tool_result(
        call.id,
        "Paris: 17 degrees Celsius, light rain.",
    ));
    let second = provider.complete(req(&messages)).await.unwrap();
    let text = second.message.content.unwrap_or_default();
    eprintln!("second: {text:?} · {:?}", second.usage);
    assert!(text.contains("17"), "{text}");
}

#[tokio::test]
#[ignore = "live: needs ANTHROPIC_API_KEY, costs a few cents"]
async fn anthropic_messages_two_step_tool_call() {
    let Some(key) = env("ANTHROPIC_API_KEY") else {
        eprintln!("ANTHROPIC_API_KEY isn't set: skipped");
        return;
    };
    let model = env("FERRULE_LIVE_ANTHROPIC_MODEL").unwrap_or_else(|| "claude-sonnet-5".into());
    let options = DriverOptions {
        thinking: Some(Thinking::Adaptive),
        ..Default::default()
    };
    let p = build(
        Api::Anthropic,
        "anthropic",
        "https://api.anthropic.com/v1",
        key,
        &model,
        options,
    );
    two_step(p).await;
}

#[tokio::test]
#[ignore = "live: needs OPENAI_API_KEY, costs a few cents"]
async fn openai_responses_two_step_tool_call() {
    let Some(key) = env("OPENAI_API_KEY") else {
        eprintln!("OPENAI_API_KEY isn't set: skipped");
        return;
    };
    let model = env("FERRULE_LIVE_OPENAI_MODEL").unwrap_or_else(|| "gpt-5-mini".into());
    let base =
        env("FERRULE_LIVE_OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into());
    let options = DriverOptions {
        effort: Some("low".into()),
        ..Default::default()
    };
    let p = build(Api::Responses, "openai", &base, key, &model, options);
    two_step(p).await;
}
