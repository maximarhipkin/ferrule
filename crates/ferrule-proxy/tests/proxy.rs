//! The proxy end to end, against a local HTTPS origin that checks for itself
//! whether the real secrets arrived and echoes them back.

use bytes::Bytes;
use ferrule_proxy::{Broker, BrokerConfig, SecretRule, Upstream};
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "ghp_REALtokenREALtokenREALtokenREAL1234";
const SHORT: &str = "hunter2";

/// An HTTPS server for 127.0.0.1 and localhost, with its own CA.
async fn origin() -> (u16, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".into(), "localhost".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let chain = vec![CertificateDer::from(leaf.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service_fn(respond))
                    .await;
            });
        }
    });
    (port, ca_cert.pem())
}

async fn respond(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let h = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("x-access-token:{TOKEN}")
        )
    );
    let checks = [
        ("bearer", h("authorization") == format!("Bearer {TOKEN}")),
        ("basic", h("x-auth-basic") == basic),
        ("api_key", h("x-api-key") == SHORT),
        // Not a credential header, so the placeholder arrives as sent.
        (
            "title_untouched",
            h("x-title").starts_with("ghp_") && h("x-title") != TOKEN,
        ),
        ("path", req.uri().path() == format!("/repos/{TOKEN}/check")),
        // TOKEN may go in URLs, SHORT may not.
        (
            "query",
            req.uri().query().is_some_and(|q| {
                q.starts_with(&format!("key={TOKEN}&note=")) && !q.contains(SHORT)
            }),
        ),
        (
            "gzip_stripped",
            req.headers().get("accept-encoding").is_none(),
        ),
    ];
    let mut body: String = checks.iter().map(|(n, ok)| format!("{n}={ok} ")).collect();
    body.push_str(&format!("echo={TOKEN} short={SHORT}"));
    if req.uri().path() == "/big" {
        body = format!("{TOKEN}-").repeat(5000);
    }
    let resp = Response::builder()
        .header("x-echo", TOKEN)
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    Ok(resp)
}

struct Setup {
    broker: Broker,
    origin_port: u16,
    client: reqwest::Client,
    placeholders: BTreeMap<String, String>,
    _dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let (origin_port, origin_ca) = origin().await;
    let dir = tempfile::tempdir().unwrap();
    // Stands in for the system bundle: ferrule trusts it upstream, and so do
    // commands, since theirs is this plus ferrule's CA.
    let base = dir.path().join("system.pem");
    std::fs::write(&base, &origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([
            (
                "TOKEN".to_string(),
                SecretRule {
                    hosts: vec!["127.0.0.1".to_string()],
                    in_url: true,
                },
            ),
            ("SHORT".to_string(), vec!["127.0.0.1".to_string()].into()),
        ]),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        ca_bundle: Some(base),
    };
    let lookup = |name: &str| match name {
        "TOKEN" => Some(TOKEN.to_string()),
        "SHORT" => Some(SHORT.to_string()),
        _ => None,
    };
    let broker = Broker::start(cfg, lookup).unwrap().unwrap();
    let env: BTreeMap<String, String> = broker.child_env().into_iter().collect();
    let bundle = std::fs::read(&env["SSL_CERT_FILE"]).unwrap();
    let mut builder = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&env["HTTPS_PROXY"]).unwrap())
        .tls_built_in_root_certs(false);
    for cert in reqwest::Certificate::from_pem_bundle(&bundle).unwrap() {
        builder = builder.add_root_certificate(cert);
    }
    let placeholders = broker
        .secrets()
        .iter()
        .map(|s| (s.name.clone(), s.placeholder.clone()))
        .collect();
    Setup {
        broker,
        origin_port,
        client: builder.build().unwrap(),
        placeholders,
        _dir: dir,
    }
}

#[tokio::test]
async fn placeholders_become_real_values_and_real_values_come_back_as_placeholders() {
    let s = setup().await;
    let (tok, short) = (&s.placeholders["TOKEN"], &s.placeholders["SHORT"]);
    assert_eq!(tok.len(), TOKEN.len());
    assert!(tok.starts_with("ghp_") && tok != TOKEN);
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("x-access-token:{tok}"),
    );
    let url = format!(
        "https://127.0.0.1:{}/repos/{tok}/check?key={tok}&note={short}",
        s.origin_port
    );
    // Twice: the second request reuses the intercepted connection.
    for _ in 0..2 {
        let resp = s
            .client
            .get(&url)
            .bearer_auth(tok)
            .header("x-auth-basic", format!("Basic {basic}"))
            .header("x-api-key", short)
            .header("x-title", tok)
            .header("accept-encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()["x-echo"],
            tok.as_str(),
            "response headers are scrubbed"
        );
        let body = resp.text().await.unwrap();
        for check in [
            "bearer",
            "basic",
            "api_key",
            "title_untouched",
            "path",
            "query",
            "gzip_stripped",
        ] {
            assert!(
                body.contains(&format!("{check}=true")),
                "{check} failed: {body}"
            );
        }
        assert!(
            body.ends_with(&format!("echo={tok} short={short}")),
            "{body}"
        );
        assert!(!body.contains(TOKEN) && !body.contains(SHORT));
    }

    let big = s
        .client
        .get(format!("https://127.0.0.1:{}/big", s.origin_port))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        big,
        format!("{tok}-").repeat(5000),
        "scrubbed across chunk boundaries"
    );
}

#[tokio::test]
async fn other_hosts_get_a_blind_tunnel_so_the_placeholder_is_useless_there() {
    let s = setup().await;
    let tok = &s.placeholders["TOKEN"];
    // Same server, but `localhost` has no secrets bound.
    let resp = s
        .client
        .get(format!("https://localhost:{}/check", s.origin_port))
        .bearer_auth(tok)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("bearer=false"), "{body}");
    // Untouched both ways: the origin's own echo arrives as sent.
    assert!(body.contains(&format!("echo={TOKEN}")), "{body}");
}

#[tokio::test]
async fn a_host_header_for_another_site_is_refused() {
    let s = setup().await;
    let resp = s
        .client
        .get(format!("https://127.0.0.1:{}/check", s.origin_port))
        .header("host", "evil.example")
        .bearer_auth(&s.placeholders["TOKEN"])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 421);
}

async fn raw(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test]
async fn the_proxy_wants_its_credentials_and_only_tunnels() {
    let s = setup().await;
    let port = s.broker.addr().port();
    let target = format!("127.0.0.1:{}", s.origin_port);
    let no_creds = raw(
        port,
        &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
    )
    .await;
    assert!(no_creds.starts_with("HTTP/1.1 407"), "{no_creds}");
    assert!(
        no_creds.contains("proxy-authenticate: Basic realm=\"ferrule\""),
        "{no_creds}"
    );

    let url = s.broker.proxy_url();
    let creds = url.trim_start_matches("http://").split('@').next().unwrap();
    let auth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, creds);
    let plain = raw(
        port,
        &format!("GET http://{target}/ HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(plain.starts_with("HTTP/1.1 400"), "{plain}");

    let dead = raw(
        port,
        &format!("CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(dead.starts_with("HTTP/1.1 502"), "{dead}");
}

/// Through this machine's real upstream proxy to a public echo service,
/// with curl as the sandboxed command would run it. Needs network access:
/// `cargo test -p ferrule-proxy -- --ignored`.
#[tokio::test]
#[ignore]
async fn curl_through_the_real_network() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([(
            "DEMO_PASSWORD".to_string(),
            vec!["httpbin.org".to_string()].into(),
        )]),
        state_dir: dir.path().to_path_buf(),
        upstream: Upstream::from_env().unwrap(),
        ca_bundle: None,
    };
    let broker = Broker::start(cfg, |_| Some("passwd".to_string()))
        .unwrap()
        .unwrap();
    let env = broker.child_env();
    let placeholder = broker.secrets()[0].placeholder.clone();
    let curl = |url: &str| {
        let (url, env, ph) = (url.to_string(), env.clone(), placeholder.clone());
        async move {
            tokio::task::spawn_blocking(move || {
                let out = std::process::Command::new("curl")
                    .args(["-sS", "--max-time", "30", "-w", " HTTP:%{http_code}"])
                    .args(["-u", &format!("user:{ph}"), &url])
                    .env_remove("NO_PROXY")
                    .env_remove("no_proxy")
                    .envs(env)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).into_owned()
                    + &String::from_utf8_lossy(&out.stderr)
            })
            .await
            .unwrap()
        }
    };
    let bound = curl("https://httpbin.org/basic-auth/user/passwd").await;
    assert!(
        bound.contains("\"authenticated\": true") && bound.ends_with("HTTP:200"),
        "{bound}"
    );
    // Reached, through the blind tunnel, and refused: it got the placeholder.
    let unbound = curl("https://httpbingo.org/basic-auth/user/passwd").await;
    assert!(unbound.ends_with("HTTP:401"), "{unbound}");
}
