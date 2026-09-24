//! ferrule's own HTTPS — `web_fetch`, MCP servers reached by URL — through
//! the credential proxy, against a local origin whose CA only the proxy
//! trusts: a request that arrives at all went through the proxy.

mod common;

use bytes::Bytes;
use common::Reply;
use ferrule_core::tool::{Tool, ToolContext};
use ferrule_mcp::{connect_and_build_tools, McpServerConfig, ServerHost};
use ferrule_proxy::{Broker, BrokerConfig, SecretRule};
use ferrule_sandbox::{Egress, Sandbox};
use ferrule_tools::WebFetchTool;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "ghp_REALtokenREALtokenREALtokenREAL1234";

/// Live MCP sessions; emptied by the `forget` tool.
type Sessions = Arc<Mutex<(u32, HashSet<String>)>>;

fn text(status: StatusCode, body: impl Into<Bytes>) -> Reply {
    Ok(Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .unwrap())
}

/// `/page` is a web page saying whether the real token came in its query;
/// `/mcp` is an MCP server whose `whoami` tool says whether it came in the
/// Authorization header.
async fn respond(req: Request<Incoming>, sessions: Sessions) -> Reply {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    if req.uri().path() == "/page" {
        let real = req.uri().query() == Some(&format!("key={TOKEN}"));
        return text(
            StatusCode::OK,
            format!("<html><script>x()</script><p>hello</p><p>key_real={real}</p></html>"),
        );
    }
    if req.method() == http::Method::GET {
        // A browser also asks for /favicon.ico.
        return text(StatusCode::NOT_FOUND, "");
    }
    let auth = if header("authorization") == format!("Bearer {TOKEN}") {
        "real".to_string()
    } else {
        header("authorization")
    };
    let session = header("mcp-session-id");
    let version = header("mcp-protocol-version");
    let body = req.into_body().collect().await.unwrap().to_bytes();
    let msg: Value = serde_json::from_slice(&body).unwrap();
    let id = msg["id"].clone();
    let method = msg["method"].as_str().unwrap_or_default();
    let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    let tool_text = |text: String| reply(json!({"content": [{"type": "text", "text": text}]}));

    if method == "initialize" {
        let mut s = sessions.lock().unwrap();
        s.0 += 1;
        let new = format!("session-{}", s.0);
        s.1.insert(new.clone());
        let result = reply(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "remote", "version": "1"},
        }));
        return Ok(Response::builder()
            .header("content-type", "application/json")
            .header("mcp-session-id", new)
            .body(Full::new(Bytes::from(result.to_string())))
            .unwrap());
    }
    if !sessions.lock().unwrap().1.contains(&session) {
        return text(StatusCode::NOT_FOUND, "no such session");
    }
    let json_reply = |value: Value| {
        Ok(Response::builder()
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(value.to_string())))
            .unwrap())
    };
    match (method, msg["params"]["name"].as_str()) {
        ("notifications/initialized", _) => text(StatusCode::ACCEPTED, ""),
        ("tools/list", _) => json_reply(reply(json!({"tools": [
            {"name": "whoami", "inputSchema": {"type": "object"}},
            {"name": "forget", "inputSchema": {"type": "object"}},
        ]}))),
        ("tools/call", Some("forget")) => {
            sessions.lock().unwrap().1.clear();
            json_reply(tool_text("forgot".into()))
        }
        ("tools/call", Some("whoami")) => {
            // As a stream, with a notification ahead of the answer.
            let progress =
                json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
            let answer = tool_text(format!("auth={auth} version={version}"));
            Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from(format!(
                    "event: message\r\ndata: {progress}\r\n\r\ndata: {answer}\r\n\r\n"
                ))))
                .unwrap())
        }
        _ => text(StatusCode::BAD_REQUEST, "unexpected"),
    }
}

fn handler(
) -> impl Fn(Request<Incoming>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>
       + Clone
       + Send
       + Sync
       + 'static {
    let sessions: Sessions = Arc::default();
    move |req| Box::pin(respond(req, sessions.clone()))
}

struct Setup {
    broker: Broker,
    egress: Egress,
    port: u16,
    _dir: tempfile::TempDir,
}

/// The origin, and a proxy that binds TOKEN to 127.0.0.1 and trusts the
/// origin's CA upstream.
async fn setup() -> Setup {
    let (port, origin_ca) = common::tls_origin(handler()).await;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("system.pem");
    std::fs::write(&base, &origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([(
            "TOKEN".to_string(),
            SecretRule {
                hosts: vec!["127.0.0.1".to_string()],
                in_url: true,
            },
        )]),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        ca_bundle: Some(base),
    };
    let broker = Broker::start(cfg, |name| (name == "TOKEN").then(|| TOKEN.to_string()))
        .unwrap()
        .unwrap();
    let egress = Egress {
        proxy_url: broker.proxy_url(),
        ca_cert_pem: std::fs::read_to_string(broker.ca_cert_path()).unwrap(),
    };
    Setup {
        broker,
        egress,
        port,
        _dir: dir,
    }
}

fn placeholder(broker: &Broker) -> String {
    broker.secrets()[0].placeholder.clone()
}

#[tokio::test]
async fn web_fetch_goes_through_the_proxy_and_trusts_its_ca() {
    let s = setup().await;
    let url = format!(
        "https://127.0.0.1:{}/page?key={}",
        s.port,
        placeholder(&s.broker)
    );
    let ctx = ToolContext::default();

    let out = WebFetchTool::with_egress(Some(s.egress.clone()))
        .call(json!({ "url": url }), &ctx)
        .await
        .expect("fetched through the proxy");
    assert!(out.content.contains("hello"), "{}", out.content);
    assert!(out.content.contains("key_real=true"), "{}", out.content);

    // Without the proxy the origin's certificate isn't trusted: the fetch
    // above can only have gone through it.
    let direct = WebFetchTool::default()
        .call(json!({ "url": url }), &ctx)
        .await;
    assert!(direct.is_err(), "{:?}", direct.map(|o| o.content));
}

fn remote(url: String, auth: &str) -> McpServerConfig {
    McpServerConfig {
        name: "remote".into(),
        command: String::new(),
        args: vec![],
        env: HashMap::new(),
        url: Some(url),
        headers: HashMap::from([("Authorization".to_string(), auth.to_string())]),
        timeout_secs: Some(10),
        sandbox: true,
        writable_roots: vec![],
        ..Default::default()
    }
}

fn host(sandbox: Sandbox) -> ServerHost {
    ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: std::env::temp_dir(),
        state_dir: std::env::temp_dir(),
    }
}

async fn call(tools: &[Arc<dyn Tool>], name: &str) -> String {
    let tool = tools
        .iter()
        .find(|t| t.definition().name == format!("mcp__remote__{name}"))
        .expect("tool");
    tool.call(json!({}), &ToolContext::default())
        .await
        .unwrap()
        .content
}

#[tokio::test]
async fn an_mcp_server_by_url_gets_the_real_token_through_the_proxy() {
    let s = setup().await;
    // What the host builds: commands' env, and the proxy for its own requests.
    let sandbox = Sandbox::off()
        .with_env(s.broker.child_env())
        .with_egress(Some(s.egress.clone()));
    let cfg = remote(
        format!("https://127.0.0.1:{}/mcp", s.port),
        "Bearer ${TOKEN}",
    );
    let tools = connect_and_build_tools(cfg, host(sandbox))
        .await
        .expect("connect");
    let mut names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
    names.sort();
    assert_eq!(names, ["mcp__remote__forget", "mcp__remote__whoami"]);

    assert_eq!(call(&tools, "whoami").await, "auth=real version=2025-06-18");
    // The server forgets the session; the next call starts a new one.
    assert_eq!(call(&tools, "forget").await, "forgot");
    assert_eq!(call(&tools, "whoami").await, "auth=real version=2025-06-18");
}

#[tokio::test]
async fn without_a_proxy_an_mcp_server_by_url_is_reached_directly() {
    // Without a credential proxy the client honours the system proxy, as it
    // should; a machine that has one (this repo's dev sandbox does) would
    // send the loopback origin through it. The other tests here set their
    // proxy explicitly, which NO_PROXY doesn't touch.
    std::env::set_var("NO_PROXY", "localhost,127.0.0.1,::1");
    std::env::set_var("no_proxy", "localhost,127.0.0.1,::1");
    let port = common::plain_origin(handler()).await;
    let cfg = remote(format!("http://127.0.0.1:{port}/mcp"), "Bearer plain");
    let tools = connect_and_build_tools(cfg, host(Sandbox::off()))
        .await
        .expect("connect");
    assert_eq!(
        call(&tools, "whoami").await,
        "auth=Bearer plain version=2025-06-18"
    );
}

#[tokio::test]
async fn a_header_variable_that_is_not_there_stops_the_server() {
    let cfg = remote(
        "https://127.0.0.1:1/mcp".into(),
        "Bearer ${FERRULE_TEST_NO_SUCH_VAR}",
    );
    let err = connect_and_build_tools(cfg, host(Sandbox::off()))
        .await
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("FERRULE_TEST_NO_SUCH_VAR"),
        "{err}"
    );
}

/// A real headless Chrome, driven by agent-browser, loads a page through
/// the proxy: it trusts the proxy's CA by key only (the origin's own CA
/// isn't trusted anywhere), answers its `407`, and the placeholder in the
/// URL reaches the origin as the real token. Skipped like
/// `ferrule-mcp/tests/browser.rs` when there's no Chrome or agent-browser.
#[tokio::test]
async fn the_browser_goes_through_the_proxy_and_trusts_its_ca() {
    use ferrule_mcp::browser;
    let agent_browser = std::env::var_os("FERRULE_AGENT_BROWSER")
        .map(std::path::PathBuf::from)
        .or_else(|| browser::find_agent_browser("agent-browser").ok());
    let Some((chrome, agent_browser)) = browser::find().zip(agent_browser) else {
        let required = std::env::var("FERRULE_REQUIRE_BROWSER_TEST").is_ok_and(|v| v == "1");
        assert!(
            !required,
            "FERRULE_REQUIRE_BROWSER_TEST=1 but Chrome or agent-browser is missing"
        );
        eprintln!("skipped: no Chrome, or no agent-browser");
        return;
    };
    let s = setup().await;
    let (addr, username, password) = s.broker.proxy_auth();
    let proxy = browser::BrowserProxy {
        addr,
        username: username.into(),
        password: password.into(),
        ca_spki_sha256: s.broker.ca_spki_sha256().into(),
    };
    #[cfg(unix)]
    let is_root = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
    };
    #[cfg(not(unix))]
    let is_root = false;
    let cfg = ferrule_mcp::BrowserConfig {
        enabled: true,
        chrome_sandbox: browser::chrome_sandbox_blocker(is_root).is_none(),
        timeout_secs: Some(90),
        ..Default::default()
    };
    let state = tempfile::tempdir().unwrap();
    let mut server = cfg
        .server_config(&chrome, state.path(), Some(&proxy))
        .unwrap();
    server.command = agent_browser.to_string_lossy().into_owned();
    // Chrome never proxies loopback unless told to; the origin is on it.
    server
        .env
        .insert("AGENT_BROWSER_PROXY_BYPASS".into(), "<-loopback>".into());
    let sandbox = Sandbox::new(ferrule_sandbox::Policy::default())
        .unwrap()
        .with_env(s.broker.child_env());
    let host = ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: tempfile::tempdir().unwrap().keep(),
        state_dir: state.path().to_path_buf(),
    };
    let tools = connect_and_build_tools(server, host)
        .await
        .expect("connect");
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t.definition().name == format!("mcp__browser__agent_browser_{name}"))
            .expect("tool")
            .clone()
    };
    let ctx = ToolContext::default();
    let url = format!(
        "https://127.0.0.1:{}/page?key={}",
        s.port,
        placeholder(&s.broker)
    );
    tool("open")
        .call(json!({ "url": url }), &ctx)
        .await
        .expect("open through the proxy");
    let text = tool("get_text")
        .call(json!({ "selector": "body" }), &ctx)
        .await
        .expect("page text")
        .content;
    assert!(text.contains("key_real=true"), "{text}");
    let _ = tool("close").call(json!({}), &ctx).await;
}
