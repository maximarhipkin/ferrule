//! Integration tests for the native Gemini / Antigravity plan (`GeminiPlan` +
//! `CloudCodeProvider`) against a local mock Google OAuth + Cloud Code
//! `v1internal` server on `127.0.0.1`.

use ferrule_core::message::{Message, Role};
use ferrule_core::provider::{CompletionRequest, Delta, DeltaSink, Provider};
use ferrule_core::tool::ToolDefinition;
use ferrule_plans::gemini::auth::{self, Issuer, Pkce};
use ferrule_plans::gemini::{GeminiPlan, PLAN};
use ferrule_plans::UsageFile;
use ferrule_providers::{CloudCodeProvider, DriverOptions};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The OAuth client the tests sign in with (the real one is the owner's,
/// from the secrets file).
const TEST_CLIENT_ID: &str = "test-client.apps.example";
const TEST_CLIENT_SECRET: &str = "test-client-secret";

#[derive(Default)]
struct MockGoogle {
    fail_first_generate_401: bool,
    refreshes: usize,
    generate_requests: Vec<Value>,
    /// `loadCodeAssist` gives no project: `Some(true)` offers a tier whose
    /// `onboardUser` runs as an operation, `Some(false)` offers none.
    no_project: Option<bool>,
    load_calls: usize,
    onboard_calls: usize,
    operation_calls: usize,
    /// The token endpoint refuses refreshes (`invalid_grant`).
    refuse_refresh: bool,
}

async fn read_http(sock: &mut TcpStream) -> (String, String, String) {
    let mut buf = vec![0u8; 65536];
    let mut total = 0;
    loop {
        let n = sock.read(&mut buf[total..]).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        total += n;
        if let Some(pos) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let body_start = pos + 4;
            while total < body_start + content_len {
                let m = sock.read(&mut buf[total..]).await.unwrap_or(0);
                if m == 0 {
                    break;
                }
                total += m;
            }
            let first = head.lines().next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let body =
                String::from_utf8_lossy(&buf[body_start..(body_start + content_len).min(total)])
                    .to_string();
            return (method, path, body);
        }
    }
    (String::new(), String::new(), String::new())
}

fn http_reply(status: &str, ctype: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn fake_jwt(email: &str, exp: u64) -> String {
    use ferrule_connections::seal::b64;
    format!(
        "{}.{}.sig",
        b64(b"{\"alg\":\"none\"}"),
        b64(json!({ "email": email, "exp": exp }).to_string().as_bytes())
    )
}

async fn serve_mock_google(state: Arc<Mutex<MockGoogle>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let (_method, path, body) = read_http(&mut sock).await;
                let resp = match path.as_str() {
                    "/token" => {
                        assert!(
                            body.contains(&format!("client_id={TEST_CLIENT_ID}"))
                                && body.contains(&format!("client_secret={TEST_CLIENT_SECRET}")),
                            "the token request carries the issuer's client"
                        );
                        let is_refresh = body.contains("grant_type=refresh_token");
                        let refuse = is_refresh && {
                            let mut st = state.lock().unwrap();
                            st.refreshes += 1;
                            st.refuse_refresh
                        };
                        if refuse {
                            let refused = http_reply(
                                "400 Bad Request",
                                "application/json",
                                r#"{"error":"invalid_grant"}"#,
                            );
                            let _ = sock.write_all(refused.as_bytes()).await;
                            return;
                        }
                        let id_tok = fake_jwt("max@example.com", 4_102_444_800);
                        http_reply(
                            "200 OK",
                            "application/json",
                            &json!({
                                "access_token": if is_refresh { "ya29.fresh-secret-tok" } else { "ya29.initial-secret-tok" },
                                "refresh_token": "1//refresh-secret-tok",
                                "id_token": id_tok,
                                "expires_in": 3600,
                                "token_type": "Bearer"
                            })
                            .to_string(),
                        )
                    }
                    "/revoke" => http_reply("200 OK", "application/json", "{}"),
                    "/v1internal:loadCodeAssist" => {
                        let mut st = state.lock().unwrap();
                        st.load_calls += 1;
                        let doc = match st.no_project {
                            None => json!({
                                "cloudaicompanionProject": "companion-proj-123",
                                "currentTier": { "id": "free-tier", "name": "Antigravity" }
                            }),
                            Some(true) => json!({
                                "allowedTiers": [
                                    { "id": "standard-tier", "name": "Standard" },
                                    { "id": "free-tier", "name": "Antigravity", "isDefault": true }
                                ]
                            }),
                            Some(false) => json!({
                                "ineligibleTiers": [{ "reasonCode": "INELIGIBLE_ACCOUNT",
                                                      "reasonMessage": "not eligible" }]
                            }),
                        };
                        http_reply("200 OK", "application/json", &doc.to_string())
                    }
                    "/v1internal:onboardUser" => {
                        let mut st = state.lock().unwrap();
                        st.onboard_calls += 1;
                        assert!(body.contains("\"tierId\":\"free-tier\""), "{body}");
                        http_reply(
                            "200 OK",
                            "application/json",
                            r#"{"name":"operations/onb-1","done":false}"#,
                        )
                    }
                    "/v1internal/operations/onb-1" => {
                        let mut st = state.lock().unwrap();
                        st.operation_calls += 1;
                        let doc = if st.operation_calls < 2 {
                            json!({ "name": "operations/onb-1", "done": false })
                        } else {
                            json!({ "name": "operations/onb-1", "done": true, "response": {
                                "@type": "type.googleapis.com/google.internal.cloud.code.v1internal.OnboardUserResponse",
                                "cloudaicompanionProject": { "id": "onboarded-proj-9", "name": "p" }
                            }})
                        };
                        http_reply("200 OK", "application/json", &doc.to_string())
                    }
                    "/v1internal:fetchAvailableModels" => http_reply(
                        "200 OK",
                        "application/json",
                        &json!({
                            "models": {
                                "gemini-3.8-flash-tiered": { "displayName": "Gemini 3.8 Flash" },
                                "claude-sonnet-5-5-medium": { "displayName": "Claude Sonnet 5.5 (Medium)" },
                                "tab_flash_lite_preview": { "displayName": null }
                            },
                            "tieredModelIds": {
                                "pro": ["gemini-3.8-flash-tiered"],
                                "flash": ["gemini-3.8-flash-tiered"],
                                "flashLite": ["gemini-3.5-flash-lite"]
                            },
                            "agentModelSorts": [{
                                "displayName": "Recommended",
                                "groups": [{ "modelIds": ["claude-sonnet-5-5-medium"] }]
                            }]
                        })
                        .to_string(),
                    ),
                    "/v1internal:retrieveUserQuotaSummary" => http_reply(
                        "200 OK",
                        "application/json",
                        &json!({
                            "groups": [{
                                "buckets": [
                                    {
                                        "bucketId": "3p-5h",
                                        "window": "5h",
                                        "resetTime": "2026-11-03T22:39:16Z",
                                        "remainingFraction": 0.8
                                    },
                                    {
                                        "bucketId": "3p-weekly",
                                        "window": "weekly",
                                        "resetTime": "2026-11-10T17:39:16Z",
                                        "remainingFraction": 0.6
                                    }
                                ]
                            }]
                        })
                        .to_string(),
                    ),
                    "/v1internal:streamGenerateContent?alt=sse" => {
                        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        let mut st = state.lock().unwrap();
                        if st.fail_first_generate_401 {
                            st.fail_first_generate_401 = false;
                            http_reply(
                                "401 Unauthorized",
                                "application/json",
                                r#"{"error":{"code":401,"message":"Unauthenticated"}}"#,
                            )
                        } else {
                            let turn = st.generate_requests.len();
                            st.generate_requests.push(parsed);
                            drop(st);
                            let sse = if turn == 0 {
                                format!(
                                    "data: {}\n\n",
                                    json!({
                                        "response": {
                                            "candidates": [{
                                                "content": {
                                                    "role": "model",
                                                    "parts": [
                                                        { "thought": true, "text": "Checking weather..." },
                                                        {
                                                            "thoughtSignature": "sig_abc_123",
                                                            "functionCall": {
                                                                "id": "call_1",
                                                                "name": "get_weather",
                                                                "args": { "city": "Tel Aviv" }
                                                            }
                                                        }
                                                    ]
                                                },
                                                "finishReason": "STOP"
                                            }],
                                            "usageMetadata": {
                                                "promptTokenCount": 40,
                                                "candidatesTokenCount": 12,
                                                "thoughtsTokenCount": 8,
                                                "cachedContentTokenCount": 10
                                            }
                                        }
                                    })
                                )
                            } else {
                                format!(
                                    "data: {}\n\n",
                                    json!({
                                        "response": {
                                            "candidates": [{
                                                "content": {
                                                    "role": "model",
                                                    "parts": [{ "text": "Sunny and 26C in Tel Aviv." }]
                                                },
                                                "finishReason": "STOP"
                                            }],
                                            "usageMetadata": {
                                                "promptTokenCount": 65,
                                                "candidatesTokenCount": 9,
                                                "cachedContentTokenCount": 35
                                            }
                                        }
                                    })
                                )
                            };
                            http_reply("200 OK", "text/event-stream", &sse)
                        }
                    }
                    _ => http_reply("404 Not Found", "text/plain", ""),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

#[tokio::test]
async fn native_gemini_pkce_sign_in_seals_tokens_and_cloudcode_provider_streams_tool_loop() {
    let state = Arc::new(Mutex::new(MockGoogle {
        fail_first_generate_401: true,
        ..Default::default()
    }));
    let base_url = serve_mock_google(state.clone()).await;
    let mut iss = Issuer::new(&base_url);
    iss.ports = vec![0];
    iss.client_id = TEST_CLIENT_ID.into();
    iss.client_secret = TEST_CLIENT_SECRET.into();

    let tmp = tempfile::tempdir().unwrap();
    let private = tmp.path().join("private");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&data).unwrap();

    let plan = Arc::new(GeminiPlan::new(&private, Some(&data), iss.clone()));

    // 1. Test PKCE URL + loopback callback + paste verification
    let pkce = Pkce::new();
    let st = auth::new_state();
    let cb = auth::Callback::bind(&iss).await.unwrap();
    let redirect = cb.redirect.clone();
    let auth_url = auth::authorize_url(&iss, &redirect, &pkce, &st).unwrap();
    assert!(auth_url.contains(&format!("client_id={TEST_CLIENT_ID}")));
    assert!(auth_url.contains("code_challenge_method=S256"));
    assert!(auth_url.contains("access_type=offline"));

    let pasted_code =
        auth::code_from_redirect(&format!("{redirect}?code=pasted-code&state={st}"), &st).unwrap();
    assert_eq!(pasted_code, "pasted-code");

    let redirect_for_browser = redirect.clone();
    let st_for_browser = st.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let _ = client
            .get(format!(
                "{redirect_for_browser}?code=browser-code&state={st_for_browser}"
            ))
            .send()
            .await;
    });
    let code = cb.wait(&st, Duration::from_secs(5)).await.unwrap();
    assert_eq!(code, "browser-code");

    let tokens = auth::exchange(plan.http(), &iss, &code, &pkce.verifier, &redirect)
        .await
        .unwrap();
    let meta = plan.sign_in(tokens).await.unwrap();
    assert_eq!(meta.email.as_deref(), Some("max@example.com"));
    assert_eq!(meta.project_id.as_deref(), Some("companion-proj-123"));
    assert_eq!(meta.plan.as_deref(), Some("Antigravity"));

    // Verify sealed on disk (raw tokens never appear in plaintext)
    let raw_file = std::fs::read_to_string(plan.store().path()).unwrap();
    assert!(!raw_file.contains("ya29.initial-secret-tok"));
    assert!(!raw_file.contains("1//refresh-secret-tok"));

    // 2. Test CloudCodeProvider model listing + quota window recording
    let provider = CloudCodeProvider::new(
        "gemini-cli",
        &base_url,
        "gemini-4-argon-medium",
        DriverOptions::default(),
        plan.clone(),
    );
    let models = provider.list_models().await.unwrap();
    assert!(models.contains(&"gemini-4-argon-medium".to_string()));
    assert!(models.contains(&"claude-sonnet-5-5-medium".to_string()));
    assert!(!models.iter().any(|m| m.starts_with("tab_")));

    let reading = UsageFile::new(&data).get(PLAN).expect("quota recorded");
    assert_eq!(reading.windows.len(), 2);
    assert_eq!(reading.windows[0].name, "5h");
    assert_eq!(reading.windows[0].used_percent, 20.0);
    assert_eq!(reading.windows[1].name, "weekly");
    assert_eq!(reading.windows[1].used_percent, 40.0);

    // 3. Test Turn 1: 401 triggers automatic refresh, then streams tool call + thoughtSignature
    let req1 = CompletionRequest {
        messages: vec![
            Message::system("You are a helpful assistant."),
            Message::user("What is the weather in Tel Aviv?"),
        ],
        tools: vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get weather".into(),
            parameters: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "additionalProperties": false,
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
        }],
        max_output_tokens: Some(1024),
        temperature: None,
        stream: None,
    };
    let resp1 = provider.complete(req1).await.unwrap();
    assert_eq!(state.lock().unwrap().refreshes, 1, "401 refreshed once");
    assert_eq!(resp1.message.tool_calls.len(), 1);
    assert_eq!(resp1.message.tool_calls[0].name, "get_weather");
    assert_eq!(resp1.message.tool_calls[0].id, "call_1");
    assert_eq!(
        resp1.message.reasoning.as_deref(),
        Some("Checking weather...")
    );
    assert_eq!(resp1.usage.input_tokens, 40);
    assert_eq!(resp1.usage.output_tokens, 20);
    assert_eq!(resp1.usage.cached_input_tokens, 10);

    // 4. Test Turn 2: replays native `thoughtSignature` + sends `functionResponse`
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let deltas = deltas.clone();
        DeltaSink::new(move |d| {
            if let Delta::Text(t) = d {
                deltas.lock().unwrap().push(t);
            }
        })
    };
    let req2 = CompletionRequest {
        messages: vec![
            Message::system("You are a helpful assistant."),
            Message::user("What is the weather in Tel Aviv?"),
            resp1.message,
            Message {
                role: Role::Tool,
                content: Some("Sunny, 26C".into()),
                tool_calls: vec![],
                tool_call_id: Some("call_1".into()),
                reasoning: None,
                native: None,
                images: vec![],
            },
        ],
        tools: vec![],
        max_output_tokens: Some(1024),
        temperature: None,
        stream: Some(sink),
    };
    let resp2 = provider.complete(req2).await.unwrap();
    assert_eq!(
        resp2.message.content.as_deref(),
        Some("Sunny and 26C in Tel Aviv.")
    );
    assert_eq!(
        deltas.lock().unwrap().concat(),
        "Sunny and 26C in Tel Aviv."
    );

    // Verify Turn 2 request replayed `thoughtSignature` and mapped `gemini-4-argon-medium`
    // to `gemini-3.8-flash-tiered` with `thinkingLevel = "MEDIUM"`
    let sent = state.lock().unwrap().generate_requests[1].clone();
    assert_eq!(sent["model"], "gemini-3.8-flash-tiered");
    assert_eq!(sent["project"], "companion-proj-123");
    assert_eq!(
        sent["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "MEDIUM"
    );
    let model_turn = &sent["request"]["contents"][1];
    assert_eq!(
        model_turn["parts"][1]["thoughtSignature"], "sig_abc_123",
        "thoughtSignature preserved across tool loop"
    );
    let tool_turn = &sent["request"]["contents"][2];
    assert_eq!(tool_turn["parts"][0]["functionResponse"]["id"], "call_1");

    // 5. Logout revokes and deletes the sealed file
    let out = plan.log_out().await.unwrap();
    assert!(out.was_signed_in);
    assert!(out.revoked);
    assert!(!plan.store().path().exists());
}

fn plan_for(base_url: &str, tmp: &std::path::Path) -> (Arc<GeminiPlan>, Issuer) {
    let mut iss = Issuer::new(base_url);
    iss.ports = vec![0];
    iss.client_id = TEST_CLIENT_ID.into();
    iss.client_secret = TEST_CLIENT_SECRET.into();
    iss.onboard_poll = Duration::from_millis(10);
    let private = tmp.join("private");
    let data = tmp.join("data");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    (
        Arc::new(GeminiPlan::new(&private, Some(&data), iss.clone())),
        iss,
    )
}

fn tokens() -> auth::Tokens {
    serde_json::from_value(json!({
        "access_token": "ya29.initial-secret-tok",
        "refresh_token": "1//refresh-secret-tok",
        "id_token": fake_jwt("max@example.com", 4_102_444_800),
        "expires_in": 3600
    }))
    .unwrap()
}

fn hello() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    }
}

#[tokio::test]
async fn a_new_account_is_onboarded_and_its_operation_waited_for() {
    let state = Arc::new(Mutex::new(MockGoogle {
        no_project: Some(true),
        ..Default::default()
    }));
    let base_url = serve_mock_google(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (plan, _) = plan_for(&base_url, tmp.path());

    let meta = plan.sign_in(tokens()).await.unwrap();
    assert_eq!(meta.project_id.as_deref(), Some("onboarded-proj-9"));
    assert_eq!(meta.plan.as_deref(), Some("Antigravity"));
    assert!(meta.project_note(0).is_none());
    let st = state.lock().unwrap();
    assert_eq!(st.onboard_calls, 1, "onboardUser once, then its operation");
    assert_eq!(st.operation_calls, 2, "polled until done");
}

#[tokio::test]
async fn an_account_with_no_tier_is_told_so_and_not_looked_up_before_every_request() {
    let state = Arc::new(Mutex::new(MockGoogle {
        no_project: Some(false),
        ..Default::default()
    }));
    let base_url = serve_mock_google(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (plan, _) = plan_for(&base_url, tmp.path());

    let meta = plan.sign_in(tokens()).await.unwrap();
    assert_eq!(meta.project_id, None);
    let note = meta
        .project_note(meta.signed_in_at)
        .expect("sign-in says so");
    assert!(
        note.contains("no Gemini Code Assist / Antigravity project yet (Google offers the account no tier (INELIGIBLE_ACCOUNT))"),
        "{note}"
    );
    assert_eq!(
        state.lock().unwrap().onboard_calls,
        0,
        "nothing to onboard to"
    );

    let provider = CloudCodeProvider::new(
        "gemini-cli",
        &base_url,
        "gemini-3.8-flash-tiered",
        DriverOptions::default(),
        plan.clone(),
    );
    for _ in 0..3 {
        provider.complete(hello()).await.unwrap();
    }
    let st = state.lock().unwrap();
    assert_eq!(
        st.load_calls, 1,
        "the miss is remembered: no lookup per request"
    );
    assert_eq!(st.generate_requests.len(), 3);
    assert!(st.generate_requests[0].get("project").is_none());
}

#[tokio::test]
async fn the_loopback_wait_ignores_other_flows_and_answers_on_both_loopbacks() {
    let iss = Issuer {
        ports: vec![0],
        ..Issuer::new("http://127.0.0.1:9")
    };
    let cb = auth::Callback::bind(&iss).await.unwrap();
    let redirect = cb.redirect.clone();
    assert!(redirect.starts_with("http://localhost:"), "{redirect}");
    let addrs = cb.addrs();
    assert!(addrs.iter().any(|a| a.ip().is_loopback() && a.is_ipv4()));
    let port = addrs[0].port();
    assert!(addrs.iter().all(|a| a.port() == port));
    let st = auth::new_state();

    let wait = tokio::spawn({
        let st = st.clone();
        async move { cb.wait(&st, Duration::from_secs(10)).await }
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    // An older tab's reply and a bare visit don't end the wait.
    for stray in ["?code=old-code&state=older-flow", "", "?state=nope"] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}/auth/callback{stray}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{stray}");
        assert!(!resp.text().await.unwrap().is_empty());
    }
    assert!(!wait.is_finished());
    // This flow's reply ends it, over ::1 when the machine has it.
    let host = match addrs.iter().find(|a| a.is_ipv6()) {
        Some(_) => format!("[::1]:{port}"),
        None => format!("127.0.0.1:{port}"),
    };
    let resp = client
        .get(format!(
            "http://{host}/auth/callback?code=the-code&state={st}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(wait.await.unwrap().unwrap(), "the-code");
}

#[tokio::test]
async fn a_refused_refresh_reads_as_the_google_sign_in_not_an_api_key() {
    let state = Arc::new(Mutex::new(MockGoogle {
        fail_first_generate_401: true,
        refuse_refresh: true,
        ..Default::default()
    }));
    let base_url = serve_mock_google(state.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (plan, _) = plan_for(&base_url, tmp.path());
    plan.sign_in(tokens()).await.unwrap();
    let provider = CloudCodeProvider::new(
        "gemini-cli",
        &base_url,
        "gemini-3.8-flash-tiered",
        DriverOptions::default(),
        plan.clone(),
    );
    let e = provider.complete(hello()).await.unwrap_err();
    assert_eq!(
        ferrule_core::failure::classify(&e),
        ferrule_core::failure::Kind::GeminiSignin
    );
    let said = ferrule_core::failure::chat_text(&e);
    assert!(said.contains("Google sign-in expired"), "{said}");
    assert!(!said.contains("API key"), "{said}");
    assert!(plan.refresh_refused());
    // Signed out for good: the next request doesn't try again.
    let again = provider.complete(hello()).await.unwrap_err();
    assert_eq!(
        ferrule_core::failure::classify(&again),
        ferrule_core::failure::Kind::GeminiSignin
    );
    assert_eq!(state.lock().unwrap().refreshes, 1);
}

#[tokio::test]
async fn with_no_oauth_client_sign_in_says_so_and_a_refresh_keeps_the_sign_in() {
    // No client on the issuer and none in the secrets file: nothing goes
    // to Google.
    assert!(std::env::var_os(auth::CLIENT_ID_VAR).is_none());
    let iss = Issuer {
        ports: vec![0],
        ..Issuer::new("http://127.0.0.1:9")
    };
    let err = iss.client().unwrap_err().to_string();
    assert_eq!(err, auth::NO_CLIENT);
    assert!(err.contains(auth::CLIENT_ID_VAR) && err.contains(auth::CLIENT_SECRET_VAR));
    let pkce = Pkce::new();
    let url = auth::authorize_url(&iss, "http://localhost:1/auth/callback", &pkce, "s");
    assert_eq!(url.unwrap_err().to_string(), auth::NO_CLIENT);
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let ex = auth::exchange(&http, &iss, "code", &pkce.verifier, "http://localhost:1/cb").await;
    assert_eq!(ex.unwrap_err().to_string(), auth::NO_CLIENT);
    // A missing client isn't a refused refresh: the record isn't signed out.
    match auth::refresh(&http, &iss, "1//refresh").await {
        Err(ferrule_connections::oauth::TokenError::Transient(why)) => {
            assert_eq!(why, auth::NO_CLIENT)
        }
        other => panic!("expected a transient error, got {:?}", other.map(|_| ())),
    }
    // Only one half set is no client either.
    let half = Issuer {
        client_id: TEST_CLIENT_ID.into(),
        ..iss.clone()
    };
    assert!(half.client().is_err());
    let both = Issuer {
        client_secret: TEST_CLIENT_SECRET.into(),
        ..half
    };
    assert_eq!(
        both.client().unwrap(),
        (TEST_CLIENT_ID.to_string(), TEST_CLIENT_SECRET.to_string())
    );
}
