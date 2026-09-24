//! Hermetic tests against `tests/fixtures/mock_mcp.py`, a tiny real MCP
//! server over stdio. No network, no ambient environment dependency.

use ferrule_core::tool::ToolContext;
use ferrule_mcp::{connect_and_build_tools, McpServerConfig, ServerHost};
use ferrule_sandbox::Sandbox;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn fixture_cfg(name: &str, timeout_secs: Option<u64>) -> McpServerConfig {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py");
    McpServerConfig {
        name: name.into(),
        command: "python3".into(),
        args: vec![script.into()],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        timeout_secs,
        sandbox: true,
        writable_roots: vec![],
    }
}

/// Off, not a real probed backend: these tests exercise the MCP protocol
/// and stay hermetic on every OS; `sandbox.rs` covers enforcement. The dirs
/// are leaked on purpose — each server gets fresh ones.
fn host() -> ServerHost {
    ServerHost {
        sandbox: Arc::new(Sandbox::off()),
        workspace: tempfile::tempdir().unwrap().keep(),
        state_dir: tempfile::tempdir().unwrap().keep(),
    }
}

#[tokio::test]
async fn handshake_lists_paginated_tools_with_namespaced_names() {
    let tools = connect_and_build_tools(fixture_cfg("test", None), host())
        .await
        .expect("connect");
    let mut names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "mcp__test__add",
            "mcp__test__boom",
            "mcp__test__crash",
            "mcp__test__echo",
            "mcp__test__env",
            "mcp__test__ping_first",
            "mcp__test__slow",
            "mcp__test__write"
        ]
    );
}

#[tokio::test]
async fn successful_call_returns_text_content() {
    let tools = connect_and_build_tools(fixture_cfg("test", None), host())
        .await
        .expect("connect");
    let echo = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__echo")
        .expect("echo tool");
    let ctx = ToolContext::default();
    let out = echo
        .call(json!({"text": "hi"}), &ctx)
        .await
        .expect("call ok");
    assert_eq!(out.content, "hi");
    assert!(!out.truncated);
}

#[tokio::test]
async fn is_error_result_becomes_a_tool_error_not_a_panic() {
    let tools = connect_and_build_tools(fixture_cfg("test", None), host())
        .await
        .expect("connect");
    let boom = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__boom")
        .expect("boom tool");
    let ctx = ToolContext::default();
    let err = boom
        .call(json!({}), &ctx)
        .await
        .expect_err("boom should error");
    assert!(err.to_string().contains("boom failed"), "got: {err}");
}

#[tokio::test]
async fn call_timeout_is_reported_not_hung() {
    let tools = connect_and_build_tools(fixture_cfg("test", Some(1)), host())
        .await
        .expect("connect");
    let slow = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__slow")
        .expect("slow tool");
    let ctx = ToolContext::default();
    let started = std::time::Instant::now();
    let err = slow
        .call(json!({}), &ctx)
        .await
        .expect_err("slow should time out");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "should give up at the 1s timeout, not hang"
    );
    assert!(err.to_string().contains("timed out"), "got: {err}");
}

#[tokio::test]
async fn crashed_server_is_respawned_lazily_on_next_call() {
    let tools = connect_and_build_tools(fixture_cfg("test", None), host())
        .await
        .expect("connect");
    let crash = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__crash")
        .expect("crash tool");
    let echo = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__echo")
        .expect("echo tool");
    let ctx = ToolContext::default();

    // First call kills the child mid-request; this call must report an
    // error, not hang or panic.
    crash
        .call(json!({}), &ctx)
        .await
        .expect_err("crash call should error");

    // The next call to the same (now-dead) connection respawns once and
    // succeeds transparently.
    let out = echo
        .call(json!({"text": "hi"}), &ctx)
        .await
        .expect("respawned call should succeed");
    assert_eq!(out.content, "hi");
}

#[tokio::test]
async fn slow_call_does_not_block_other_calls_to_the_same_server() {
    let tools = connect_and_build_tools(fixture_cfg("test", Some(5)), host())
        .await
        .expect("connect");
    let find = |n: &str| {
        tools
            .iter()
            .find(|t| t.definition().name == n)
            .cloned()
            .expect("tool")
    };
    let slow = find("mcp__test__slow");
    let echo = find("mcp__test__echo");

    let pending = tokio::spawn(async move { slow.call(json!({}), &ToolContext::default()).await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let out = echo
        .call(json!({"text": "hi"}), &ToolContext::default())
        .await
        .expect("echo while slow is in flight");
    assert_eq!(out.content, "hi");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "echo waited on the in-flight slow call: {:?}",
        started.elapsed()
    );
    pending.abort();
}

#[tokio::test]
async fn server_initiated_ping_is_answered_and_not_mistaken_for_a_response() {
    let tools = connect_and_build_tools(fixture_cfg("test", Some(5)), host())
        .await
        .expect("connect");
    let ping_first = tools
        .iter()
        .find(|t| t.definition().name == "mcp__test__ping_first")
        .expect("ping_first tool");
    let out = ping_first
        .call(json!({}), &ToolContext::default())
        .await
        .expect("call ok");
    assert_eq!(out.content, "pong-ok");
}
