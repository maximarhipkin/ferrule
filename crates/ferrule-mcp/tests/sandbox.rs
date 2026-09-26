//! A real MCP server (`tests/fixtures/mock_mcp.py`) under the real OS
//! sandbox: what it can write, judged by which files exist afterwards —
//! not by what the server says — and what it can read. Skipped where
//! there's no sandbox backend.

use ferrule_core::tool::{Tool, ToolContext};
use ferrule_mcp::{connect_and_build_tools, McpServerConfig, ServerHost};
use ferrule_sandbox::{Mode, Policy, Sandbox};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Dirs {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    state: PathBuf,
    extra: PathBuf,
    outside: PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let make = |name: &str| {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        dunce::canonicalize(dir).unwrap()
    };
    Dirs {
        workspace: make("workspace"),
        state: make("state"),
        extra: make("extra"),
        outside: make("outside"),
        _root: root,
    }
}

/// `None` when this machine can't sandbox — the test then has nothing to
/// check. `/tmp` is left out so "outside" really is outside.
fn sandbox(mode: Mode) -> Option<Arc<Sandbox>> {
    sandbox_with(Policy {
        mode,
        tmp: false,
        ..Policy::default()
    })
}

fn sandbox_with(policy: Policy) -> Option<Arc<Sandbox>> {
    // Windows: the start-up probe runs the commands' shell, and Git Bash
    // can't hold the token (docs/windows-sandbox.md). The server is python.
    if cfg!(windows) {
        std::env::set_var(ferrule_sandbox::SHELL_VAR, "powershell");
    }
    let sandbox = Sandbox::new(policy).unwrap();
    if !sandbox.is_active() {
        eprintln!("skipped: {}", sandbox.degraded().unwrap_or("no sandbox"));
        return None;
    }
    Some(Arc::new(sandbox))
}

fn server(sandboxed: bool, extra: &Path) -> McpServerConfig {
    McpServerConfig {
        name: "fs".into(),
        command: "python3".into(),
        args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_mcp.py").into()],
        env: Default::default(),
        url: None,
        headers: Default::default(),
        timeout_secs: Some(20),
        sandbox: sandboxed,
        writable_roots: vec![extra.to_path_buf()],
        ..Default::default()
    }
}

async fn tools(cfg: McpServerConfig, sandbox: Arc<Sandbox>, d: &Dirs) -> Vec<Arc<dyn Tool>> {
    let host = ServerHost {
        sandbox,
        workspace: d.workspace.clone(),
        state_dir: d.state.clone(),
    };
    connect_and_build_tools(cfg, host).await.expect("connect")
}

async fn call(tools: &[Arc<dyn Tool>], tool: &str, args: serde_json::Value) -> String {
    let name = format!("mcp__fs__{tool}");
    let tool = tools
        .iter()
        .find(|t| t.definition().name == name)
        .expect("tool");
    tool.call(args, &ToolContext::default())
        .await
        .unwrap()
        .content
}

async fn write(tools: &[Arc<dyn Tool>], path: PathBuf) -> PathBuf {
    call(tools, "write", json!({ "path": path })).await;
    path
}

#[tokio::test]
async fn a_sandboxed_server_writes_only_where_it_is_allowed() {
    let Some(sandbox) = sandbox(Mode::WorkspaceWrite) else {
        return;
    };
    let d = dirs();
    let tools = tools(server(true, &d.extra), sandbox, &d).await;

    assert!(
        write(&tools, d.workspace.join("a")).await.exists(),
        "workspace"
    );
    assert!(write(&tools, d.state.join("a")).await.exists(), "state dir");
    assert!(
        write(&tools, d.extra.join("a")).await.exists(),
        "writable_roots"
    );
    assert!(
        !write(&tools, d.outside.join("a")).await.exists(),
        "outside"
    );

    let home = call(&tools, "env", json!({"name": "HOME"})).await;
    assert_eq!(Path::new(&home), d.state, "HOME is the state dir");
    let tmp = call(&tools, "env", json!({"name": "TMPDIR"})).await;
    assert!(
        write(&tools, Path::new(&tmp).join("a")).await.exists(),
        "TMPDIR"
    );
}

#[tokio::test]
async fn read_only_mode_still_leaves_a_server_its_state_dir() {
    let Some(sandbox) = sandbox(Mode::ReadOnly) else {
        return;
    };
    let d = dirs();
    let tools = tools(server(true, &d.extra), sandbox, &d).await;

    assert!(write(&tools, d.state.join("a")).await.exists(), "state dir");
    assert!(
        write(&tools, d.extra.join("a")).await.exists(),
        "writable_roots"
    );
    assert!(
        !write(&tools, d.workspace.join("a")).await.exists(),
        "workspace"
    );
    assert!(
        !write(&tools, d.outside.join("a")).await.exists(),
        "outside"
    );
}

#[tokio::test]
async fn sandbox_false_writes_anywhere_and_keeps_the_real_home() {
    let Some(sandbox) = sandbox(Mode::WorkspaceWrite) else {
        return;
    };
    let d = dirs();
    let tools = tools(server(false, &d.extra), sandbox, &d).await;

    assert!(write(&tools, d.outside.join("a")).await.exists(), "outside");
    let home = call(&tools, "env", json!({"name": "HOME"})).await;
    assert_ne!(Path::new(&home), d.state);
}

#[tokio::test]
async fn denied_reads_fail_for_a_server_with_or_without_the_sandbox() {
    let d = dirs();
    let secrets = d.outside.join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    let key = secrets.join("key");
    std::fs::write(&key, "sk-live").unwrap();
    let open = d.outside.join("open.txt");
    std::fs::write(&open, "fine").unwrap();
    let Some(sandbox) = sandbox_with(Policy {
        tmp: false,
        deny_read: vec![secrets.clone()],
        ..Policy::default()
    }) else {
        return;
    };
    for sandboxed in [true, false] {
        let tools = tools(server(sandboxed, &d.extra), sandbox.clone(), &d).await;
        let got = call(&tools, "read", json!({ "path": key })).await;
        assert!(got.starts_with("refused"), "sandbox = {sandboxed}: {got}");
        let got = call(&tools, "read", json!({ "path": open })).await;
        assert_eq!(got, "fine", "sandbox = {sandboxed}");
    }
}
