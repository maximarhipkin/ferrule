//! `ferrule connections relay check` against a relay speaking the Worker's
//! contract, and `relay deploy` against a mock of Cloudflare's API.

mod common;

use common::*;
use ferrule_connections::relay::{self, Deploy, Relay};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const RELAY_KEY: &str = "relay-key-for-tests-0123456789";
const TOKEN: &str = "cf-token-SECRET-4242";

#[tokio::test]
async fn check_walks_every_step_against_the_relay() {
    let mock = MockRelay::start(RELAY_KEY).await;
    let steps = relay::check(&Relay::new(&mock.url, RELAY_KEY)).await;
    assert_eq!(steps.len(), 5, "{steps:?}");
    assert!(steps.iter().all(|(_, ok)| *ok), "{steps:?}");

    // A wrong key stops at the slot.
    let steps = relay::check(&Relay::new(&mock.url, "not-the-key")).await;
    assert_eq!(
        steps.iter().map(|(_, ok)| *ok).collect::<Vec<_>>(),
        [true, false]
    );
}

#[derive(Default)]
struct Cf {
    /// Script name → its migration tag.
    scripts: HashMap<String, Option<String>>,
    /// Each upload's metadata.
    uploads: Vec<Value>,
    subdomain_on: Vec<String>,
}

fn cloudflare(st: &Mutex<Cf>, r: Req) -> Resp {
    if r.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}")) {
        return Resp::json(
            403,
            json!({"success": false, "errors": [{"message": "Authentication error"}]}),
        );
    }
    let mut st = st.lock().unwrap();
    let base = "/client/v4/accounts/acct/workers";
    let Some(rest) = r.path.strip_prefix(base) else {
        return Resp::status(404);
    };
    match (r.method.as_str(), rest) {
        ("GET", "/scripts") => {
            let list: Vec<Value> = st
                .scripts
                .iter()
                .map(|(id, tag)| json!({"id": id, "migration_tag": tag}))
                .collect();
            Resp::json(200, json!({"success": true, "result": list}))
        }
        ("GET", "/subdomain") => Resp::json(
            200,
            json!({"success": true, "result": {"subdomain": "owner"}}),
        ),
        (m, path) if path.starts_with("/scripts/") => {
            let path = &path["/scripts/".len()..];
            if m == "PUT" {
                let body = String::from_utf8_lossy(&r.body).to_string();
                let meta = body
                    .split("\r\n\r\n")
                    .nth(1)
                    .and_then(|p| p.split("\r\n--").next())
                    .and_then(|m| serde_json::from_str::<Value>(m).ok())
                    .unwrap_or(Value::Null);
                if !body.contains("class Slot") {
                    return Resp::json(400, json!({"success": false}));
                }
                let had = st.scripts.get(path).cloned().flatten();
                let tag = meta["migrations"]["new_tag"].as_str().map(String::from);
                if had.is_some() && tag.is_some() {
                    return Resp::json(
                        400,
                        json!({"success": false, "errors": [{"message": "migration tag already applied"}]}),
                    );
                }
                st.scripts.insert(path.to_string(), had.or(tag));
                st.uploads.push(meta);
                Resp::json(200, json!({"success": true, "result": {}}))
            } else if let (true, Some(name)) = (m == "POST", path.strip_suffix("/subdomain")) {
                st.subdomain_on.push(name.to_string());
                Resp::json(200, json!({"success": true, "result": {}}))
            } else {
                Resp::status(404)
            }
        }
        _ => Resp::status(404),
    }
}

#[tokio::test]
async fn deploy_is_idempotent_and_never_repeats_the_migration() {
    let st = Arc::new(Mutex::new(Cf::default()));
    let s2 = st.clone();
    let url = serve(Arc::new(move |r| cloudflare(&s2, r))).await;
    let api = format!("{url}/client/v4");
    let d = Deploy {
        api: &api,
        token: TOKEN,
        account: "acct",
        name: "ferrule-relay",
        relay_key: RELAY_KEY,
    };
    let first = relay::deploy(&d).await.unwrap();
    let second = relay::deploy(&d).await.unwrap();
    assert_eq!(first, "https://ferrule-relay.owner.workers.dev");
    assert_eq!(second, first);

    let st = st.lock().unwrap();
    assert_eq!(st.scripts.len(), 1, "one script, nothing else");
    assert_eq!(st.uploads.len(), 2);
    assert_eq!(st.uploads[0]["migrations"]["new_tag"], "v1");
    assert!(st.uploads[1].get("migrations").is_none());
    for meta in &st.uploads {
        assert_eq!(meta["observability"]["enabled"], false);
        assert_eq!(meta["logpush"], false);
        let bindings = meta["bindings"].as_array().unwrap();
        assert!(bindings
            .iter()
            .any(|b| b["name"] == "SLOTS" && b["class_name"] == "Slot"));
        assert!(bindings
            .iter()
            .any(|b| b["name"] == "RELAY_KEY" && b["type"] == "secret_text"));
    }
    assert_eq!(st.subdomain_on, ["ferrule-relay", "ferrule-relay"]);
}

#[tokio::test]
async fn a_refused_deploy_says_why_without_the_credentials() {
    let st = Arc::new(Mutex::new(Cf::default()));
    let s2 = st.clone();
    let url = serve(Arc::new(move |r| cloudflare(&s2, r))).await;
    let api = format!("{url}/client/v4");
    let err = relay::deploy(&Deploy {
        api: &api,
        token: "wrong-token-SECRET",
        account: "acct",
        name: "ferrule-relay",
        relay_key: RELAY_KEY,
    })
    .await
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("HTTP 403") && text.contains("Authentication error"),
        "{text}"
    );
    assert!(
        !text.contains("SECRET") && !text.contains(RELAY_KEY),
        "{text}"
    );
    assert!(st.lock().unwrap().scripts.is_empty());
}
