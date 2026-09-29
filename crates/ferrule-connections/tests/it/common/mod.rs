//! Mocks for the connections tests: a tiny HTTP server, an OAuth server
//! with DCR and PKCE checks that is also an MCP server wanting a bearer
//! token, a relay speaking the Worker's contract, and a recorder for what
//! the owner is told.
#![allow(dead_code)]

use async_trait::async_trait;
use ferrule_connections::seal::{b64, sha256_b64, unb64};
use ferrule_connections::service::{Action, Button, Chat, Events};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(&self.body)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(status: u16, v: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: v.to_string().into_bytes(),
        }
    }
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: vec![],
        }
    }
    pub fn text(status: u16, t: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: t.as_bytes().to_vec(),
        }
    }
}

pub type Handler = Arc<dyn Fn(Req) -> Resp + Send + Sync>;

/// Serves `h` on 127.0.0.1; the base URL.
pub async fn serve(h: Handler) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let h = h.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let h = h.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body
                                .collect()
                                .await
                                .map(|b| b.to_bytes().to_vec())
                                .unwrap_or_default();
                            let query = parts
                                .uri
                                .query()
                                .map(|q| {
                                    url::form_urlencoded::parse(q.as_bytes())
                                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let headers = parts
                                .headers
                                .iter()
                                .map(|(k, v)| {
                                    (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                                })
                                .collect();
                            let r = h(Req {
                                method: parts.method.to_string(),
                                path: parts.uri.path().to_string(),
                                query,
                                headers,
                                body,
                            });
                            let mut out = hyper::Response::builder().status(r.status);
                            for (k, v) in r.headers {
                                out = out.header(k, v);
                            }
                            Ok::<_, std::convert::Infallible>(
                                out.body(Full::new(Bytes::from(r.body))).unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    base
}

// ---- the OAuth + MCP provider -------------------------------------------

#[derive(Default)]
pub struct PState {
    pub base: String,
    /// client id → redirect URIs.
    pub clients: HashMap<String, Vec<String>>,
    /// code → (client, redirect, challenge, scope).
    pub codes: HashMap<String, (String, String, String, String)>,
    pub access: HashSet<String>,
    pub refresh: HashSet<String>,
    pub n: u32,
    pub expires_in: Option<u64>,
    pub refuse_refresh: bool,
    pub refreshes: u32,
    pub revoked: Vec<String>,
    /// Every Authorization header /mcp got.
    pub seen_auth: Vec<String>,
    pub authorize_queries: Vec<HashMap<String, String>>,
    pub tool_calls: u32,
}

#[derive(Clone)]
pub struct Provider {
    pub base: String,
    pub st: Arc<Mutex<PState>>,
}

fn s256(v: &str) -> String {
    b64(ring::digest::digest(&ring::digest::SHA256, v.as_bytes()).as_ref())
}

impl Provider {
    pub async fn start() -> Self {
        let st = Arc::new(Mutex::new(PState {
            expires_in: Some(3600),
            ..Default::default()
        }));
        let s2 = st.clone();
        let base = serve(Arc::new(move |r| provider(&s2, r))).await;
        st.lock().unwrap().base = base.clone();
        Self { base, st }
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base)
    }

    /// Every access token stops working (the next call gets a 401).
    pub fn expire_access(&self) {
        self.st.lock().unwrap().access.clear();
    }

    pub fn tokens_issued(&self) -> Vec<String> {
        let st = self.st.lock().unwrap();
        st.access.iter().chain(st.refresh.iter()).cloned().collect()
    }

    /// The owner's browser: open the authorize link, approve, and return
    /// where the provider redirects (not followed).
    pub async fn approve(&self, authorize_url: &str) -> String {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = http.get(authorize_url).send().await.unwrap();
        assert_eq!(resp.status(), 302, "the provider approved");
        resp.headers()["location"].to_str().unwrap().to_string()
    }
}

fn provider(st: &Mutex<PState>, r: Req) -> Resp {
    let mut st = st.lock().unwrap();
    let base = st.base.clone();
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => Resp::json(
            200,
            json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}),
        ),
        ("GET", "/.well-known/oauth-authorization-server") => Resp::json(
            200,
            json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "revocation_endpoint": format!("{base}/revoke"),
                "code_challenge_methods_supported": ["S256"],
            }),
        ),
        ("POST", "/register") => {
            let v = r.json();
            let uris: Vec<String> = v["redirect_uris"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|u| u.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if uris.is_empty() || v["token_endpoint_auth_method"] != "none" {
                return Resp::json(400, json!({"error": "invalid_client_metadata"}));
            }
            st.n += 1;
            let id = format!("client-{}", st.n);
            st.clients.insert(id.clone(), uris);
            Resp::json(201, json!({"client_id": id}))
        }
        ("GET", "/authorize") => {
            let q = r.query.clone();
            st.authorize_queries.push(q.clone());
            let ok = q.get("response_type").map(String::as_str) == Some("code")
                && q.get("code_challenge_method").map(String::as_str) == Some("S256")
                && q.contains_key("state")
                && q.contains_key("code_challenge")
                && q.get("client_id")
                    .and_then(|c| st.clients.get(c))
                    .zip(q.get("redirect_uri"))
                    .is_some_and(|(uris, u)| uris.contains(u));
            if !ok {
                return Resp::text(400, "bad authorize request");
            }
            st.n += 1;
            let code = format!("code-{}", st.n);
            st.codes.insert(
                code.clone(),
                (
                    q["client_id"].clone(),
                    q["redirect_uri"].clone(),
                    q["code_challenge"].clone(),
                    q.get("scope").cloned().unwrap_or_default(),
                ),
            );
            let mut to = url::Url::parse(&q["redirect_uri"]).unwrap();
            to.query_pairs_mut()
                .append_pair("code", &code)
                .append_pair("state", &q["state"]);
            Resp {
                status: 302,
                headers: vec![("location".into(), to.to_string())],
                body: vec![],
            }
        }
        ("POST", "/token") => {
            let f = r.form();
            let issue = |st: &mut PState, scope: &str| {
                st.n += 1;
                let at = format!("at-SECRET-{}", st.n);
                let rt = format!("rt-SECRET-{}", st.n);
                st.access.insert(at.clone());
                st.refresh.insert(rt.clone());
                let mut v = json!({"access_token": at, "refresh_token": rt, "token_type": "Bearer", "scope": scope});
                if let Some(e) = st.expires_in {
                    v["expires_in"] = e.into();
                }
                Resp::json(200, v)
            };
            match f.get("grant_type").map(String::as_str) {
                Some("authorization_code") => {
                    let Some((client, redirect, challenge, scope)) =
                        f.get("code").and_then(|c| st.codes.remove(c))
                    else {
                        return Resp::json(400, json!({"error": "invalid_grant"}));
                    };
                    let good = f.get("client_id") == Some(&client)
                        && f.get("redirect_uri") == Some(&redirect)
                        && f.get("code_verifier").map(|v| s256(v)) == Some(challenge);
                    if !good {
                        return Resp::json(400, json!({"error": "invalid_grant"}));
                    }
                    issue(&mut st, &scope)
                }
                Some("refresh_token") => {
                    st.refreshes += 1;
                    let rt = f.get("refresh_token").cloned().unwrap_or_default();
                    if st.refuse_refresh || !st.refresh.remove(&rt) {
                        return Resp::json(400, json!({"error": "invalid_grant"}));
                    }
                    issue(&mut st, "read")
                }
                _ => Resp::json(400, json!({"error": "unsupported_grant_type"})),
            }
        }
        ("POST", "/revoke") => {
            let f = r.form();
            if let Some(t) = f.get("token") {
                st.access.remove(t);
                st.refresh.remove(t);
                st.revoked.push(t.clone());
            }
            Resp::status(200)
        }
        ("POST", "/mcp") => {
            let auth = r.headers.get("authorization").cloned().unwrap_or_default();
            st.seen_auth.push(auth.clone());
            let ok = auth
                .strip_prefix("Bearer ")
                .is_some_and(|t| st.access.contains(t));
            if !ok {
                return Resp {
                    status: 401,
                    headers: vec![(
                        "www-authenticate".into(),
                        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""),
                    )],
                    body: b"{\"error\":\"invalid_token\"}".to_vec(),
                };
            }
            let msg = r.json();
            let id = msg["id"].clone();
            match msg["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let mut resp = Resp::json(
                        200,
                        json!({"jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "mock", "version": "1"}
                        }}),
                    );
                    resp.headers
                        .push(("mcp-session-id".into(), "sess-1".into()));
                    resp
                }
                m if m.starts_with("notifications/") => Resp::status(202),
                "tools/list" => Resp::json(
                    200,
                    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                        {"name": "search", "description": "Search the workspace.",
                         "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}},
                         "annotations": {"readOnlyHint": true}},
                        {"name": "create_issue", "description": "Create an issue.",
                         "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}}},
                         "annotations": {"readOnlyHint": false}}
                    ]}}),
                ),
                "tools/call" => {
                    st.tool_calls += 1;
                    Resp::json(
                        200,
                        json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": "found 3 issues"}]
                        }}),
                    )
                }
                _ => Resp::json(
                    200,
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no"}}),
                ),
            }
        }
        _ => Resp::status(404),
    }
}

// ---- the relay ------------------------------------------------------------

#[derive(Default)]
pub struct Slot {
    pub value: Option<String>,
    pub used: bool,
}

#[derive(Default)]
pub struct RState {
    pub slots: HashMap<String, Slot>,
    pub polls: u32,
}

/// The Worker's contract (`relay/worker.js`), without the timers.
#[derive(Clone)]
pub struct MockRelay {
    pub url: String,
    pub key: String,
    pub st: Arc<Mutex<RState>>,
}

impl MockRelay {
    pub async fn start(key: &str) -> Self {
        let st = Arc::new(Mutex::new(RState::default()));
        let (s2, k2) = (st.clone(), key.to_string());
        let url = serve(Arc::new(move |r| relay(&s2, &k2, r))).await;
        Self {
            url,
            key: key.to_string(),
            st,
        }
    }

    /// Follow a redirect that points at the relay, like the browser does.
    pub async fn visit(&self, location: &str) -> u16 {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(location)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

fn put(st: &mut RState, id: &str, value: String) -> u16 {
    match st.slots.get_mut(id) {
        None => 404,
        Some(s) if s.used || s.value.is_some() => 409,
        Some(s) => {
            s.value = Some(value);
            200
        }
    }
}

fn relay(st: &Mutex<RState>, key: &str, r: Req) -> Resp {
    let mut st = st.lock().unwrap();
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/health") => {
            Resp::json(200, json!({"ok": true, "relay": "ferrule-relay", "v": 1}))
        }
        ("POST", "/poll") => {
            st.polls += 1;
            if r.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {key}"))
            {
                return Resp::status(401);
            }
            let Some(raw) = r.json()["secret"].as_str().and_then(|s| unb64(s).ok()) else {
                return Resp::status(400);
            };
            if raw.len() != 32 {
                return Resp::status(400);
            }
            let id = sha256_b64(&raw);
            let slot = st.slots.entry(id).or_default();
            if slot.used {
                return Resp::status(410);
            }
            match slot.value.take() {
                Some(v) => {
                    slot.used = true;
                    Resp {
                        status: 200,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: v.into_bytes(),
                    }
                }
                None => Resp::status(204),
            }
        }
        ("GET", "/cb") => {
            let Some(state) = r.query.get("state") else {
                return Resp::status(400);
            };
            let mut v = json!({"kind": "oauth"});
            for k in ["code", "error", "error_description", "iss"] {
                if let Some(x) = r.query.get(k) {
                    v[k] = x.clone().into();
                }
            }
            if v.get("code").is_none() && v.get("error").is_none() {
                return Resp::status(400);
            }
            Resp::text(put(&mut st, state, v.to_string()), "done")
        }
        ("POST", p) if p.starts_with("/drop/") => {
            let id = p.trim_start_matches("/drop/").to_string();
            if r.body.len() > 8192 {
                return Resp::status(413);
            }
            let v = r.json();
            if v["v"] != 1 || !["epk", "iv", "ct"].iter().all(|f| v[*f].is_string()) {
                return Resp::status(400);
            }
            let value =
                json!({"kind": "key", "v": 1, "epk": v["epk"], "iv": v["iv"], "ct": v["ct"]});
            Resp::text(put(&mut st, &id, value.to_string()), "sent")
        }
        _ => Resp::status(404),
    }
}

/// What the browser's key form does (`encryptKey` in worker.js): ECDH
/// P-256 with the link's public key, HKDF-SHA256 (salt = slot id), and
/// AES-256-GCM with the slot id as associated data.
pub fn encrypt_key(public_b64: &str, slot_id: &str, secret: &str) -> Value {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
    use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
    let rng = ring::rand::SystemRandom::new();
    let mine = EphemeralPrivateKey::generate(&ECDH_P256, &rng).unwrap();
    let epk = b64(mine.compute_public_key().unwrap().as_ref());
    let peer = UnparsedPublicKey::new(&ECDH_P256, unb64(public_b64).unwrap());
    let key = agreement::agree_ephemeral(mine, &peer, |shared| {
        let prk =
            ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, slot_id.as_bytes()).extract(shared);
        UnboundKey::from(prk.expand(&[b"ferrule key form v1"], &AES_256_GCM).unwrap())
    })
    .unwrap();
    let iv: [u8; 12] = ferrule_connections::seal::random::<12>();
    let mut ct = secret.as_bytes().to_vec();
    LessSafeKey::new(key)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(iv),
            Aad::from(slot_id.as_bytes()),
            &mut ct,
        )
        .unwrap();
    json!({"v": 1, "epk": epk, "iv": b64(&iv), "ct": b64(&ct)})
}

// ---- the owner's side -----------------------------------------------------

#[derive(Default)]
pub struct Recorder {
    pub told: Mutex<Vec<(String, Vec<Button>)>>,
    pub audits: Mutex<Vec<(String, Value)>>,
    pub connected: Mutex<Vec<(String, Option<Chat>)>>,
}

#[async_trait]
impl Events for Recorder {
    async fn tell_owner(&self, text: &str, buttons: Vec<Button>) {
        self.told.lock().unwrap().push((text.to_string(), buttons));
    }
    fn audit(&self, event: &str, detail: Value) {
        self.audits
            .lock()
            .unwrap()
            .push((event.to_string(), detail));
    }
    async fn connected(&self, name: &str, chat: Option<&Chat>) {
        self.connected
            .lock()
            .unwrap()
            .push((name.to_string(), chat.cloned()));
    }
}

impl Recorder {
    /// Waits (up to 10 s) for a message to the owner containing `needle`.
    pub async fn wait_told(&self, needle: &str) -> (String, Vec<Button>) {
        for _ in 0..200 {
            if let Some(m) = self
                .told
                .lock()
                .unwrap()
                .iter()
                .find(|(t, _)| t.contains(needle))
                .cloned()
            {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "the owner was never told {needle:?}; told: {:#?}",
            self.told.lock().unwrap()
        );
    }

    pub fn events(&self) -> Vec<String> {
        self.audits
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.clone())
            .collect()
    }

    /// Everything the owner saw and the audit log got, as one string.
    pub fn everything(&self) -> String {
        format!(
            "{:?}{:?}{:?}",
            self.told.lock().unwrap(),
            self.audits.lock().unwrap(),
            self.connected.lock().unwrap()
        )
    }
}

pub fn url_button(buttons: &[Button]) -> String {
    buttons
        .iter()
        .find_map(|b| match &b.action {
            Action::Url(u) => Some(u.clone()),
            _ => None,
        })
        .expect("a link button")
}

pub fn command_buttons(buttons: &[Button]) -> Vec<String> {
    buttons
        .iter()
        .filter_map(|b| match &b.action {
            Action::Command(c) => Some(c.clone()),
            _ => None,
        })
        .collect()
}
