//! The browser end to end: agent-browser's MCP server driving a real
//! headless Chrome, spawned through the MCP client under the real sandbox,
//! against a page served from this test. Skipped when there's no Chrome or
//! no agent-browser (`FERRULE_AGENT_BROWSER`, else `agent-browser` on
//! PATH), unless `FERRULE_REQUIRE_BROWSER_TEST=1` says it must run.

use ferrule_core::tool::{Tool, ToolContext};
use ferrule_mcp::browser::{self, HIDDEN_ARGS};
use ferrule_mcp::{connect_and_build_tools, BrowserConfig, McpServerConfig, ServerHost};
use ferrule_sandbox::{Policy, Sandbox};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

const PAGE: &str = "<!doctype html><html><head><title>Ferrule test</title></head><body>\
<h1 id=\"h\">Hello from the test page</h1>\
<button onclick=\"document.getElementById('h').textContent='clicked by script'\">Press me</button>\
</body></html>";

/// Serve `PAGE` on a loopback port for as long as the test runs.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    PAGE.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(PAGE.as_bytes());
            });
        }
    });
    format!("http://{addr}/")
}

/// Chrome and agent-browser, or `None` (and a note) to skip.
fn prerequisites() -> Option<(PathBuf, PathBuf)> {
    let agent_browser = std::env::var_os("FERRULE_AGENT_BROWSER")
        .map(PathBuf::from)
        .or_else(|| browser::find_agent_browser("agent-browser").ok());
    let found = browser::find().zip(agent_browser);
    if found.is_none() {
        let required = std::env::var("FERRULE_REQUIRE_BROWSER_TEST").is_ok_and(|v| v == "1");
        assert!(
            !required,
            "FERRULE_REQUIRE_BROWSER_TEST=1 but Chrome or agent-browser is missing"
        );
        eprintln!("skipped: no Chrome, or no agent-browser");
    }
    found
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn tool<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
    let full = format!("mcp__browser__agent_browser_{name}");
    tools
        .iter()
        .find(|t| t.definition().name == full)
        .unwrap_or_else(|| panic!("no {full}"))
}

#[tokio::test]
async fn the_agent_drives_a_real_chrome_inside_the_sandbox() {
    let Some((chrome, agent_browser)) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    // As root or in a container Chrome can't keep its own sandbox; the
    // server refuses to start there unless the config accepts that.
    let blocker = browser::chrome_sandbox_blocker(is_root());
    if let Some(why) = blocker {
        eprintln!("Chrome's own sandbox is off here: {why}");
    }
    let cfg = BrowserConfig {
        enabled: true,
        chrome_sandbox: blocker.is_none(),
        timeout_secs: Some(90),
        ..Default::default()
    };
    let server: McpServerConfig = McpServerConfig {
        command: agent_browser.to_string_lossy().into_owned(),
        ..cfg.server_config(&chrome, state.path(), None).unwrap()
    };
    let sandbox = Sandbox::new(Policy::default()).expect("sandbox");
    eprintln!("sandbox active: {}", sandbox.is_active());
    let host = ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: workspace.path().to_path_buf(),
        state_dir: state.path().to_path_buf(),
    };
    let tools = connect_and_build_tools(server, host)
        .await
        .expect("connect");

    // What could loosen the policy isn't offered to the model at all.
    for t in &tools {
        let def = t.definition();
        for hidden in HIDDEN_ARGS {
            assert!(
                def.parameters["properties"].get(hidden).is_none(),
                "{} still offers {hidden}",
                def.name
            );
        }
    }

    let ctx = ToolContext::default();
    let url = serve();
    let call = |name: &'static str, args: serde_json::Value| {
        let t = tool(&tools, name).clone();
        let ctx = ctx.clone();
        async move {
            t.call(args, &ctx)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .content
        }
    };
    call("open", json!({ "url": url })).await;
    let title = call("get_title", json!({})).await;
    assert!(title.contains("Ferrule test"), "title: {title}");
    let snapshot = call("snapshot", json!({ "interactive": true })).await;
    assert!(snapshot.contains("Press me"), "snapshot: {snapshot}");
    call("click", json!({ "selector": "button" })).await;
    // Only a real browser running the page's script gets here.
    let text = call("get_text", json!({ "selector": "#h" })).await;
    assert!(text.contains("clicked by script"), "text: {text}");
    call("close", json!({})).await;

    // The profile went where the config put it, not in the owner's home.
    if cfg.allowed_domains.is_empty() {
        assert!(
            state.path().join("profile").exists(),
            "no profile in the state dir"
        );
    }
}
