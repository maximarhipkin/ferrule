//! A mock OpenAI issuer and Codex backend on 127.0.0.1, for tests here and
//! in the CLI (feature `mock`): device code, the code exchange, refresh
//! with rotation, revoke, and `/backend-api/codex/responses` with its
//! rate-limit headers, a 401 for a refused token and a 429 usage limit.
//! Its tokens are unsigned fakes.

use crate::chatgpt::auth::{self, Issuer};
use crate::chatgpt::ChatGpt;
use ferrule_connections::seal::b64;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn jwt(claims: Value) -> String {
    format!(
        "{}.{}.sig",
        b64(b"{\"alg\":\"none\"}"),
        b64(claims.to_string().as_bytes())
    )
}

pub fn access(n: u32, exp: u64) -> String {
    jwt(json!({"exp": exp, "n": n}))
}

pub fn id_token() -> String {
    jwt(
        json!({"email": "owner@example.com", "https://api.openai.com/auth": {
        "chatgpt_account_id": "acct_42", "chatgpt_plan_type": "plus"}}),
    )
}

/// The mock issuer's (and backend's) state.
#[derive(Default)]
pub struct Issuing {
    /// The refresh token that works now; older ones are "reused".
    pub current_refresh: String,
    pub generation: u32,
    pub refreshes: u32,
    pub revoked: Vec<String>,
    /// Refuse every refresh with `invalid_grant`.
    pub revoke_all: bool,
    /// Pending polls before the device code is typed.
    pub device_pending: u32,
    /// Hold each refresh this long (to widen a race).
    pub refresh_delay_ms: u64,
    /// The backend refuses this access token with a 401.
    pub refused_access: Option<String>,
    pub requests: Vec<(String, String, String)>,
    pub exchanges: Vec<HashMap<String, String>>,
    /// The backend answers 429 `usage_limit_reached`, resetting then.
    pub limited_until: Option<u64>,
    /// No device-code sign-in: the user-code endpoint answers 404.
    pub no_device: bool,
    /// What the backend's turns answer.
    pub answer: Option<String>,
}

pub type Shared = Arc<Mutex<Issuing>>;

pub async fn read_request(
    sock: &mut tokio::net::TcpStream,
) -> Option<(String, String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    let mut first = head.lines().next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    Some((method, target, head, body))
}

pub fn reply(status: &str, ctype: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub fn json_reply(status: &str, v: Value) -> String {
    reply(status, "application/json", &v.to_string())
}

pub fn form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub fn completed(text: &str) -> String {
    let response = json!({"id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5.5",
        "output": [{"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}});
    format!(
        "event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": response})
    )
}

pub async fn handle(state: Shared, method: &str, target: &str, head: &str, body: &str) -> String {
    let path = target.split('?').next().unwrap_or(target);
    state
        .lock()
        .unwrap()
        .requests
        .push((method.into(), target.into(), body.into()));
    match path {
        "/api/accounts/deviceauth/usercode" if state.lock().unwrap().no_device => {
            reply("404 Not Found", "text/plain", "no")
        }
        "/api/accounts/deviceauth/usercode" => {
            let v: Value = serde_json::from_str(body).unwrap_or_default();
            assert_eq!(v["client_id"], auth::CLIENT_ID);
            json_reply(
                "200 OK",
                json!({"device_auth_id": "dev_secret", "user_code": "ABCD-1234", "interval": "1"}),
            )
        }
        "/api/accounts/deviceauth/token" => {
            let v: Value = serde_json::from_str(body).unwrap_or_default();
            assert_eq!(v["device_auth_id"], "dev_secret");
            assert_eq!(v["user_code"], "ABCD-1234");
            let mut s = state.lock().unwrap();
            if s.device_pending > 0 {
                s.device_pending -= 1;
                return json_reply("403 Forbidden", json!({"error": "authorization_pending"}));
            }
            json_reply(
                "200 OK",
                json!({"authorization_code": "device-code", "code_challenge": "c", "code_verifier": "device-verifier"}),
            )
        }
        "/oauth/token" if body.starts_with('{') => {
            let v: Value = serde_json::from_str(body).unwrap();
            assert_eq!(v["grant_type"], "refresh_token");
            assert_eq!(v["client_id"], auth::CLIENT_ID);
            let delay = state.lock().unwrap().refresh_delay_ms;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let mut s = state.lock().unwrap();
            s.refreshes += 1;
            if s.revoke_all {
                return json_reply("400 Bad Request", json!({"error": "invalid_grant"}));
            }
            if v["refresh_token"] != s.current_refresh.as_str() {
                return json_reply(
                    "401 Unauthorized",
                    json!({"error": {"code": "refresh_token_reused", "message": "Your refresh token was already used"}}),
                );
            }
            s.generation += 1;
            s.current_refresh = format!("refresh-{}", s.generation);
            json_reply(
                "200 OK",
                json!({"access_token": access(s.generation, now() + 3600),
                       "refresh_token": s.current_refresh, "id_token": id_token()}),
            )
        }
        "/oauth/token" => {
            let f = form(body);
            assert_eq!(f["grant_type"], "authorization_code");
            assert_eq!(f["client_id"], auth::CLIENT_ID);
            let mut s = state.lock().unwrap();
            s.exchanges.push(f);
            s.generation += 1;
            s.current_refresh = format!("refresh-{}", s.generation);
            json_reply(
                "200 OK",
                json!({"access_token": access(s.generation, now() + 3600),
                       "refresh_token": s.current_refresh, "id_token": id_token()}),
            )
        }
        "/oauth/revoke" => {
            let v: Value = serde_json::from_str(body).unwrap();
            assert_eq!(v["token_type_hint"], "refresh_token");
            state
                .lock()
                .unwrap()
                .revoked
                .push(v["token"].as_str().unwrap().into());
            json_reply("200 OK", json!({}))
        }
        "/backend-api/codex/responses" => {
            let bearer = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("authorization: Bearer ")
                        .or(l.strip_prefix("Authorization: Bearer "))
                })
                .unwrap_or("")
                .trim()
                .to_string();
            let (refused, limited, answer) = {
                let s = state.lock().unwrap();
                (s.refused_access.clone(), s.limited_until, s.answer.clone())
            };
            if let Some(until) = limited {
                let body = json!({"error": {"type": "usage_limit_reached",
                    "message": "You've hit your usage limit.", "plan_type": "plus",
                    "resets_at": until, "limit_window_minutes": 300}})
                .to_string();
                return format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n\
                     x-codex-primary-used-percent: 100\r\nx-codex-primary-window-minutes: 300\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            if refused.as_deref() == Some(bearer.as_str()) {
                return json_reply(
                    "401 Unauthorized",
                    json!({"error": {"message": "token expired"}}),
                );
            }
            let body = completed(answer.as_deref().unwrap_or("hello from the plan"));
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 x-codex-primary-used-percent: 5\r\nx-codex-primary-window-minutes: 300\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        _ => reply("404 Not Found", "text/plain", "no"),
    }
}

pub async fn serve(state: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                if let Some((m, t, h, b)) = read_request(&mut sock).await {
                    let out = handle(state, &m, &t, &h, &b).await;
                    let _ = sock.write_all(out.as_bytes()).await;
                }
            });
        }
    });
    url
}

pub fn issuer(url: &str) -> Issuer {
    let mut i = Issuer::new(url);
    i.ports = vec![0];
    i
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// A store holding `refresh` and an access token that expires at `exp`.
pub async fn signed_in(state: &Shared, private: &std::path::Path, url: &str, exp: u64) -> ChatGpt {
    let plan = ChatGpt::new(private, None, issuer(url));
    state.lock().unwrap().current_refresh = "refresh-0".into();
    plan.sign_in(auth::Tokens {
        id_token: Some(id_token()),
        access_token: Some(access(0, exp)),
        refresh_token: Some("refresh-0".into()),
    })
    .await
    .unwrap();
    plan
}
