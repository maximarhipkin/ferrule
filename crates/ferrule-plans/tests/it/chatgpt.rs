//! The ChatGPT sign-in against a mock issuer (and a mock Codex backend) on
//! 127.0.0.1: device code, PKCE through the loopback listener, the pasted
//! redirect, refresh with rotation, a refused refresh, logout, two OS
//! processes refreshing at once, and a 401 answered by one refresh.

use ferrule_core::error::CoreError;
use ferrule_core::message::Message;
use ferrule_core::provider::{CompletionRequest, Provider};
use ferrule_plans::chatgpt::auth::{self, Issuer, Pkce};
use ferrule_plans::chatgpt::{ChatGpt, EXPIRED, NOT_SIGNED_IN};
use ferrule_plans::mock::*;
use ferrule_providers::codex::{CodexProvider, PlanAuth};
use ferrule_providers::DriverOptions;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn device_code_sign_in_polls_then_trades_the_code() {
    let state = Shared::default();
    state.lock().unwrap().device_pending = 1;
    let url = serve(state.clone()).await;
    let iss = issuer(&url);
    let code = auth::start_device(&http(), &iss).await.unwrap().unwrap();
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.page, format!("{url}/codex/device"));
    assert!(!format!("{code:?}").contains("dev_secret"));
    let tokens = auth::finish_device(&http(), &iss, &code).await.unwrap();
    assert!(tokens.refresh_token.is_some());

    let s = state.lock().unwrap();
    let polls = s
        .requests
        .iter()
        .filter(|r| r.1 == "/api/accounts/deviceauth/token")
        .count();
    assert_eq!(polls, 2, "one pending poll, then the code");
    let ex = &s.exchanges[0];
    assert_eq!(ex["code"], "device-code");
    assert_eq!(ex["code_verifier"], "device-verifier");
    assert_eq!(ex["redirect_uri"], format!("{url}/deviceauth/callback"));
}

#[tokio::test]
async fn a_404_means_no_device_sign_in() {
    // An issuer that knows nothing: every path is a 404.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let _ = read_request(&mut sock).await;
            let _ = sock
                .write_all(reply("404 Not Found", "text/plain", "").as_bytes())
                .await;
        }
    });
    assert!(auth::start_device(&http(), &issuer(&url))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn browser_sign_in_comes_back_to_the_loopback_listener() {
    let state = Shared::default();
    let url = serve(state.clone()).await;
    let iss = issuer(&url);
    let pkce = Pkce::new();
    assert_eq!(pkce.verifier.len(), 86, "64 bytes, base64url");
    let st = auth::new_state();
    let cb = auth::Callback::bind(&iss).await.unwrap();
    let redirect = cb.redirect.clone();
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/auth/callback"));

    let page = auth::authorize_url(&iss, &redirect, &pkce, &st);
    let q: Vec<String> = url::Url::parse(&page)
        .unwrap()
        .query_pairs()
        .map(|(k, _)| k.into_owned())
        .collect();
    assert_eq!(
        q,
        [
            "response_type",
            "client_id",
            "redirect_uri",
            "code_challenge",
            "code_challenge_method",
            "state",
            "scope",
            "id_token_add_organizations",
            "codex_cli_simplified_flow",
            "originator"
        ]
    );
    assert!(page.starts_with(&format!("{url}/oauth/authorize?")));

    let browser = {
        let redirect = redirect.clone();
        let st = st.clone();
        tokio::spawn(async move {
            let base = redirect.trim_end_matches("/auth/callback").to_string();
            let fav = http()
                .get(format!("{base}/favicon.ico"))
                .send()
                .await
                .unwrap();
            assert_eq!(fav.status().as_u16(), 404);
            let done = http()
                .get(format!("{redirect}?code=browser-code&state={st}"))
                .send()
                .await
                .unwrap();
            done.text().await.unwrap()
        })
    };
    let code = cb.wait(&st, Duration::from_secs(10)).await.unwrap();
    assert_eq!(code, "browser-code");
    assert!(browser.await.unwrap().contains("Signed in"));
    auth::exchange(&http(), &iss, &code, &pkce.verifier, &redirect)
        .await
        .unwrap();
    let ex = &state.lock().unwrap().exchanges[0];
    assert_eq!(ex["code_verifier"], pkce.verifier);
    assert_eq!(ex["redirect_uri"], redirect);
}

#[test]
fn a_pasted_redirect_is_checked_like_the_listeners() {
    let ok = auth::code_from_redirect(
        "here it is: http://127.0.0.1:1455/auth/callback?code=abc&scope=x&state=S1",
        "S1",
    );
    assert_eq!(ok.unwrap(), "abc");
    let wrong = auth::code_from_redirect(
        "http://127.0.0.1:1455/auth/callback?code=abc&state=S2",
        "S1",
    );
    assert!(wrong.unwrap_err().to_string().contains("another sign-in"));
    let refused = auth::code_from_redirect(
        "http://127.0.0.1:1455/auth/callback?error=access_denied&state=S1",
        "S1",
    );
    assert!(refused.unwrap_err().to_string().contains("access_denied"));
    assert!(auth::code_from_redirect("just some text", "S1").is_err());
}

#[tokio::test]
async fn the_store_seals_the_tokens_and_refreshes_ahead_of_expiry_with_rotation() {
    let state = Shared::default();
    let url = serve(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    // Expires in 2 minutes: inside the 5-minute margin.
    let plan = signed_in(&state, &private, &url, now() + 120).await;

    let meta = plan.store().meta().unwrap().unwrap();
    assert_eq!(meta.account_id.as_deref(), Some("acct_42"));
    assert_eq!(meta.plan.as_deref(), Some("plus"));
    assert_eq!(meta.email.as_deref(), Some("owner@example.com"));
    let on_disk = std::fs::read_to_string(plan.store().path()).unwrap();
    assert!(!on_disk.contains("refresh-0"), "sealed: {on_disk}");
    let before = plan.store().load().unwrap().unwrap();
    assert!(!on_disk.contains(&before.secrets.access_token), "{on_disk}");
    assert!(!on_disk.contains(&before.secrets.id_token), "{on_disk}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(plan.store().path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let creds = plan.credentials().await.unwrap();
    assert_eq!(creds.account_id.as_deref(), Some("acct_42"));
    assert!(!format!("{creds:?}").contains(&creds.access_token));
    assert_eq!(state.lock().unwrap().refreshes, 1);
    let record = plan.store().load().unwrap().unwrap();
    assert_eq!(record.secrets.refresh_token, "refresh-1", "rotated in");
    assert_eq!(creds.access_token, record.secrets.access_token);
    assert!(!format!("{record:?}").contains("refresh-1"));

    // Fresh now: no second refresh.
    plan.credentials().await.unwrap();
    assert_eq!(state.lock().unwrap().refreshes, 1);
}

#[tokio::test]
async fn a_refused_refresh_signs_out_and_logout_still_revokes() {
    let state = Shared::default();
    let url = serve(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    let plan = signed_in(&state, &private, &url, now() - 10).await;
    state.lock().unwrap().revoke_all = true;

    let err = plan.credentials().await.unwrap_err();
    assert_eq!(err.to_string(), format!("provider error: {EXPIRED}"));
    assert!(plan.refresh_refused());
    assert!(plan.store().meta().unwrap().unwrap().signed_out);
    // Never tried again.
    plan.credentials().await.unwrap_err();
    assert_eq!(state.lock().unwrap().refreshes, 1);

    let out = plan.log_out().await.unwrap();
    assert!(out.was_signed_in && out.revoked);
    assert_eq!(state.lock().unwrap().revoked, ["refresh-0"]);
    assert!(!plan.store().path().exists());
    let err = plan.credentials().await.unwrap_err();
    assert!(err.to_string().contains(NOT_SIGNED_IN));
    let again = plan.log_out().await.unwrap();
    assert!(!again.was_signed_in && !again.revoked);
}

#[tokio::test]
async fn logout_deletes_even_when_the_revoke_fails() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    let state = Shared::default();
    // Nothing listens there.
    let plan = signed_in(&state, &private, "http://127.0.0.1:9", now() + 3600).await;
    let out = plan.log_out().await.unwrap();
    assert!(out.was_signed_in && !out.revoked);
    assert!(!plan.store().path().exists());
}

#[tokio::test]
async fn a_transient_refresh_failure_keeps_a_token_that_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    let state = Shared::default();
    let plan = signed_in(&state, &private, "http://127.0.0.1:9", now() + 120).await;
    let creds = plan.credentials().await.unwrap();
    assert_eq!(
        creds.access_token,
        plan.store().load().unwrap().unwrap().secrets.access_token
    );

    let expired = signed_in(&state, &private, "http://127.0.0.1:9", now() - 1).await;
    assert!(expired.credentials().await.unwrap_err().is_transient());
}

const CHILD_DIR: &str = "FERRULE_PLANS_TEST_CHILD_DIR";
const CHILD_ISSUER: &str = "FERRULE_PLANS_TEST_CHILD_ISSUER";

/// Run as a separate process by the test below; a no-op otherwise.
#[tokio::test]
async fn child_refresh() {
    let (Ok(dir), Ok(url)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_ISSUER)) else {
        return;
    };
    let plan = ChatGpt::new(std::path::Path::new(&dir), None, Issuer::new(&url));
    let creds = plan.credentials().await.unwrap();
    println!("ACCESS={}", creds.access_token);
}

#[tokio::test]
async fn two_processes_refreshing_at_once_make_one_refresh() {
    let state = Shared::default();
    state.lock().unwrap().refresh_delay_ms = 400;
    let url = serve(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    signed_in(&state, &private, &url, now() - 10).await;

    let exe = std::env::current_exe().unwrap();
    let spawn = || {
        tokio::process::Command::new(&exe)
            .args([
                "--exact",
                "chatgpt::child_refresh",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_DIR, &private)
            .env(CHILD_ISSUER, &url)
            .env_remove(ferrule_connections::seal::KEY_ENV)
            .output()
    };
    let (a, b) = tokio::join!(spawn(), spawn());
    let token = |out: std::process::Output| {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.split_once("ACCESS=").map(|(_, t)| t.trim().to_string()))
            .expect("the child printed its token")
    };
    let (a, b) = (token(a.unwrap()), token(b.unwrap()));
    assert_eq!(a, b, "both use the one refreshed token");
    let s = state.lock().unwrap();
    assert_eq!(s.refreshes, 1, "exactly one refresh reached the issuer");
    assert_eq!(s.current_refresh, "refresh-1");
    assert!(!private.join("plans/chatgpt.lock").exists());
}

fn request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::user("hi")],
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    }
}

#[tokio::test]
async fn a_401_from_the_backend_refreshes_once_and_the_turn_goes_through() {
    let state = Shared::default();
    let url = serve(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    let data = dir.path().join("data");
    let plan = Arc::new(ChatGpt::new(&private, Some(&data), issuer(&url)));
    state.lock().unwrap().current_refresh = "refresh-0".into();
    let first = access(0, now() + 3600);
    plan.sign_in(auth::Tokens {
        id_token: Some(id_token()),
        access_token: Some(first.clone()),
        refresh_token: Some("refresh-0".into()),
    })
    .await
    .unwrap();
    // Revoked server-side before its exp.
    state.lock().unwrap().refused_access = Some(first.clone());

    let provider = CodexProvider::new(
        "chatgpt",
        format!("{url}/backend-api/codex"),
        "gpt-5.5",
        DriverOptions::default(),
        plan.clone(),
    );
    let resp = provider.complete(request()).await.unwrap();
    assert_eq!(resp.message.content.as_deref(), Some("hello from the plan"));
    let s = state.lock().unwrap();
    assert_eq!(s.refreshes, 1);
    let calls = s
        .requests
        .iter()
        .filter(|r| r.1 == "/backend-api/codex/responses")
        .count();
    assert_eq!(calls, 2);
    // The usage file got the headers' reading.
    let reading = ferrule_plans::UsageFile::new(&data).get("chatgpt").unwrap();
    assert_eq!(reading.windows[0].name, "5h");
    assert_eq!(reading.windows[0].used_percent, 5.0);
}

#[tokio::test]
async fn not_signed_in_is_a_plain_error_with_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let plan = ChatGpt::new(&dir.path().join("private"), None, Issuer::default());
    let err: CoreError = plan.credentials().await.unwrap_err();
    assert!(!err.is_transient());
    assert!(err.to_string().contains("ferrule login chatgpt"));
}

/// One real turn on a signed-in ChatGPT plan. Needs network and a sign-in
/// (`ferrule login chatgpt`); point `FERRULE_LIVE_PRIVATE` at its
/// `private/` directory:
/// `FERRULE_LIVE_PRIVATE=~/.local/share/ferrule/private cargo test -p ferrule-plans --test it chatgpt::live -- --ignored`
#[tokio::test]
#[ignore = "network and a ChatGPT sign-in"]
async fn live_chatgpt_turn() {
    let private = std::env::var("FERRULE_LIVE_PRIVATE").expect("FERRULE_LIVE_PRIVATE");
    let plan = Arc::new(ChatGpt::new(
        std::path::Path::new(&private),
        None,
        Issuer::default(),
    ));
    let provider = CodexProvider::new(
        "chatgpt",
        "",
        "gpt-5.5",
        DriverOptions::default(),
        plan.clone(),
    );
    let models = provider.list_models().await.unwrap();
    println!("models: {models:?}");
    let resp = provider.complete(request()).await.unwrap();
    println!("reply: {:?}", resp.message.content);
    assert!(resp.message.content.is_some_and(|t| !t.is_empty()));
}
