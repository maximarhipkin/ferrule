//! A ChatGPT-plan turn end to end against the mock issuer and backend:
//! the plan's tokens never reach the transcript, the ledger or the logs,
//! and its row is $0 with the notional price.

use super::*;
use ferrule_core::{Agent, AgentConfig, HarnessProfile, ToolContext, ToolRegistry, Transcript};
use ferrule_plans::chatgpt::auth::Tokens;
use ferrule_plans::mock;
use ferrule_providers::codex::{CodexProvider, PlanAuth};
use std::io::Write;

/// Everything logged, at every level.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_plan_turn_keeps_its_tokens_out_of_the_transcript_ledger_and_logs() {
    let logs = Logs::default();
    let writer = logs.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let state = mock::Shared::default();
    let url = mock::serve(state.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (private, data) = (dir.path().join("private"), dir.path().join("data"));
    let plan = Arc::new(ChatGpt::new(&private, Some(&data), mock::issuer(&url)));
    state.lock().unwrap().current_refresh = "refresh-0".into();
    let first = mock::access(0, mock::now() + 3600);
    plan.sign_in(Tokens {
        id_token: Some(mock::id_token()),
        access_token: Some(first.clone()),
        refresh_token: Some("refresh-0".into()),
    })
    .await
    .unwrap();
    // The first token is refused, so the turn refreshes and rotates.
    state.lock().unwrap().refused_access = Some(first.clone());

    let cfg: crate::config::Config = toml::from_str(
        "[providers.chatgpt]\nplan = \"chatgpt\"\nmodel = \"gpt-5.5\"\n\
         price_input_per_mtok = 1.0\nprice_cached_input_per_mtok = 0.5\nprice_output_per_mtok = 2.0\n",
    )
    .unwrap();
    let cat = Arc::new(crate::models::Catalog::from_config(&cfg));
    let (p, q) = (cat.clone(), cat.clone());
    let ledger_path = dir.path().join("ledger.jsonl");
    let sink = Arc::new(
        crate::ledger::FileLedgerSink::new(
            ledger_path.clone(),
            Arc::new(move |a: &str, b: &str| p.price(a, b)),
        )
        .with_plans(Arc::new(move |a: &str| q.plan_of(a))),
    );
    let provider = Arc::new(CodexProvider::new(
        "chatgpt",
        format!("{url}/backend-api/codex"),
        "gpt-5.5",
        Default::default(),
        plan.clone(),
    ));
    let transcripts = dir.path().join("sessions");
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        HarnessProfile::generic(),
        AgentConfig::default(),
        ToolContext::default(),
        Some(Transcript::create(&transcripts, "s1").unwrap()),
    )
    .with_system_prompt("test")
    .with_ledger(sink, "chat", None, "gpt-5.5");
    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    let answer = agent.run("hi", tx.clone()).await.unwrap();
    assert_eq!(answer, "hello from the plan");

    // Then the sign-in is revoked: the turn fails plainly, naming the fix.
    state.lock().unwrap().revoke_all = true;
    let held = plan.credentials().await.unwrap();
    state.lock().unwrap().refused_access = Some(held.access_token.clone());
    let err = agent.run("again", tx).await.unwrap_err().to_string();
    assert!(err.contains("ferrule login chatgpt"), "{err}");

    let s = state.lock().unwrap();
    let mut secrets = vec![first, held.access_token.clone(), s.current_refresh.clone()];
    secrets.extend((0..=s.generation).map(|n| format!("refresh-{n}")));
    let transcript = std::fs::read_to_string(transcripts.join("s1.jsonl")).unwrap();
    let ledger = std::fs::read_to_string(&ledger_path).unwrap();
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for secret in &secrets {
        for (what, text) in [
            ("transcript", &transcript),
            ("ledger", &ledger),
            ("logs", &logs),
            ("error", &err),
        ] {
            assert!(
                !text.contains(secret.as_str()),
                "a token in the {what}: {text}"
            );
        }
    }
    let row: ferrule_core::LedgerRecord =
        serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
    assert_eq!(
        (row.plan.as_deref(), row.cost_usd),
        (Some("chatgpt"), Some(0.0))
    );
    assert!(row.notional_usd.is_some());
}

#[tokio::test]
async fn status_says_the_sign_in_and_the_usage_windows() {
    let dir = tempfile::tempdir().unwrap();
    let (private, data) = (dir.path().join("private"), dir.path().join("data"));
    let cfg: crate::config::Config =
        toml::from_str("[providers.chatgpt]\nplan = \"chatgpt\"\nmodel = \"gpt-5.5\"\n").unwrap();
    assert_eq!(
        status_lines_at(&cfg, &private, &data),
        ["chatgpt: not signed in — `ferrule login chatgpt`"]
    );

    let state = mock::Shared::default();
    let url = mock::serve(state.clone()).await;
    let plan = Arc::new(ChatGpt::new(&private, Some(&data), mock::issuer(&url)));
    mock::signed_in(&state, &private, &url, mock::now() + 3600).await;
    let provider = CodexProvider::new(
        "chatgpt",
        format!("{url}/backend-api/codex"),
        "gpt-5.5",
        Default::default(),
        plan,
    );
    let req = ferrule_core::provider::CompletionRequest {
        messages: vec![ferrule_core::Message::user("hi")],
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        stream: None,
    };
    ferrule_core::provider::Provider::complete(&provider, req)
        .await
        .unwrap();
    let lines = status_lines_at(&cfg, &private, &data);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("chatgpt: signed in as owner@example.com (plus) · "),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("5h") && lines[0].contains("5%"),
        "{}",
        lines[0]
    );

    // No plan in the config: no lines.
    let keyed: crate::config::Config = toml::from_str(
        "[providers.a]\nbase_url = \"http://127.0.0.1:1/v1\"\napi_key_env = \"PATH\"\nmodel = \"m\"\n",
    )
    .unwrap();
    assert!(status_lines_at(&keyed, &private, &data).is_empty());
}
