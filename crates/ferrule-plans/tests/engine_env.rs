//! The engine's child never sees a credential that would outrank the plan.
//! Its own binary, not in `tests/it/`: it sets API keys in the process
//! environment, which the other engine tests would inherit.

use ferrule_core::message::Message;
use ferrule_core::provider::{CompletionRequest, Provider};
use ferrule_plans::claude::{ClaudeCode, EngineConfig};
use std::path::PathBuf;

#[tokio::test]
async fn keys_that_would_bill_the_api_never_reach_claude() {
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-api03-must-not-leak");
    std::env::set_var("ANTHROPIC_BASE_URL", "https://example.invalid");
    std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
    std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("claude-code");
    let cfg = EngineConfig::new(
        PathBuf::from(env!("CARGO_BIN_EXE_ferrule-fake-claude")),
        config_dir.clone(),
        tmp.path().to_path_buf(),
    );
    let engine = ClaudeCode::new("claude-code", "haiku", cfg);
    engine
        .complete(CompletionRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_output_tokens: None,
            temperature: None,
            stream: None,
        })
        .await
        .unwrap();
    let call = std::fs::read_dir(config_dir.join("fake"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .unwrap()
        .path();
    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(call).unwrap()).unwrap();
    assert_eq!(seen["anthropic_api_key"], false);
    assert_eq!(seen["anthropic_base_url"], false);
    assert_eq!(seen["claude_code_any"], false);
    assert_eq!(seen["token_sha"], serde_json::Value::Null);
    // No bridge configured: no MCP config and no permission prompt tool.
    let args = seen["args"].to_string();
    assert!(!args.contains("--mcp-config"), "{args}");
    assert!(!args.contains("--permission-prompt-tool"), "{args}");
}
