//! The Cloud Code driver against canned replies on 127.0.0.1: how a
//! stream ends (a finish reason, a cut, a block) and what each error
//! reply becomes.

use super::*;
use crate::common::mock::{serve, sse, sse_cut, status, Canned};
use ferrule_core::failure::{classify, retry_after, Kind};
use ferrule_core::message::Message;

/// A token source that counts what the driver asked of it.
#[derive(Default)]
struct Fake {
    refreshes: Mutex<u32>,
    seen: Mutex<Vec<RateLimits>>,
}

#[async_trait::async_trait]
impl PlanAuth for Fake {
    async fn credentials(&self) -> Result<PlanCredentials, CoreError> {
        Ok(PlanCredentials {
            access_token: format!("ya29.tok-{}", self.refreshes.lock().unwrap()),
            account_id: Some("proj-1".into()),
            fedramp: false,
        })
    }
    async fn refused(&self, _used: &str) -> Result<(), CoreError> {
        *self.refreshes.lock().unwrap() += 1;
        Ok(())
    }
    fn observe(&self, limits: &RateLimits) {
        self.seen.lock().unwrap().push(limits.clone());
    }
}

fn provider(url: &str, auth: Arc<Fake>) -> CloudCodeProvider {
    CloudCodeProvider::new(
        "gemini-cli",
        url,
        "gemini-3.8-flash-tiered",
        DriverOptions::default(),
        auth,
    )
    .with_identity(Arc::new(
        version::ClientIdentity::new(None).with_override(Some("1.9.3".into())),
    ))
}

fn req() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::system("You are Ferrule."), Message::user("hi")],
        tools: vec![],
        max_output_tokens: Some(512),
        temperature: None,
        stream: None,
    }
}

fn chunk(v: Value) -> String {
    format!("data: {}\n\n", json!({ "response": v }))
}

fn text(t: &str) -> Value {
    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t}]}}]})
}

fn done(t: &str) -> Value {
    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t}]},
                           "finishReason": "STOP"}],
           "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3}})
}

/// The quota summary the driver reads after a streamed turn.
fn quota() -> Canned {
    status("200 OK", "content-type: application/json\r\n", "{}")
}

async fn run(
    replies: Vec<Canned>,
) -> (
    Result<CompletionResponse, CoreError>,
    Vec<String>,
    Arc<Fake>,
) {
    let (url, handle) = serve(replies);
    let auth = Arc::new(Fake::default());
    let res = provider(&url, auth.clone()).complete(req()).await;
    (res, handle.join().unwrap(), auth)
}

#[tokio::test]
async fn an_answer_split_across_chunks_reads_whole_and_says_who_is_calling() {
    let first = chunk(text("Hel"));
    let second = chunk(done("lo"));
    // Event boundaries fall mid-event: the parser must stitch them.
    let all = format!("{first}{second}");
    let (a, rest) = all.split_at(first.len() - 7);
    let (b, c) = rest.split_at(20);
    let (res, seen, _) = run(vec![sse(&[a, b, c]), quota()]).await;
    let resp = res.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("Hello"));
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(resp.usage.input_tokens, 10);
    // Defect 7: one client identity, its version in both headers.
    let request = seen[0].to_ascii_lowercase();
    assert!(
        request.contains("user-agent: antigravity/1.9.3 "),
        "{request}"
    );
    assert!(
        request.contains("x-goog-api-client: gl-go/1.23.0 antigravity/1.9.3"),
        "{request}"
    );
    assert_eq!(crate::common::mock::body(&seen[0])["project"], "proj-1");
}

#[tokio::test]
async fn a_stream_that_ends_without_a_finish_reason_is_cut_off_not_an_answer() {
    let (res, _, _) = run(vec![sse(&[&chunk(text("Half an ans"))]), quota()]).await;
    let e = res.unwrap_err();
    assert!(e.is_transient(), "{e}");
    assert!(e.to_string().contains("stream ended early"), "{e}");

    // The connection dropping mid-way is the same: another try may pass.
    let (res, _, _) = run(vec![sse_cut(&[&chunk(text("Half"))]), quota()]).await;
    assert!(res.unwrap_err().is_transient());
}

#[tokio::test]
async fn empty_candidates_are_no_answer() {
    let only_usage = chunk(json!({"candidates": [], "usageMetadata": {"promptTokenCount": 4}}));
    let (res, _, _) = run(vec![sse(&[&only_usage]), quota()]).await;
    let e = res.unwrap_err();
    assert!(e.is_transient(), "{e}");
    assert_ne!(classify(&e), Kind::Refused);
}

#[tokio::test]
async fn a_blocked_prompt_and_a_safety_stop_are_refusals_in_words() {
    let blocked = chunk(json!({
        "promptFeedback": {"blockReason": "SAFETY", "blockReasonMessage": "The prompt was blocked."},
        "usageMetadata": {"promptTokenCount": 4}
    }));
    let (res, _, _) = run(vec![sse(&[&blocked]), quota()]).await;
    let e = res.unwrap_err();
    assert_eq!(classify(&e), Kind::Refused);
    assert!(!e.is_transient());
    let said = ferrule_core::failure::chat_text(&e);
    assert!(said.contains("the model refused to answer this."), "{said}");
    assert!(
        said.contains("Gemini blocked the prompt (blockReason SAFETY: The prompt was blocked.)"),
        "{said}"
    );

    let recited = chunk(json!({"candidates": [{
        "content": {"role": "model", "parts": [{"text": "Once upon"}]},
        "finishReason": "RECITATION"}]}));
    let (res, _, _) = run(vec![sse(&[&recited]), quota()]).await;
    let e = res.unwrap_err();
    assert_eq!(classify(&e), Kind::Refused);
    assert!(e.to_string().contains("finishReason RECITATION"), "{e}");
}

#[tokio::test]
async fn an_error_object_mid_stream_is_the_plans_limit_and_is_recorded() {
    let error = format!(
        "data: {}\n\n",
        json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
            "message": "You have exhausted your capacity on this model.",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "QUOTA_EXHAUSTED", "metadata": {"quotaResetDelay": "3h"}}]}})
    );
    let (res, _, auth) = run(vec![sse(&[&chunk(text("Wo")), &error]), quota()]).await;
    let e = res.unwrap_err();
    assert_eq!(classify(&e), Kind::UsageLimit);
    assert!(e.to_string().contains("it resets in 3 h 0 min"), "{e}");
    let seen = auth.seen.lock().unwrap();
    assert!(seen.iter().any(|l| l.limited_until.is_some()), "{seen:?}");
}

#[tokio::test]
async fn a_429_reply_is_the_plans_limit_with_its_reset_and_no_retry_here() {
    let body = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
    "message": "You have exhausted your capacity on this model. Your quota will reset after 1h30m.",
    "details": [
        {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "QUOTA_EXHAUSTED",
         "metadata": {"model": "gemini-3.8-flash-tiered", "quotaResetDelay": "1h30m"}},
        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "5400s"}
    ]}});
    let (res, seen, auth) = run(vec![status(
        "429 Too Many Requests",
        "content-type: application/json\r\n",
        body.to_string(),
    )])
    .await;
    let e = res.unwrap_err();
    assert_eq!(seen.len(), 1);
    assert_eq!(classify(&e), Kind::UsageLimit);
    assert_eq!(retry_after(&e), Some(Duration::from_secs(5400)));
    let words = e.plain_words().unwrap();
    assert!(
        words.starts_with(
            "the Gemini plan's usage limit is reached for gemini-3.8-flash-tiered; it resets in 1 h 30 min"
        ),
        "{words}"
    );
    assert!(!e.to_string().contains("\"details\""), "{e}");
    assert!(auth.seen.lock().unwrap()[0].limited_until.is_some());
}

#[tokio::test]
async fn a_403_says_no_tier_and_a_401_after_a_refresh_says_sign_in() {
    let denied = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
        "message": "The caller does not have permission"}});
    let (res, _, _) = run(vec![status(
        "403 Forbidden",
        "content-type: application/json\r\n",
        denied.to_string(),
    )])
    .await;
    let e = res.unwrap_err();
    assert_eq!(classify(&e), Kind::GeminiNoTier);
    assert!(
        ferrule_core::failure::chat_text(&e).contains("no Gemini Code Assist / Antigravity tier")
    );

    let unauthenticated = r#"{"error":{"code":401,"status":"UNAUTHENTICATED"}}"#;
    let (res, seen, auth) = run(vec![
        status(
            "401 Unauthorized",
            "content-type: application/json\r\n",
            unauthenticated,
        ),
        status(
            "401 Unauthorized",
            "content-type: application/json\r\n",
            unauthenticated,
        ),
    ])
    .await;
    let e = res.unwrap_err();
    assert_eq!(seen.len(), 2);
    assert_eq!(*auth.refreshes.lock().unwrap(), 1);
    assert_eq!(classify(&e), Kind::GeminiSignin);
    let said = ferrule_core::failure::chat_text(&e);
    assert!(!said.contains("API key"), "{said}");
    assert!(said.contains("Models page"), "{said}");
}
