//! M33: the egress policy end to end. Every origin here is on loopback, so
//! "private" is what's being tested: ferrule's own requests (the `tool`
//! source) are refused there unless an endpoint or `private_allow` opens it,
//! while commands (the `command` source) keep localhost for their dev
//! servers. The metadata address is refused before any connection, so these
//! tests never touch the network.

mod common;

use bytes::Bytes;
use common::Reply;
use ferrule_core::tool::{Tool, ToolContext};
use ferrule_mcp::{connect_and_build_tools, McpServerConfig, ServerHost};
use ferrule_proxy::egress::Reason;
use ferrule_proxy::{Broker, BrokerConfig, EgressPolicy, Source, EGRESS_HEADER};
use ferrule_sandbox::{Egress, Sandbox};
use ferrule_tools::WebFetchTool;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

async fn hello(_req: Request<Incoming>) -> Reply {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(Full::new(Bytes::from_static(
            b"<p>hello from the origin</p>",
        )))
        .unwrap())
}

type Seen = Arc<Mutex<Vec<(Source, String, u16, Reason)>>>;

struct Setup {
    broker: Broker,
    /// The origin's port, over HTTPS; its CA is trusted only upstream.
    port: u16,
    origin_ca: String,
    seen: Seen,
    _dir: tempfile::TempDir,
}

/// A proxy with no secrets at all, only `policy`: it starts for the policy's
/// sake, and records every reported denial.
async fn setup(policy: impl FnOnce(u16) -> EgressPolicy) -> Setup {
    let (port, origin_ca) = common::tls_origin(hello).await;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("system.pem");
    std::fs::write(&base, &origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::new(),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        http_upstream: None,
        ca_bundle: Some(base),
        egress: Some(policy(port)),
    };
    let broker = Broker::start(cfg, |_| None)
        .unwrap()
        .expect("a policy starts the proxy without secrets");
    let seen: Seen = Arc::default();
    let log = seen.clone();
    broker.on_deny(move |d| {
        log.lock()
            .unwrap()
            .push((d.source, d.host.clone(), d.port, d.reason))
    });
    Setup {
        broker,
        port,
        origin_ca,
        seen,
        _dir: dir,
    }
}

fn tool_egress(broker: &Broker) -> Egress {
    Egress {
        proxy_url: broker.tool_proxy_url(),
        ca_cert_pem: std::fs::read_to_string(broker.ca_cert_path()).unwrap(),
    }
}

/// A client with the command source's credentials, trusting what commands
/// trust (the proxy's CA plus the "system" bundle with the origin's CA).
fn command_client(s: &Setup) -> reqwest::Client {
    let broker = &s.broker;
    let mut bundle = std::fs::read(broker.ca_cert_path()).unwrap();
    bundle.extend_from_slice(s.origin_ca.as_bytes());
    let mut builder = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(broker.proxy_url()).unwrap())
        .tls_built_in_root_certs(false);
    for cert in reqwest::Certificate::from_pem_bundle(&bundle).unwrap() {
        builder = builder.add_root_certificate(cert);
    }
    builder.build().unwrap()
}

async fn fetch(egress: &Egress, url: &str) -> Result<String, String> {
    WebFetchTool::with_egress(Some(egress.clone()))
        .call(json!({ "url": url }), &ToolContext::default())
        .await
        .map(|o| o.content)
        .map_err(|e| e.to_string())
}

/// A raw request to the proxy with `user`'s credentials; the response head.
async fn raw(broker: &Broker, user_url: &str, request_line: &str, host: &str) -> String {
    let creds = user_url
        .trim_start_matches("http://")
        .split('@')
        .next()
        .unwrap();
    let auth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, creds);
    let mut s = TcpStream::connect(broker.addr()).await.unwrap();
    s.write_all(
        format!("{request_line}\r\nHost: {host}\r\nProxy-Authorization: Basic {auth}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = vec![0u8; 2048];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test]
async fn ferrule_s_own_requests_to_loopback_are_refused_with_a_reason() {
    let s = setup(|_| EgressPolicy::standard()).await;
    let egress = tool_egress(&s.broker);

    // The IP literal, and a name that resolves to it: both private.
    for host in ["127.0.0.1", "localhost"] {
        let err = fetch(&egress, &format!("https://{host}:{}/", s.port))
            .await
            .unwrap_err();
        assert!(err.contains("ferrule egress policy: blocked"), "{err}");
        assert!(err.contains("private address"), "{err}");
        assert!(err.contains("private_allow"), "{err}");
        assert!(err.contains("retrying won't help"), "{err}");
    }
    // Plain HTTP as well.
    let plain = common::plain_origin(hello).await;
    let err = fetch(&egress, &format!("http://127.0.0.1:{plain}/"))
        .await
        .unwrap_err();
    assert!(err.contains("ferrule egress policy: blocked"), "{err}");

    // Every attempt is refused and counted, but a host is reported (ledger
    // row, audit event) once per 10 s, whatever the port: the plain-HTTP
    // one above and this retry aren't.
    let _ = fetch(&egress, &format!("https://127.0.0.1:{}/", s.port)).await;
    assert_eq!(s.broker.denials(), 4);
    let seen = s.seen.lock().unwrap().clone();
    let hosts: Vec<&str> = seen.iter().map(|(_, h, _, _)| h.as_str()).collect();
    assert_eq!(hosts, ["127.0.0.1", "localhost"], "{seen:?}");
    assert!(seen
        .iter()
        .all(|(src, _, _, r)| *src == Source::Tool && *r == Reason::Private));
}

#[tokio::test]
async fn commands_keep_localhost_but_not_the_metadata_address() {
    let s = setup(|_| EgressPolicy::standard()).await;
    let client = command_client(&s);
    let resp = client
        .get(format!("https://127.0.0.1:{}/", s.port))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("hello"));

    // Refused before any connection is tried: no network needed.
    let head = raw(
        &s.broker,
        &s.broker.proxy_url(),
        "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1",
        "169.254.169.254",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains(&format!("{EGRESS_HEADER}: denied")), "{head}");
    assert!(head.contains("169.254.169.254"), "{head}");

    // Over CONNECT the refusal is served inside the tunnel, so an HTTPS
    // client reads it as a response rather than a bare connect error.
    let resp = client
        .get("https://169.254.169.254/latest/meta-data/")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.headers()[EGRESS_HEADER], "denied");
    assert!(resp.text().await.unwrap().contains("private_allow"));
}

// These two use plain HTTP: with no secret bound for the host, HTTPS would
// be a blind tunnel to an origin whose CA web_fetch doesn't trust.
#[tokio::test]
async fn a_configured_endpoint_passes_the_guard_on_its_port_only() {
    // A local model server, as a provider's base_url would add it.
    let plain = common::plain_origin(hello).await;
    let s = setup(|_| {
        let mut p = EgressPolicy::standard();
        p.allow_endpoint("127.0.0.1", plain).unwrap();
        p
    })
    .await;
    let egress = tool_egress(&s.broker);
    let out = fetch(&egress, &format!("http://127.0.0.1:{plain}/"))
        .await
        .unwrap();
    assert!(out.contains("hello"), "{out}");
    let err = fetch(&egress, &format!("https://127.0.0.1:{}/", s.port))
        .await
        .unwrap_err();
    assert!(err.contains("private address"), "{err}");
}

#[tokio::test]
async fn private_allow_opens_loopback_to_ferrule_s_own_requests() {
    let s =
        setup(|_| EgressPolicy::new(false, &[], &[], true, &["localhost".into()]).unwrap()).await;
    let plain = common::plain_origin(hello).await;
    let egress = tool_egress(&s.broker);
    let out = fetch(&egress, &format!("http://localhost:{plain}/"))
        .await
        .unwrap();
    assert!(out.contains("hello"), "{out}");
    // The name was opened, not the address.
    let err = fetch(&egress, &format!("http://127.0.0.1:{plain}/"))
        .await
        .unwrap_err();
    assert!(err.contains("private address"), "{err}");
}

#[tokio::test]
async fn deny_rules_and_default_deny_apply_to_commands_too() {
    let s = setup(|_| {
        EgressPolicy::new(
            true,
            &["*.allowed.example".into()],
            &["*.example.com".into(), "127.0.0.1".into()],
            true,
            &[],
        )
        .unwrap()
    })
    .await;
    // A deny rule on the name is decided before any DNS.
    let head = raw(
        &s.broker,
        &s.broker.proxy_url(),
        "GET http://api.example.com/v1 HTTP/1.1",
        "api.example.com",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains("`*.example.com`"), "{head}");
    // An IP deny rule wins over the command source's loopback.
    let resp = command_client(&s)
        .get(format!("https://127.0.0.1:{}/", s.port))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(resp.text().await.unwrap().contains("deny rule `127.0.0.1`"));
    // Anything else isn't listed.
    let head = raw(
        &s.broker,
        &s.broker.proxy_url(),
        "GET http://192.0.2.10/ HTTP/1.1",
        "192.0.2.10",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains("[egress] allow"), "{head}");

    let reasons: Vec<Reason> = s.seen.lock().unwrap().iter().map(|x| x.3).collect();
    assert_eq!(reasons, [Reason::Rule, Reason::Rule, Reason::NotAllowed]);
}

#[tokio::test]
async fn an_mcp_server_on_loopback_is_refused_to_ferrule_s_own_client() {
    let s = setup(|_| EgressPolicy::standard()).await;
    let sandbox = Sandbox::off().with_egress(Some(tool_egress(&s.broker)));
    let cfg = McpServerConfig {
        name: "remote".into(),
        url: Some(format!("https://127.0.0.1:{}/mcp", s.port)),
        headers: HashMap::new(),
        timeout_secs: Some(10),
        ..Default::default()
    };
    let host = ServerHost {
        sandbox: Arc::new(sandbox),
        workspace: std::env::temp_dir(),
        state_dir: std::env::temp_dir(),
    };
    let err = match connect_and_build_tools(cfg, host).await {
        Ok(_) => panic!("connected through a refusal"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("ferrule egress policy: blocked"), "{err}");
    assert_eq!(s.seen.lock().unwrap().len(), 1);
}

/// A shell command's `curl` goes through the proxy when the policy has rules
/// of its own, and gets the refusal as an HTTP response. Unix only (it runs
/// `/bin/sh`), and skipped without curl.
#[cfg(unix)]
#[tokio::test]
async fn a_sandboxed_curl_is_refused_by_the_policy() {
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: no curl");
        return;
    }
    let s = setup(|_| EgressPolicy::new(false, &[], &["*.example.com".into()], true, &[]).unwrap())
        .await;
    assert!(s.broker.commands_proxied());
    let env = s.broker.child_env();
    assert!(env.iter().any(|(k, _)| k == "HTTPS_PROXY"), "{env:?}");
    let workspace = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::new(ferrule_sandbox::Policy {
        require: false,
        ..Default::default()
    })
    .unwrap()
    .with_env(env);
    let script =
        "curl -sS -i http://169.254.169.254/latest/; echo; curl -sS -i https://www.example.com/";
    let mut cmd = sandbox
        .command("/bin/sh", ["-c", script], workspace.path())
        .unwrap();
    for var in ["NO_PROXY", "no_proxy"] {
        cmd.env_remove(var);
    }
    let out = tokio::task::spawn_blocking(move || {
        cmd.stdin(std::process::Stdio::null()).output().unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stdout.matches(&format!("{EGRESS_HEADER}: denied")).count(),
        2,
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("private address"), "{stdout}");
    assert!(stdout.contains("`*.example.com`"), "{stdout}");
}

#[tokio::test]
async fn without_a_policy_or_rules_commands_are_left_alone() {
    let s = setup(|_| EgressPolicy::standard()).await;
    assert!(!s.broker.commands_proxied());
    assert!(s.broker.child_env().is_empty());
    // Secret-free and policy-free: nothing to run.
    let dir = tempfile::tempdir().unwrap();
    let none = Broker::start(
        BrokerConfig {
            secrets: BTreeMap::new(),
            state_dir: dir.path().join("proxy"),
            upstream: None,
            http_upstream: None,
            ca_bundle: None,
            egress: None,
        },
        |_| None,
    )
    .unwrap();
    assert!(none.is_none());
}
