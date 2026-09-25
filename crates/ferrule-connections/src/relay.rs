//! ferrule's side of the relay (`relay/worker.js`): health, a slot per
//! flow, polling it, and deploying the Worker to the owner's Cloudflare
//! account.

use crate::seal::{b64, random, sha256_b64};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub const WORKER_JS: &str = include_str!("../../../relay/worker.js");
pub const RELAY_KEY_ENV: &str = "FERRULE_RELAY_KEY";
/// The Durable Object migration the Worker needs, sent once.
const MIGRATION_TAG: &str = "v1";

/// A flow's slot: the secret only ferrule holds, and its public id (the
/// OAuth `state`).
pub struct Slot {
    pub secret: String,
    pub id: String,
}

impl Slot {
    pub fn new() -> Self {
        let raw = random::<32>();
        Self {
            secret: b64(&raw),
            id: sha256_b64(&raw),
        }
    }
}

impl Default for Slot {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq)]
pub enum Poll {
    /// What the redirect or the key form wrote; now deleted at the relay.
    Value(Value),
    Empty,
    /// Already read (a second poller, or a replay).
    Used,
}

#[derive(Clone)]
pub struct Relay {
    pub url: String,
    key: String,
    http: reqwest::Client,
}

impl Relay {
    pub fn new(url: &str, key: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("an HTTP client"),
        }
    }

    pub fn callback_url(&self) -> String {
        format!("{}/cb", self.url)
    }

    /// The key form's link: everything after `#` stays in the browser.
    pub fn key_form_url(&self, slot: &Slot, public_key: &str, title: &str) -> String {
        let title: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        format!("{}/key#s={}&k={public_key}&t={title}", self.url, slot.id)
    }

    /// Answers `/health` as a ferrule relay, within 5 s.
    pub async fn healthy(&self) -> bool {
        let got = self
            .http
            .get(format!("{}/health", self.url))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match got {
            Ok(r) if r.status().is_success() => r
                .json::<Value>()
                .await
                .is_ok_and(|v| v["relay"] == "ferrule-relay" && v["ok"] == true),
            _ => false,
        }
    }

    /// Opens the slot if it's new, and takes its value if there is one.
    pub async fn poll(&self, slot: &Slot) -> Result<Poll> {
        let resp = self
            .http
            .post(format!("{}/poll", self.url))
            .bearer_auth(&self.key)
            .json(&json!({ "secret": slot.secret }))
            .send()
            .await
            .map_err(|_| anyhow!("the relay didn't answer"))?;
        match resp.status().as_u16() {
            200 => Ok(Poll::Value(
                resp.json().await.context("the relay's answer")?,
            )),
            204 => Ok(Poll::Empty),
            410 => Ok(Poll::Used),
            401 => bail!("the relay refused ferrule's relay key ({RELAY_KEY_ENV})"),
            s => bail!("the relay answered HTTP {s}"),
        }
    }
}

/// `relay check`: open a slot, write a test code through `/cb`, read it
/// back, and read again (used). Each step, in order.
pub async fn check(relay: &Relay) -> Vec<(String, bool)> {
    let mut steps = Vec::new();
    let healthy = relay.healthy().await;
    steps.push(("/health answers as a ferrule relay".to_string(), healthy));
    if !healthy {
        return steps;
    }
    let slot = Slot::new();
    let opened = matches!(relay.poll(&slot).await, Ok(Poll::Empty));
    steps.push(("the relay key opens a slot".to_string(), opened));
    if !opened {
        return steps;
    }
    let code = format!("check-{}", b64(&random::<9>()));
    let wrote = relay
        .http
        .get(format!("{}/cb?state={}&code={code}", relay.url, slot.id))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    steps.push(("the callback writes into it".to_string(), wrote));
    let read = matches!(relay.poll(&slot).await, Ok(Poll::Value(v)) if v["code"] == code.as_str());
    steps.push(("the value is read back once".to_string(), read));
    let gone = matches!(relay.poll(&slot).await, Ok(Poll::Used));
    steps.push(("a second read finds it gone".to_string(), gone));
    steps
}

/// Where and as whom to deploy. `api` is Cloudflare's API base (a mock in
/// tests).
pub struct Deploy<'a> {
    pub api: &'a str,
    pub token: &'a str,
    pub account: &'a str,
    pub name: &'a str,
    /// The relay key to set as the Worker's `RELAY_KEY` secret.
    pub relay_key: &'a str,
}

/// Upload the Worker (idempotent), turn on its `workers.dev` route, and
/// return its URL. Credentials never appear in an error.
pub async fn deploy(d: &Deploy<'_>) -> Result<String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let api = d.api.trim_end_matches('/');
    let base = format!("{api}/accounts/{}/workers", d.account);
    let call = |r: reqwest::RequestBuilder| async move {
        let resp = r
            .bearer_auth(d.token)
            .send()
            .await
            .map_err(|_| anyhow!("Cloudflare's API didn't answer"))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() || body["success"] == false {
            let why = body["errors"][0]["message"]
                .as_str()
                .map(|m| m.chars().take(200).collect::<String>())
                .or_else(|| body["error"].as_str().map(String::from))
                .unwrap_or_default();
            bail!(
                "Cloudflare refused (HTTP {}){}{}",
                status.as_u16(),
                if why.is_empty() { "" } else { ": " },
                why
            );
        }
        Ok::<Value, anyhow::Error>(body)
    };

    // Is the migration there already? Cloudflare refuses a repeated tag.
    let list = call(http.get(format!("{base}/scripts"))).await?;
    let applied = list["result"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["id"] == d.name && s["migration_tag"] == MIGRATION_TAG);
    let mut metadata = json!({
        "main_module": "worker.js",
        "compatibility_date": "2025-09-01",
        "bindings": [
            {"type": "durable_object_namespace", "name": "SLOTS", "class_name": "Slot"},
            {"type": "secret_text", "name": "RELAY_KEY", "text": d.relay_key},
        ],
        "observability": {"enabled": false},
        "logpush": false,
    });
    if !applied {
        metadata["migrations"] = json!({"new_tag": MIGRATION_TAG, "new_sqlite_classes": ["Slot"]});
    }
    let boundary = format!("ferrule-{}", b64(&random::<12>()));
    let body = multipart(
        &boundary,
        &[
            (
                "metadata",
                "metadata.json",
                "application/json",
                &metadata.to_string(),
            ),
            (
                "worker.js",
                "worker.js",
                "application/javascript+module",
                WORKER_JS,
            ),
        ],
    );
    call(
        http.put(format!("{base}/scripts/{}", d.name))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body),
    )
    .await?;
    call(
        http.post(format!("{base}/scripts/{}/subdomain", d.name))
            .json(&json!({"enabled": true, "previews_enabled": false})),
    )
    .await?;
    let sub = call(http.get(format!("{base}/subdomain"))).await?;
    let sub = sub["result"]["subdomain"].as_str().context(
        "the account has no workers.dev subdomain yet: open Workers once in the dashboard",
    )?;
    Ok(format!("https://{}.{sub}.workers.dev", d.name))
}

/// A `multipart/form-data` body: (field, file name, type, content).
fn multipart(boundary: &str, parts: &[(&str, &str, &str, &str)]) -> String {
    let mut body = String::new();
    for (name, file, kind, content) in parts {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\nContent-Type: {kind}\r\n\r\n{content}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_id_is_the_hash_of_its_secret() {
        let s = Slot::new();
        assert_eq!(s.id.len(), 43);
        assert_eq!(s.secret.len(), 43);
        assert_eq!(s.id, sha256_b64(&crate::seal::unb64(&s.secret).unwrap()));
    }

    #[test]
    fn the_key_form_link_keeps_everything_in_the_fragment() {
        let r = Relay::new("https://r.example/", "k");
        let slot = Slot::new();
        let url = r.key_form_url(&slot, "PUB", "GitHub CLI");
        assert!(url.starts_with("https://r.example/key#s="));
        assert!(url.ends_with("&k=PUB&t=GitHub+CLI"));
        assert!(!url.contains(&slot.secret));
    }
}
