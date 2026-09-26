//! M32: a WASM plugin's `http` host op goes through the credential proxy
//! like `web_fetch`: the plugin names a secret as `${TOKEN}`, the request
//! leaves with its placeholder, the proxy swaps the real value in for a
//! bound host, and the response comes back scrubbed. The origin's CA is
//! trusted only by the proxy, so a request that arrives went through it.

mod common;

use bytes::Bytes;
use common::Reply;
use ferrule_core::tool::{Tool, ToolContext};
use ferrule_plugins::manifest::sha256_hex;
use ferrule_plugins::{tools, Manifest, Plugin};
use ferrule_proxy::{Broker, BrokerConfig, SecretRule};
use ferrule_sandbox::{Egress, Sandbox};
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const TOKEN: &str = "ghp_PLUGINrealPLUGINrealPLUGINreal5678";

/// Says whether the real token came in the Authorization header, and
/// echoes the header back (which the proxy must scrub).
async fn respond(req: Request<Incoming>) -> Reply {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let real = auth == format!("Bearer {TOKEN}");
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(Full::new(Bytes::from(format!("real={real} echo=[{auth}]"))))
        .unwrap())
}

struct Setup {
    broker: Broker,
    egress: Egress,
    port: u16,
    ws: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let (port, origin_ca) = common::tls_origin(|req| Box::pin(respond(req))).await;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("system.pem");
    std::fs::write(&base, &origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([(
            "TOKEN".to_string(),
            SecretRule {
                hosts: vec!["127.0.0.1".to_string()],
                in_url: false,
            },
        )]),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        http_upstream: None,
        ca_bundle: Some(base),
        egress: None,
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
        ws: tempfile::tempdir().unwrap(),
        _dir: dir,
    }
}

/// The probe plugin's `host` tool (its arguments are the host call, its
/// output the reply), allowed 127.0.0.1 and the secret TOKEN.
fn host_tool(sandbox: &Sandbox) -> Arc<dyn Tool> {
    let wasm = wat::parse_str(include_str!(
        "../../ferrule-plugins/tests/fixtures/probe.wat"
    ))
    .unwrap();
    let m = json!({
        "name": "probe",
        "version": "0.1.0",
        "wasm": "probe.wasm",
        "sha256": sha256_hex(&wasm),
        "tools": [{"name": "host", "description": "probe host", "read_only": true}],
        "capabilities": {"http": {"domains": ["127.0.0.1"]}, "secrets": ["TOKEN"]},
    });
    let plugin = Plugin::load(Manifest::parse(&m.to_string()).unwrap(), &wasm).unwrap();
    tools(Arc::new(plugin), sandbox).remove(0)
}

impl Setup {
    fn proxied(&self) -> Sandbox {
        Sandbox::off()
            .with_env(self.broker.child_env())
            .with_egress(Some(self.egress.clone()))
    }

    /// The reply the plugin saw for one host call, out of its fence.
    async fn op(&self, sandbox: &Sandbox, request: Value) -> Value {
        let ctx = ToolContext {
            workspace: self.ws.path().to_path_buf(),
            max_output_chars: 30_000,
        };
        let out = host_tool(sandbox)
            .call(request, &ctx)
            .await
            .unwrap()
            .content;
        assert!(out.contains("untrusted=\"true\""), "{out}");
        assert!(
            !out.contains(TOKEN),
            "the real token reached the plugin: {out}"
        );
        let body = &out[out.find('\n').unwrap() + 1..out.rfind('\n').unwrap()];
        serde_json::from_str(&body.replace("&lt;", "<").replace("&gt;", ">")).unwrap()
    }

    fn get(&self, host: &str) -> Value {
        json!({
            "op": "http",
            "url": format!("https://{host}:{}/who", self.port),
            "headers": {"authorization": "Bearer ${TOKEN}"},
        })
    }
}

#[tokio::test]
async fn the_proxy_swaps_the_placeholder_in_and_the_plugin_never_sees_the_token() {
    let s = setup().await;
    let placeholder = s.broker.secrets()[0].placeholder.clone();
    assert_ne!(placeholder, TOKEN);

    let reply = s.op(&s.proxied(), s.get("127.0.0.1")).await;
    let body = reply["ok"]["body"]
        .as_str()
        .unwrap_or_else(|| panic!("{reply}"));
    assert_eq!(reply["ok"]["status"], 200, "{reply}");
    // The origin got the real token; what came back names the placeholder.
    assert!(body.starts_with("real=true"), "{body}");
    assert!(body.contains(&placeholder), "{body}");
}

#[tokio::test]
async fn an_undeclared_domain_is_refused_before_any_request() {
    let s = setup().await;
    let reply = s.op(&s.proxied(), s.get("localhost")).await;
    let err = reply["error"].as_str().unwrap_or_else(|| panic!("{reply}"));
    assert!(err.contains("isn't one of this plugin's domains"), "{err}");
}

#[tokio::test]
async fn without_the_proxy_there_is_no_secret_and_no_route() {
    let s = setup().await;
    // No egress: the secret can't be used at all...
    let reply = s.op(&Sandbox::off(), s.get("127.0.0.1")).await;
    let err = reply["error"].as_str().unwrap_or_else(|| panic!("{reply}"));
    assert!(err.contains("credential proxy isn't running"), "{err}");
    // ...and a plain request can't reach the origin, whose CA only the
    // proxy trusts: the successful call above went through the proxy.
    let reply = s
        .op(
            &Sandbox::off(),
            json!({"op": "http", "url": format!("https://127.0.0.1:{}/who", s.port)}),
        )
        .await;
    assert!(reply["error"].is_string(), "{reply}");
}
