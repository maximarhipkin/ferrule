//! M28: `web_search` through the credential proxy, against a local origin
//! shaped like each provider's API. The origin's CA is trusted only by the
//! proxy, the tool holds only the placeholder, and the origin checks that
//! the real key arrived: a search that works went through the proxy.

mod common;

use bytes::Bytes;
use common::Reply;
use ferrule_core::tool::{Tool, ToolContext};
use ferrule_proxy::{Broker, BrokerConfig, SecretRule};
use ferrule_sandbox::Egress;
use ferrule_tools::search::{SearchCall, SearchProvider, SearchSettings};
use ferrule_tools::WebSearchTool;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const KEY: &str = "BSA-REALkeyREALkeyREALkeyREAL5678";

/// What the origin saw of the last request.
#[derive(Default, Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: String,
    key_header: String,
    body: Value,
}

type Log = Arc<Mutex<Option<Seen>>>;

fn reply(status: StatusCode, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Reply {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    Ok(b.body(Full::new(body.into())).unwrap())
}

/// The header each provider reads its key from, `Bearer ` stripped.
fn key_header(req: &Request<Incoming>) -> String {
    for name in ["x-subscription-token", "x-api-key", "authorization"] {
        if let Some(v) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
            return v.trim_start_matches("Bearer ").to_string();
        }
    }
    String::new()
}

/// An origin that logs each request and answers with `answer(seen)`.
async fn origin(answer: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> (u16, String, Log) {
    let log: Log = Arc::default();
    let answer = Arc::new(answer);
    let seen_log = log.clone();
    let (port, ca) = common::tls_origin(move |req: Request<Incoming>| {
        let log = seen_log.clone();
        let answer = answer.clone();
        async move {
            let mut seen = Seen {
                method: req.method().to_string(),
                path: req.uri().path().to_string(),
                query: req.uri().query().unwrap_or_default().to_string(),
                key_header: key_header(&req),
                body: Value::Null,
            };
            let body = req.into_body().collect().await.unwrap().to_bytes();
            seen.body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            *log.lock().unwrap() = Some(seen.clone());
            answer(&seen)
        }
    })
    .await;
    (port, ca, log)
}

struct Setup {
    broker: Broker,
    egress: Egress,
    _dir: tempfile::TempDir,
}

/// A proxy that binds KEY to 127.0.0.1 and trusts `origin_ca` upstream.
fn proxy(origin_ca: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("system.pem");
    std::fs::write(&base, origin_ca).unwrap();
    let cfg = BrokerConfig {
        secrets: BTreeMap::from([(
            "SEARCH_KEY".to_string(),
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
    let broker = Broker::start(cfg, |n| (n == "SEARCH_KEY").then(|| KEY.to_string()))
        .unwrap()
        .unwrap();
    let egress = Egress {
        proxy_url: broker.proxy_url(),
        ca_cert_pem: std::fs::read_to_string(broker.ca_cert_path()).unwrap(),
    };
    Setup {
        broker,
        egress,
        _dir: dir,
    }
}

type Calls = Arc<Mutex<Vec<SearchCall>>>;

fn tool(p: SearchProvider, port: u16, s: &Setup, keyed: bool) -> (WebSearchTool, Calls) {
    let mut settings = SearchSettings::new(p, format!("https://127.0.0.1:{port}"));
    if keyed {
        settings.key = Some(s.broker.secrets()[0].placeholder.clone());
        settings.key_env = Some("SEARCH_KEY".into());
    }
    let calls: Calls = Arc::default();
    let log = calls.clone();
    let t = WebSearchTool::new(settings, Some(s.egress.clone()))
        .with_recorder(Arc::new(move |c| log.lock().unwrap().push(c)));
    (t, calls)
}

/// Each provider's answer, if the real key came; a 401 if it didn't.
fn provider_answer(p: SearchProvider, keyed: bool) -> impl Fn(&Seen) -> Reply {
    move |seen: &Seen| {
        if keyed && seen.key_header != KEY {
            return reply(StatusCode::UNAUTHORIZED, &[], "bad key");
        }
        let body = match p {
            SearchProvider::Brave => json!({"web": {"results": [
                {"title": "Rust <strong>Book</strong>", "url": "https://doc.rust-lang.org/book/",
                 "description": "Learn <strong>Rust</strong> &amp; more", "page_age": "2026-01-02T00:00:00"},
                {"title": "Crates", "url": "https://crates.io/", "description": "registry"},
            ]}}),
            SearchProvider::Tavily => json!({"results": [
                {"title": "Rust Book", "url": "https://doc.rust-lang.org/book/",
                 "content": "Learn Rust & more", "published_date": "2026-01-02"},
                {"title": "Crates", "url": "https://crates.io/", "content": "registry"},
            ]}),
            SearchProvider::Searxng => json!({"results": [
                {"title": "Rust Book", "url": "https://doc.rust-lang.org/book/",
                 "content": "Learn Rust & more", "publishedDate": "2026-01-02"},
                {"title": "Crates", "url": "https://crates.io/", "content": "registry"},
            ]}),
            SearchProvider::Exa => json!({"results": [
                {"title": "Rust Book", "url": "https://doc.rust-lang.org/book/",
                 "highlights": ["Learn Rust & more"], "publishedDate": "2026-01-02"},
                {"title": "Crates", "url": "https://crates.io/", "text": "registry"},
            ]}),
        };
        reply(
            StatusCode::OK,
            &[("content-type", "application/json")],
            body.to_string(),
        )
    }
}

async fn search_with(p: SearchProvider, keyed: bool) -> (String, Seen, Vec<SearchCall>) {
    let (port, ca, log) = origin(provider_answer(p, keyed)).await;
    let s = proxy(&ca);
    let (t, calls) = tool(p, port, &s, keyed);
    let out = t
        .call(
            json!({"query": "rust book", "count": 2}),
            &ToolContext::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{p:?}: {e}"));
    let seen = log.lock().unwrap().clone().expect("the origin was asked");
    let calls = std::mem::take(&mut *calls.lock().unwrap());
    (out.content, seen, calls)
}

fn assert_results(p: SearchProvider, out: &str, calls: &[SearchCall]) {
    assert!(
        out.contains(&format!("provider=\"{}\"", p.name())) && out.contains("untrusted=\"true\""),
        "{out}"
    );
    assert!(out.contains("1. Rust Book"), "{out}");
    assert!(out.contains("https://doc.rust-lang.org/book/"), "{out}");
    assert!(out.contains("2026-01-02"), "{out}");
    assert!(out.contains("2. Crates"), "{out}");
    assert!(!out.contains(KEY), "{out}");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].error.is_none(), "{:?}", calls[0]);
}

#[tokio::test]
async fn brave_gets_the_real_key_in_its_header() {
    let (out, seen, calls) = search_with(SearchProvider::Brave, true).await;
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("GET", "/res/v1/web/search")
    );
    assert!(
        seen.query.contains("q=rust+book") && seen.query.contains("count=2"),
        "{}",
        seen.query
    );
    assert_eq!(seen.key_header, KEY);
    assert_results(SearchProvider::Brave, &out, &calls);
    // Brave's highlighting is stripped, entities decoded.
    assert!(out.contains("Learn Rust & more"), "{out}");
}

#[tokio::test]
async fn tavily_gets_the_real_key_as_a_bearer_and_none_in_the_body() {
    let (out, seen, calls) = search_with(SearchProvider::Tavily, true).await;
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/search")
    );
    assert_eq!(seen.body["query"], "rust book");
    assert_eq!(seen.body["max_results"], 2);
    assert!(!seen.body.to_string().contains("BSA-"), "{}", seen.body);
    assert_eq!(seen.key_header, KEY);
    assert_results(SearchProvider::Tavily, &out, &calls);
}

#[tokio::test]
async fn exa_gets_the_real_key_in_x_api_key() {
    let (out, seen, calls) = search_with(SearchProvider::Exa, true).await;
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/search")
    );
    assert_eq!(seen.body["numResults"], 2);
    assert_eq!(seen.key_header, KEY);
    assert_results(SearchProvider::Exa, &out, &calls);
}

#[tokio::test]
async fn searxng_searches_with_no_key_and_with_one_behind_auth() {
    let (out, seen, calls) = search_with(SearchProvider::Searxng, false).await;
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("GET", "/search")
    );
    assert!(seen.query.contains("format=json"), "{}", seen.query);
    assert!(seen.key_header.is_empty());
    assert_results(SearchProvider::Searxng, &out, &calls);

    let (out, seen, calls) = search_with(SearchProvider::Searxng, true).await;
    assert_eq!(seen.key_header, KEY);
    assert_results(SearchProvider::Searxng, &out, &calls);
}

/// Without the proxy the origin's certificate isn't trusted, so the
/// searches above can only have gone through it.
#[tokio::test]
async fn a_search_that_skips_the_proxy_fails() {
    let (port, _ca, log) = origin(provider_answer(SearchProvider::Searxng, false)).await;
    let settings =
        SearchSettings::new(SearchProvider::Searxng, format!("https://127.0.0.1:{port}"));
    let err = WebSearchTool::new(settings, None)
        .call(json!({"query": "rust"}), &ToolContext::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("couldn't be reached"), "{err}");
    assert!(log.lock().unwrap().is_none());
}

async fn error_for(
    answer: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
) -> (String, Vec<SearchCall>) {
    let (port, ca, _log) = origin(answer).await;
    let s = proxy(&ca);
    let (t, calls) = tool(SearchProvider::Brave, port, &s, true);
    let err = t
        .call(json!({"query": "rust"}), &ToolContext::default())
        .await
        .unwrap_err()
        .to_string();
    let calls = std::mem::take(&mut *calls.lock().unwrap());
    (err, calls)
}

#[tokio::test]
async fn a_401_says_which_key_to_check() {
    let (err, calls) = error_for(|_| reply(StatusCode::UNAUTHORIZED, &[], "nope")).await;
    assert!(
        err.contains("HTTP 401") && err.contains("SEARCH_KEY"),
        "{err}"
    );
    let kind = calls[0].error.as_ref().unwrap().0.clone();
    assert_eq!(kind, "http_401");
}

#[tokio::test]
async fn a_429_passes_on_retry_after() {
    let (err, calls) = error_for(|_| {
        reply(
            StatusCode::TOO_MANY_REQUESTS,
            &[("retry-after", "17")],
            "slow down",
        )
    })
    .await;
    assert!(
        err.contains("HTTP 429") && err.contains("wait 17 s"),
        "{err}"
    );
    assert_eq!(calls[0].error.as_ref().unwrap().0, "http_429");

    let (err, _) = error_for(|_| reply(StatusCode::TOO_MANY_REQUESTS, &[], "")).await;
    assert!(err.contains("try again later"), "{err}");
}

/// An error page that echoes the key the origin got: the proxy scrubs the
/// real one out before the tool (and the model) could see it.
#[tokio::test]
async fn an_error_body_echoing_the_key_never_shows_it() {
    let (err, _) = error_for(|seen| {
        reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            &[],
            format!("internal error for token {}", seen.key_header),
        )
    })
    .await;
    assert!(
        err.contains("HTTP 500") && err.contains("internal error"),
        "{err}"
    );
    assert!(!err.contains(KEY), "{err}");
}

#[tokio::test]
async fn no_results_say_so() {
    let (port, ca, _log) = origin(|_| {
        reply(
            StatusCode::OK,
            &[],
            json!({"web": {"results": []}}).to_string(),
        )
    })
    .await;
    let s = proxy(&ca);
    let (t, calls) = tool(SearchProvider::Brave, port, &s, true);
    let out = t
        .call(json!({"query": "zzzqqq"}), &ToolContext::default())
        .await
        .unwrap();
    assert!(
        out.content.contains("No results for this query."),
        "{}",
        out.content
    );
    assert!(calls.lock().unwrap()[0].error.is_none());
}

#[tokio::test]
async fn a_long_answer_is_trimmed_to_the_budget() {
    let long = "word ".repeat(400);
    let results: Vec<Value> = (0..20)
        .map(|i| json!({"title": format!("Result {i}"), "url": format!("https://e.example/{i}"), "description": long}))
        .collect();
    let body = json!({"web": {"results": results}}).to_string();
    let (port, ca, _log) = origin(move |_| reply(StatusCode::OK, &[], body.clone())).await;
    let s = proxy(&ca);
    let mut settings =
        SearchSettings::new(SearchProvider::Brave, format!("https://127.0.0.1:{port}"));
    settings.key = Some(s.broker.secrets()[0].placeholder.clone());
    settings.max_results = 20;
    settings.max_output_tokens = 300;
    let out = WebSearchTool::new(settings, Some(s.egress.clone()))
        .call(json!({"query": "many"}), &ToolContext::default())
        .await
        .unwrap()
        .content;
    assert!(out.len() <= 300 * 4 + 200, "{} chars", out.len());
    assert!(out.contains("1. Result 0"), "{out}");
    assert!(out.contains("trimmed to fit the output budget"), "{out}");
    assert!(out.trim_end().ends_with("</web_search_results>"), "{out}");
}

#[tokio::test]
async fn a_closed_gate_sends_nothing() {
    let (port, ca, log) = origin(provider_answer(SearchProvider::Brave, true)).await;
    let s = proxy(&ca);
    let (t, calls) = tool(SearchProvider::Brave, port, &s, true);
    let t = t.with_gate(Arc::new(|| {
        Err("the daily search cap of 3 is reached".into())
    }));
    let err = t
        .call(json!({"query": "rust"}), &ToolContext::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("daily search cap"), "{err}");
    assert!(log.lock().unwrap().is_none());
    assert!(calls.lock().unwrap().is_empty());
}

/// A real search through the proxy: the key from `var`, bound to the
/// provider's host. Run by hand; docs/web-search.md has the commands.
async fn live(p: SearchProvider, var: &str, endpoint: Option<String>) {
    let endpoint = endpoint
        .or_else(|| p.default_endpoint().map(str::to_string))
        .expect("an endpoint");
    let host = endpoint
        .split("://")
        .nth(1)
        .unwrap()
        .split(['/', ':'])
        .next()
        .unwrap()
        .to_string();
    let key = std::env::var(var).ok();
    if p.needs_key() {
        assert!(key.is_some(), "set {var}");
    }
    let dir = tempfile::tempdir().unwrap();
    let secrets = match &key {
        Some(_) => BTreeMap::from([(
            var.to_string(),
            SecretRule {
                hosts: vec![host],
                in_url: false,
            },
        )]),
        None => BTreeMap::new(),
    };
    let cfg = BrokerConfig {
        secrets,
        state_dir: dir.path().join("proxy"),
        upstream: ferrule_proxy::Upstream::from_env().unwrap(),
        http_upstream: ferrule_proxy::Upstream::from_env_http().unwrap(),
        ca_bundle: None,
    };
    let (placeholder, egress) = match key.clone() {
        Some(k) => {
            let broker = Broker::start(cfg, move |n| (n == var).then(|| k.clone()))
                .unwrap()
                .unwrap();
            let egress = Egress {
                proxy_url: broker.proxy_url(),
                ca_cert_pem: std::fs::read_to_string(broker.ca_cert_path()).unwrap(),
            };
            let placeholder = broker.secrets()[0].placeholder.clone();
            std::mem::forget(broker);
            (Some(placeholder), Some(egress))
        }
        None => (None, None),
    };
    let mut settings = SearchSettings::new(p, endpoint);
    settings.key = placeholder;
    settings.key_env = key.as_ref().map(|_| var.to_string());
    let out = WebSearchTool::new(settings, egress)
        .call(
            json!({"query": "rust programming language", "count": 3}),
            &ToolContext::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    println!("{}", out.content);
    assert!(out.content.contains("1. "), "{}", out.content);
    assert!(out.content.contains("https://"), "{}", out.content);
    if let Some(k) = key {
        assert!(!out.content.contains(&k));
    }
}

#[tokio::test]
#[ignore = "live: a paid search; needs BRAVE_API_KEY"]
async fn live_brave() {
    live(SearchProvider::Brave, "BRAVE_API_KEY", None).await;
}

#[tokio::test]
#[ignore = "live: a paid search; needs TAVILY_API_KEY"]
async fn live_tavily() {
    live(SearchProvider::Tavily, "TAVILY_API_KEY", None).await;
}

#[tokio::test]
#[ignore = "live: a paid search; needs EXA_API_KEY"]
async fn live_exa() {
    live(SearchProvider::Exa, "EXA_API_KEY", None).await;
}

#[tokio::test]
#[ignore = "live: needs SEARXNG_URL, an instance with format=json on (SEARXNG_KEY if it wants a bearer)"]
async fn live_searxng() {
    let url = std::env::var("SEARXNG_URL").expect("set SEARXNG_URL");
    live(SearchProvider::Searxng, "SEARXNG_KEY", Some(url)).await;
}
