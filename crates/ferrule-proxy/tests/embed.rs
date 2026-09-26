//! M30: the `/v1/embeddings` backend through the credential proxy, against
//! a local origin shaped like OpenAI's API. The embedder holds only the
//! placeholder, and the origin checks that the real key arrived, so an
//! embedding that works went through the proxy.

mod common;

use bytes::Bytes;
use common::Reply;
use ferrule_embed::{EmbedError, Embedder, OpenAiEmbedder, OpenAiSettings, Purpose};
use ferrule_proxy::{Broker, BrokerConfig, SecretRule};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "sk-REALembedKEYrealEMBEDkey1234";

type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

fn reply(status: StatusCode, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Reply {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    Ok(b.body(Full::new(body.into())).unwrap())
}

/// An origin that logs (path, bearer, body) and answers with `answer`.
async fn origin(
    answer: impl Fn(&str, &Value) -> Reply + Send + Sync + 'static,
) -> (u16, String, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let answer = Arc::new(answer);
    let (port, ca) = common::tls_origin(move |req: Request<Incoming>| {
        let log = log.clone();
        let answer = answer.clone();
        async move {
            let path = req.uri().path().to_string();
            let bearer = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_string();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            log.lock()
                .unwrap()
                .push((path, bearer.clone(), body.clone()));
            answer(&bearer, &body)
        }
    })
    .await;
    (port, ca, seen)
}

struct Setup {
    broker: Broker,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

fn proxy(origin_ca: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("system.pem");
    std::fs::write(&base, origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([(
            "EMBED_KEY".to_string(),
            SecretRule {
                hosts: vec!["127.0.0.1".to_string()],
                in_url: false,
            },
        )]),
        state_dir: dir.path().join("proxy"),
        upstream: None,
        http_upstream: None,
        ca_bundle: Some(base),
    };
    let broker = Broker::start(cfg, |n| (n == "EMBED_KEY").then(|| KEY.to_string()))
        .unwrap()
        .unwrap();
    let egress = ferrule_sandbox::Egress {
        proxy_url: broker.proxy_url(),
        ca_cert_pem: std::fs::read_to_string(broker.ca_cert_path()).unwrap(),
    };
    let client = ferrule_tools::egress::client_builder(Some(&egress))
        .unwrap()
        .build()
        .unwrap();
    Setup {
        broker,
        client,
        _dir: dir,
    }
}

fn embedder(s: &Setup, port: u16, dim: usize, key: Option<String>) -> OpenAiEmbedder {
    OpenAiEmbedder::new(
        OpenAiSettings {
            base_url: format!("https://127.0.0.1:{port}/v1"),
            model: "text-embedding-3-small".into(),
            dim,
            send_dimensions: true,
            key,
            key_env: Some("EMBED_KEY".into()),
            timeout: Duration::from_secs(10),
        },
        s.client.clone(),
    )
}

/// OpenAI's answer shape, with the items out of order on purpose.
fn vectors(bearer: &str, body: &Value) -> Reply {
    if bearer != KEY {
        return reply(
            StatusCode::UNAUTHORIZED,
            &[],
            r#"{"error":{"message":"bad key"}}"#,
        );
    }
    let n = body["input"].as_array().map(Vec::len).unwrap_or(0);
    let data: Vec<Value> = (0..n)
        .rev()
        .map(|i| json!({"object": "embedding", "index": i, "embedding": [i as f32 + 1.0, 0.0, 0.0, 1.0]}))
        .collect();
    let answer = json!({"object": "list", "data": data, "model": body["model"],
                        "usage": {"prompt_tokens": 7 * n, "total_tokens": 7 * n}});
    reply(
        StatusCode::OK,
        &[("content-type", "application/json")],
        answer.to_string(),
    )
}

fn texts(t: &[&str]) -> Vec<String> {
    t.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn embeds_through_the_proxy_with_the_real_key_swapped_in() {
    let (port, ca, seen) = origin(vectors).await;
    let s = proxy(&ca);
    let placeholder = s.broker.secrets()[0].placeholder.clone();
    assert_ne!(placeholder, KEY);
    let e = embedder(&s, port, 4, Some(placeholder));
    assert_eq!(e.model().as_str(), "openai:text-embedding-3-small/4");

    let out = e
        .embed(&texts(&["first fact", "second fact"]), Purpose::Document)
        .await
        .unwrap();
    assert_eq!(out.tokens, 14);
    // Sorted back by index, and normalised.
    let n0 = (1.0f32 + 1.0).sqrt();
    assert!(
        (out.vectors[0][0] - 1.0 / n0).abs() < 1e-6,
        "{:?}",
        out.vectors
    );
    let n1 = (4.0f32 + 1.0).sqrt();
    assert!(
        (out.vectors[1][0] - 2.0 / n1).abs() < 1e-6,
        "{:?}",
        out.vectors
    );

    let seen = seen.lock().unwrap();
    let (path, bearer, body) = &seen[0];
    assert_eq!(path, "/v1/embeddings");
    assert_eq!(bearer, KEY);
    assert_eq!(body["model"], "text-embedding-3-small");
    assert_eq!(body["input"], json!(["first fact", "second fact"]));
    assert_eq!(body["dimensions"], 4);
}

#[tokio::test]
async fn a_refused_key_is_unauthorized_and_never_shows_the_key() {
    let (port, ca, _) = origin(vectors).await;
    let s = proxy(&ca);
    // Not the placeholder: the proxy has nothing to swap, the origin says 401.
    let e = embedder(&s, port, 4, Some("sk-not-the-placeholder".into()));
    let err = e.embed(&texts(&["x"]), Purpose::Query).await.unwrap_err();
    assert!(
        matches!(err, EmbedError::Unauthorized { status: 401, .. }),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("refused the key") && msg.contains("EMBED_KEY"),
        "{msg}"
    );
    assert!(!msg.contains(KEY), "{msg}");
    assert_eq!(err.kind(), "unauthorized");
}

#[tokio::test]
async fn rate_limiting_is_reported_with_retry_after() {
    let (port, ca, _) = origin(|_, _| {
        reply(
            StatusCode::TOO_MANY_REQUESTS,
            &[("retry-after", "7")],
            r#"{"error":{"message":"slow down"}}"#,
        )
    })
    .await;
    let s = proxy(&ca);
    let e = embedder(&s, port, 4, Some(s.broker.secrets()[0].placeholder.clone()));
    let err = e.embed(&texts(&["x"]), Purpose::Query).await.unwrap_err();
    match &err {
        EmbedError::RateLimited { retry_after, .. } => assert_eq!(*retry_after, Some(7)),
        other => panic!("{other:?}"),
    }
    assert!(err.to_string().contains("retry after 7s"), "{err}");
}

#[tokio::test]
async fn a_vector_of_the_wrong_dimension_is_refused() {
    let (port, ca, _) = origin(vectors).await;
    let s = proxy(&ca);
    // The origin answers 4 floats; this embedder was told 8.
    let e = embedder(&s, port, 8, Some(s.broker.secrets()[0].placeholder.clone()));
    let err = e.embed(&texts(&["x"]), Purpose::Query).await.unwrap_err();
    assert!(matches!(err, EmbedError::BadAnswer(_)), "{err:?}");
}

#[tokio::test]
async fn a_server_error_keeps_its_status_and_a_short_body() {
    let (port, ca, _) = origin(|_, _| reply(StatusCode::BAD_GATEWAY, &[], "x".repeat(5000))).await;
    let s = proxy(&ca);
    let e = embedder(&s, port, 4, Some(s.broker.secrets()[0].placeholder.clone()));
    let err = e.embed(&texts(&["x"]), Purpose::Query).await.unwrap_err();
    match &err {
        EmbedError::Http { status, body, .. } => {
            assert_eq!(*status, 502);
            assert!(body.len() <= 300);
        }
        other => panic!("{other:?}"),
    }
}
