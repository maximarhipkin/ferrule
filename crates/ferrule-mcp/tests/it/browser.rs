//! The browser end to end: agent-browser's MCP server driving a real
//! headless Chrome, spawned through the MCP client under the real sandbox,
//! against a page served from this test. Skipped when there's no Chrome or
//! no agent-browser (`FERRULE_AGENT_BROWSER`, else `agent-browser` on
//! PATH), unless `FERRULE_REQUIRE_BROWSER_TEST=1` says it must run.

use ferrule_core::tool::{Tool, ToolContext};
use ferrule_mcp::browser::{self, HIDDEN_ARGS};
use ferrule_mcp::{connect_and_build_tools, BrowserConfig, McpServerConfig, ServerHost};
use ferrule_sandbox::{Backend, Policy, Sandbox};
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
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("browser");
    let sandbox = Sandbox::new(Policy::default()).expect("sandbox");
    eprintln!("sandbox active: {}", sandbox.is_active());
    // As root, in a container or under Seatbelt Chrome can't keep its own
    // sandbox; the server refuses to start there unless the config accepts
    // that.
    let blocker =
        browser::chrome_sandbox_blocker(is_root(), sandbox.backend() == Backend::Seatbelt);
    if let Some(why) = blocker {
        eprintln!("Chrome's own sandbox is off here: {why}");
    }
    // FERRULE_TEST_CHROME_SANDBOX=on forces Chrome's own sandbox on even
    // where the check above says it can't work (M44 §9.1 measures that).
    let forced = std::env::var("FERRULE_TEST_CHROME_SANDBOX").is_ok_and(|v| v == "on");
    let cfg = BrowserConfig {
        enabled: true,
        chrome_sandbox: blocker.is_none() || forced,
        timeout_secs: Some(90),
        ..Default::default()
    };
    let server: McpServerConfig = McpServerConfig {
        command: agent_browser.to_string_lossy().into_owned(),
        ..cfg.server_config(&chrome, &state, None).unwrap()
    };
    let host = ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: workspace.path().to_path_buf(),
        state_dir: state.clone(),
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
            state.join("profile").exists(),
            "no profile in the state dir"
        );
    }
}

/// The sum of `VmRSS` (KiB) over `pid`'s descendants, from /proc. Linux only.
#[cfg(target_os = "linux")]
fn descendants_rss_kib(pid: u32) -> (u64, Vec<String>) {
    let mut kids: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|p| p.parse().ok()) {
            kids.entry(ppid).or_default().push(child);
        }
    }
    let (mut total, mut names, mut todo) = (0, Vec::new(), kids.remove(&pid).unwrap_or_default());
    while let Some(p) = todo.pop() {
        todo.extend(kids.remove(&p).unwrap_or_default());
        let Ok(status) = std::fs::read_to_string(format!("/proc/{p}/status")) else {
            continue;
        };
        let field = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim().to_string())
        };
        total += field("VmRSS:")
            .and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok()))
            .unwrap_or(0);
        names.push(field("Name:").unwrap_or_default());
    }
    (total, names)
}

/// A measurement for docs/m44-managed-mode.md §9, not a check. It needs the
/// network and a real Chrome, so it is skipped unless asked for:
///
/// `FERRULE_AGENT_BROWSER=/pnpm/agent-browser cargo test -p ferrule-mcp --test it browser_peak -- --ignored --nocapture`
///
/// Opens a real page, prints the peak RSS of everything under the test
/// (agent-browser, its daemon, Chrome and its helpers), then waits out a
/// 5 s idle timeout and prints what is left, which must not be Chrome.
#[cfg(target_os = "linux")]
#[ignore = "network: FERRULE_AGENT_BROWSER=… cargo test -p ferrule-mcp --test it browser_peak -- --ignored --nocapture"]
#[tokio::test]
async fn browser_peak_rss_on_a_real_page() {
    let Some((chrome, agent_browser)) = prerequisites() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("browser");
    let sandbox = Sandbox::new(Policy::default()).expect("sandbox");
    let blocker =
        browser::chrome_sandbox_blocker(is_root(), sandbox.backend() == Backend::Seatbelt);
    let cfg = BrowserConfig {
        enabled: true,
        chrome_sandbox: blocker.is_none(),
        idle_timeout_secs: 5,
        timeout_secs: Some(120),
        ..Default::default()
    };
    let mut server: McpServerConfig = McpServerConfig {
        command: agent_browser.to_string_lossy().into_owned(),
        ..cfg.server_config(&chrome, &state, None).unwrap()
    };
    // Behind a TLS-intercepting egress proxy Chrome doesn't trust the
    // proxy's CA: FERRULE_TEST_CHROME_ARGS=--ignore-certificate-errors.
    if let Ok(extra) = std::env::var("FERRULE_TEST_CHROME_ARGS") {
        let args = server.env.entry("AGENT_BROWSER_ARGS".into()).or_default();
        if !args.is_empty() {
            args.push(',');
        }
        args.push_str(&extra);
    }
    let host = ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: workspace.path().to_path_buf(),
        state_dir: state.clone(),
    };
    let tools = connect_and_build_tools(server, host)
        .await
        .expect("connect");

    let me = std::process::id();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut peak = (0, Vec::new());
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let now = descendants_rss_kib(me);
                if now.0 > peak.0 {
                    peak = now;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            peak
        })
    };
    let ctx = ToolContext::default();
    let call = |name: &'static str, args: serde_json::Value| {
        let t = tool(&tools, name).clone();
        let ctx = ctx.clone();
        async move { t.call(args, &ctx).await.map(|o| o.content) }
    };
    let opened = call(
        "open",
        json!({ "url": "https://en.wikipedia.org/wiki/Rust_(programming_language)" }),
    )
    .await
    .unwrap_or_else(|e| panic!("open: {e}"));
    let title = call("get_title", json!({})).await.unwrap();
    let _ = call("snapshot", json!({ "interactive": true })).await;
    eprintln!("opened: {opened:.120} / title: {title}");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (peak, names) = sampler.join().unwrap();
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for n in names {
        *counts.entry(n).or_default() += 1;
    }
    eprintln!("PEAK tree RSS: {:.1} MiB {counts:?}", peak as f64 / 1024.0);

    // Idle: the daemon closes Chrome after the idle timeout.
    std::thread::sleep(std::time::Duration::from_secs(12));
    let (left, names) = descendants_rss_kib(me);
    eprintln!("IDLE tree RSS: {:.1} MiB {names:?}", left as f64 / 1024.0);
    assert!(
        !names.iter().any(|n| n.to_lowercase().contains("chrom")),
        "Chrome is still running after the idle timeout: {names:?}"
    );
    assert!(
        state.join("profile").exists(),
        "no profile in the state dir"
    );
}
