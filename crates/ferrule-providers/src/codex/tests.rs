//! The Codex backend's shapes (the pinned commit's), served from 127.0.0.1.

use super::*;
use crate::common::mock::{body, event, serve, sse, status};
use ferrule_core::message::Message;
use std::sync::Mutex;

/// A token source that counts what the driver asked of it.
#[derive(Default)]
struct Fake {
    token: Mutex<String>,
    refreshes: Mutex<u32>,
    refuse_refresh: bool,
    seen: Mutex<Vec<RateLimits>>,
}

#[async_trait::async_trait]
impl PlanAuth for Fake {
    async fn credentials(&self) -> Result<PlanCredentials, CoreError> {
        Ok(PlanCredentials {
            access_token: self.token.lock().unwrap().clone(),
            account_id: Some("acct_123".into()),
            fedramp: false,
        })
    }
    async fn refused(&self, used: &str) -> Result<(), CoreError> {
        assert_eq!(used, *self.token.lock().unwrap());
        *self.refreshes.lock().unwrap() += 1;
        if self.refuse_refresh {
            return Err(CoreError::Provider("sign in again".into()));
        }
        *self.token.lock().unwrap() = "access-2".into();
        Ok(())
    }
    fn observe(&self, limits: &RateLimits) {
        self.seen.lock().unwrap().push(limits.clone());
    }
}

fn fake() -> Arc<Fake> {
    let f = Fake::default();
    *f.token.lock().unwrap() = "access-1".into();
    Arc::new(f)
}

fn provider(url: &str, auth: Arc<Fake>) -> CodexProvider {
    let base = url.trim_end_matches("/v1").to_string() + "/backend-api/codex";
    CodexProvider::new("chatgpt", base, "gpt-5.5", DriverOptions::default(), auth)
}

fn req() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::system("You are Ferrule."), Message::user("hi")],
        tools: vec![],
        max_output_tokens: Some(512),
        temperature: Some(0.3),
        stream: None,
    }
}

/// A `response.completed` stream saying `text`.
fn stream(text: &str) -> Vec<String> {
    let response = json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-5.5",
        "output": [{"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": 1200, "input_tokens_details": {"cached_tokens": 1024},
                  "output_tokens": 7, "output_tokens_details": {"reasoning_tokens": 0},
                  "total_tokens": 1207}
    });
    vec![
        event(
            "response.created",
            &json!({"type": "response.created", "response": {"id": "resp_1"}}),
        ),
        event(
            "response.output_text.delta",
            &json!({"type": "response.output_text.delta", "delta": text}),
        ),
        event(
            "response.completed",
            &json!({"type": "response.completed", "response": response}),
        ),
    ]
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

const LIMIT_HEADERS: &str = "content-type: text/event-stream\r\n\
x-codex-primary-used-percent: 12.5\r\nx-codex-primary-window-minutes: 300\r\nx-codex-primary-reset-at: 1790506800\r\n\
x-codex-secondary-used-percent: 40\r\nx-codex-secondary-window-minutes: 10080\r\nx-codex-secondary-reset-at: 1790820000\r\n";

#[tokio::test]
async fn a_turn_goes_to_the_codex_backend_in_its_shape() {
    let wire = stream("pong");
    let mut reply = sse(&refs(&wire));
    reply.headers = LIMIT_HEADERS;
    let (url, seen) = serve(vec![reply]);
    let auth = fake();
    let resp = provider(&url, auth.clone()).complete(req()).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("pong"));
    assert_eq!(resp.usage.input_tokens, 1200);
    assert_eq!(resp.usage.cached_input_tokens, 1024);

    let raw = seen.join().unwrap().remove(0);
    let head = raw.to_ascii_lowercase();
    assert!(
        head.starts_with("post /backend-api/codex/responses "),
        "{raw}"
    );
    assert!(head.contains("authorization: bearer access-1"));
    assert!(head.contains("chatgpt-account-id: acct_123"));
    assert!(head.contains("originator: codex_cli_rs"));
    assert!(head.contains("accept: text/event-stream"));
    assert!(head.contains("session-id: ferrule-"));
    assert!(!head.contains("openai-beta"));
    let sent = body(&raw);
    assert_eq!(sent["store"], false);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(sent["instructions"], "You are Ferrule.");
    assert!(sent.get("max_output_tokens").is_none(), "{sent}");
    assert!(sent.get("temperature").is_none(), "{sent}");
    assert!(sent.get("previous_response_id").is_none(), "{sent}");
    assert_eq!(
        sent["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    assert_eq!(
        sent["prompt_cache_key"],
        head.split("session-id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
    );

    let seen = auth.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].windows,
        vec![
            LimitWindow {
                minutes: Some(300),
                used_percent: 12.5,
                resets_at: Some(1_790_506_800)
            },
            LimitWindow {
                minutes: Some(10080),
                used_percent: 40.0,
                resets_at: Some(1_790_820_000)
            },
        ]
    );
}

#[tokio::test]
async fn the_cache_key_is_stable_across_a_conversation() {
    let p = provider("http://127.0.0.1:1/v1", fake());
    let mut r = req();
    let a = p.payload(&r, false).0["prompt_cache_key"].clone();
    r.messages
        .push(Message::assistant(Some("hello".into()), vec![], None));
    r.messages.push(Message::user("and again"));
    let b = p.payload(&r, false).0["prompt_cache_key"].clone();
    assert_eq!(a, b);
    r.messages[1] = Message::user("another chat");
    assert_ne!(p.payload(&r, false).0["prompt_cache_key"], a);
}

#[tokio::test]
async fn a_401_refreshes_once_and_retries() {
    let wire = stream("after refresh");
    let (url, seen) = serve(vec![
        status(
            "401 Unauthorized",
            "content-type: application/json\r\n",
            r#"{"error":{"message":"expired"}}"#,
        ),
        sse(&refs(&wire)),
    ]);
    let auth = fake();
    let resp = provider(&url, auth.clone()).complete(req()).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("after refresh"));
    assert_eq!(*auth.refreshes.lock().unwrap(), 1);
    let reqs = seen.join().unwrap();
    assert!(reqs[0].to_ascii_lowercase().contains("bearer access-1"));
    assert!(reqs[1].to_ascii_lowercase().contains("bearer access-2"));
}

#[tokio::test]
async fn a_second_401_says_sign_in_again() {
    let r401 = || {
        status(
            "401 Unauthorized",
            "content-type: application/json\r\n",
            "{}",
        )
    };
    let (url, _seen) = serve(vec![r401(), r401()]);
    let auth = fake();
    let err = provider(&url, auth.clone())
        .complete(req())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ferrule login chatgpt"), "{err}");
    assert!(!err.is_transient());
    assert_eq!(
        *auth.refreshes.lock().unwrap(),
        1,
        "never more than one refresh"
    );

    let (url, _seen) = serve(vec![r401()]);
    let refusing = Arc::new(Fake {
        refuse_refresh: true,
        ..Fake::default()
    });
    *refusing.token.lock().unwrap() = "a".into();
    let err = provider(&url, refusing).complete(req()).await.unwrap_err();
    assert!(err.to_string().contains("sign in again"), "{err}");
}

#[tokio::test]
async fn a_usage_limit_waits_for_its_reset_so_the_agent_falls_back() {
    let now = now();
    let body429 =
        json!({"error": {"type": "usage_limit_reached", "message": "You've hit your usage limit.",
        "plan_type": "plus", "resets_at": now + 7440, "limit_window_minutes": 300}})
        .to_string();
    let (url, _seen) = serve(vec![status(
        "429 Too Many Requests",
        "content-type: application/json\r\nx-codex-primary-used-percent: 100\r\nx-codex-primary-window-minutes: 300\r\n",
        body429,
    )]);
    let auth = fake();
    let err = provider(&url, auth.clone())
        .complete(req())
        .await
        .unwrap_err();
    let CoreError::Transient {
        message,
        retry_after,
    } = &err
    else {
        panic!("{err:?}")
    };
    assert!(
        message.contains("usage limit is reached (plus)"),
        "{message}"
    );
    assert!(message.contains("in 2 h 4 min"), "{message}");
    let wait = retry_after.unwrap().as_secs();
    assert!((7400..=7440).contains(&wait), "{wait}");
    // Longer than the retry budget: the loop gives up at once (and M21
    // falls back).
    let policy = ferrule_core::agent::RetryPolicy::default();
    assert_eq!(policy.delay(&err, 1, Duration::ZERO), None);
    let seen = auth.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.limited_until, Some(now + 7440));
    assert_eq!(seen.plan.as_deref(), Some("plus"));
    assert_eq!(seen.windows[0].used_percent, 100.0);

    let (url, _seen) = serve(vec![status(
        "429 Too Many Requests",
        "content-type: application/json\r\n",
        r#"{"error":{"type":"usage_not_included","plan_type":"free"}}"#,
    )]);
    let err = provider(&url, fake()).complete(req()).await.unwrap_err();
    assert!(!err.is_transient());
    assert!(
        err.to_string()
            .contains("plan (free) doesn't include Codex"),
        "{err}"
    );
}

#[tokio::test]
async fn the_model_list_is_the_backends_in_priority_order() {
    let models = r#"{"models":[
      {"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list","priority":12,"supported_in_api":true},
      {"slug":"codex-auto-review","display_name":"x","visibility":"hide","priority":43},
      {"slug":"gpt-6-astra","display_name":"GPT-6 Astra","visibility":"list","priority":1}]}"#;
    let (url, seen) = serve(vec![crate::common::mock::ok(models)]);
    let list = provider(&url, fake()).list_models().await.unwrap();
    assert_eq!(list, ["gpt-6-astra", "gpt-5.5"]);
    let raw = seen.join().unwrap().remove(0);
    assert!(
        raw.starts_with("GET /backend-api/codex/models?client_version=0.153.0 "),
        "{raw}"
    );
    assert!(raw
        .to_ascii_lowercase()
        .contains("chatgpt-account-id: acct_123"));
}

#[test]
fn a_reset_reads_as_a_span_and_a_clock() {
    assert_eq!(
        describe_reset(3600 * 15 + 600, 3600 * 13 + 360),
        "in 2 h 4 min (15:10 UTC)"
    );
    assert_eq!(describe_reset(100, 90), "in 1 min (00:01 UTC)");
    assert_eq!(
        describe_reset(86_400 * 3 + 7200, 0),
        "in 3 d 2 h (02:00 UTC)"
    );
}
