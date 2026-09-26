//! The proxy end to end, against a local HTTPS origin that checks for itself
//! whether the real secrets arrived and echoes them back.

mod common;

use bytes::Bytes;
use ferrule_proxy::{Broker, BrokerConfig, SecretRule, Upstream};
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use std::collections::BTreeMap;
use std::convert::Infallible;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

const TOKEN: &str = "ghp_REALtokenREALtokenREALtokenREAL1234";
const SHORT: &str = "hunter2";

async fn origin() -> (u16, String) {
    common::tls_origin(respond).await
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
        http_upstream: None,
        ca_bundle: Some(base),
        egress: None,
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

/// M17: a secret bound into the running proxy is swapped on the next
/// tunnel, and commands started from then on get its placeholder.
#[tokio::test]
async fn a_secret_bound_while_running_is_swapped_from_then_on() {
    let s = setup().await;
    let rule = SecretRule {
        hosts: vec!["localhost".to_string()],
        in_url: false,
    };
    assert!(s.broker.bind("LATE-KEY", &rule, TOKEN).is_err());
    assert!(s
        .broker
        .bind("LATE", &SecretRule::default(), TOKEN)
        .is_err());
    s.broker.bind("LATE", &rule, TOKEN).unwrap();
    let env: BTreeMap<String, String> = s.broker.child_env().into_iter().collect();
    let late = &env["LATE"];
    assert!(late != TOKEN && late != &s.placeholders["TOKEN"]);
    assert_eq!(env["TOKEN"], s.placeholders["TOKEN"], "the others stay");
    assert!(s.broker.model_note().contains("- $LATE: localhost"));

    let resp = s
        .client
        .get(format!("https://localhost:{}/check", s.origin_port))
        .bearer_auth(late)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("bearer=true"), "{body}");
    assert!(!body.contains(TOKEN), "scrubbed on the way back: {body}");

    // A new value: a new placeholder, and the old one still works.
    s.broker
        .bind("LATE", &rule, "ghp_another_value_000000000000000000")
        .unwrap();
    let infos = s.broker.secrets();
    let now = infos.iter().find(|i| i.name == "LATE").unwrap();
    assert_ne!(&now.placeholder, late);
    assert_eq!(infos.len(), 3);
    let resp = s
        .client
        .get(format!("https://localhost:{}/check", s.origin_port))
        .bearer_auth(late)
        .send()
        .await
        .unwrap();
    assert!(resp.text().await.unwrap().contains("bearer=true"));
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
async fn the_proxy_wants_its_credentials_and_a_full_target() {
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
    let origin_form = raw(
        port,
        &format!("GET / HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(origin_form.starts_with("HTTP/1.1 400"), "{origin_form}");
    let https_url = raw(
        port,
        &format!("GET https://{target}/ HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(https_url.starts_with("HTTP/1.1 400"), "{https_url}");

    let dead = raw(
        port,
        &format!("CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
    )
    .await;
    assert!(dead.starts_with("HTTP/1.1 502"), "{dead}");
}

/// M26: plain `http://` is forwarded rather than refused. A loopback server
/// with secrets bound gets them swapped like over HTTPS; a remote one is
/// refused, since the real value would cross the network in the clear.
#[tokio::test]
async fn plain_http_is_forwarded_and_secrets_only_go_to_loopback() {
    let s = setup().await;
    let plain = common::plain_origin(respond).await;
    let (tok, short) = (&s.placeholders["TOKEN"], &s.placeholders["SHORT"]);
    let resp = s
        .client
        .get(format!(
            "http://127.0.0.1:{plain}/repos/{tok}/check?key={tok}&note={short}"
        ))
        .bearer_auth(tok)
        .header("x-api-key", short)
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-echo"], tok.as_str());
    let body = resp.text().await.unwrap();
    for check in ["bearer", "api_key", "path", "query", "gzip_stripped"] {
        assert!(body.contains(&format!("{check}=true")), "{check}: {body}");
    }
    assert!(!body.contains(TOKEN) && !body.contains(SHORT), "{body}");

    // `localhost` has nothing bound: forwarded untouched both ways.
    let body = s
        .client
        .get(format!("http://localhost:{plain}/check"))
        .bearer_auth(tok)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("bearer=false"), "{body}");
    assert!(body.contains(&format!("echo={TOKEN}")), "{body}");

    // A remote host with a secret bound: refused before any connection.
    s.broker
        .bind(
            "REMOTE",
            &vec!["api.remote.example".to_string()].into(),
            TOKEN,
        )
        .unwrap();
    let resp = s
        .client
        .get("http://api.remote.example/v1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(resp.text().await.unwrap().contains("only go over HTTPS"));
}

/// Plain HTTP to an unbound host goes through the `HTTP_PROXY` ferrule was
/// started behind, in absolute form with that proxy's credentials (and not
/// ferrule's own), unless `NO_PROXY` covers the host.
#[tokio::test]
async fn plain_http_goes_through_the_upstream_http_proxy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = listener.local_addr().unwrap().port();
    let seen = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(sock.read_u8().await.unwrap());
        }
        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\n\r\nupstream")
            .await
            .unwrap();
        String::from_utf8(head).unwrap()
    });
    let plain = common::plain_origin(respond).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([("SHORT".to_string(), vec!["127.0.0.1".to_string()].into())]),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        http_upstream: Some(
            Upstream::parse(&format!("http://u:p@127.0.0.1:{up_port}"), "localhost").unwrap(),
        ),
        ca_bundle: None,
        egress: None,
    };
    let broker = Broker::start(cfg, |_| Some(SHORT.to_string()))
        .unwrap()
        .unwrap();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(broker.proxy_url()).unwrap())
        .build()
        .unwrap();

    let body = client
        .get("http://unbound.example/x?y=1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "upstream");
    let head = seen.await.unwrap();
    assert!(
        head.starts_with("GET http://unbound.example/x?y=1 HTTP/1.1\r\n"),
        "{head}"
    );
    let theirs = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "u:p");
    assert!(
        head.contains(&format!("proxy-authorization: Basic {theirs}")),
        "{head}"
    );
    assert_eq!(head.matches("proxy-authorization").count(), 1, "{head}");

    // NO_PROXY: straight to the server.
    let body = client
        .get(format!("http://localhost:{plain}/check"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("bearer=false"), "{body}");
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
        http_upstream: Upstream::from_env_http().unwrap(),
        ca_bundle: None,
        egress: None,
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
