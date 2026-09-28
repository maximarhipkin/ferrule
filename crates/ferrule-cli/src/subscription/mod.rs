//! M35: signing in with a subscription instead of a key
//! (docs/subscriptions.md, docs/m35-subscriptions.md). The sign-ins live in
//! `ferrule-plans`; this is where the CLI builds a plan's driver, and the
//! lines status and doctor print.

pub mod claude;
pub mod login;

use crate::config::Plan;
use async_trait::async_trait;
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Provider};
use ferrule_core::CoreError;
use ferrule_plans::chatgpt::auth::Issuer;
use ferrule_plans::ChatGpt;
use ferrule_providers::{CodexProvider, DriverOptions};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// The process's ChatGPT sign-in for `issuer` ("" is OpenAI's): one per
/// store, so a refused refresh is seen by every lane.
pub fn chatgpt(issuer: &str) -> anyhow::Result<Arc<ChatGpt>> {
    let private = crate::secrets::private_dir()?;
    let data = crate::config::data_dir()?;
    Ok(chatgpt_at(&private, &data, issuer))
}

/// [`chatgpt`] with this private dir and data dir (the dashboard's).
pub fn chatgpt_at(private: &std::path::Path, data: &std::path::Path, issuer: &str) -> Arc<ChatGpt> {
    /// Keyed by the private dir and the issuer.
    type SignIns = HashMap<(PathBuf, String), Arc<ChatGpt>>;
    static SHARED: OnceLock<Mutex<SignIns>> = OnceLock::new();
    let private = private.to_path_buf();
    let mut all = SHARED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    all.entry((private.clone(), issuer.to_string()))
        .or_insert_with(|| Arc::new(ChatGpt::new(&private, Some(data), Issuer::new(issuer))))
        .clone()
}

/// Models on the ChatGPT plan: the account's own list when signed in
/// (`GET /models`), else the built-in one, so setup works before sign-in.
/// `model` stays free text: a new slug works without a release. Either
/// way, only models the current Codex client version may use (M36).
pub async fn chatgpt_models(issuer: &str) -> Vec<String> {
    let builtin = || {
        let version = ferrule_providers::codex::client_version();
        BUILTIN_CHATGPT_MODELS
            .iter()
            .filter(|m| ferrule_providers::codex::version::builtin_usable(m, &version))
            .map(|m| m.to_string())
            .collect()
    };
    if !matches!(state(Plan::Chatgpt), SignIn::In { .. }) {
        return builtin();
    }
    let Ok(auth) = chatgpt(issuer) else {
        return builtin();
    };
    let codex = CodexProvider::new(
        "chatgpt",
        ferrule_providers::codex::DEFAULT_BASE_URL,
        login::CHATGPT_MODEL,
        DriverOptions::default(),
        auth,
    );
    match codex.list_models().await {
        Ok(models) if !models.is_empty() => models,
        _ => builtin(),
    }
}

/// The Claude Code engine's model names (claude's own aliases).
pub const CLAUDE_CODE_MODELS: &[&str] = &["sonnet", "opus", "haiku"];

/// Setup's Claude plan choice.
pub async fn claude_setup_step(t: &mut crate::setup::Target) -> anyhow::Result<()> {
    claude::setup_step(t).await
}

/// The plan's models at the pinned Codex commit.
pub const BUILTIN_CHATGPT_MODELS: &[&str] = &["gpt-5.5", "gpt-6-astra"];

/// A driver for `model` on a plan. The credential is read per call, so
/// building one never fails; a call without a sign-in says how to sign in.
pub fn client(
    plan: Plan,
    name: &str,
    base_url: &str,
    model: &str,
    options: DriverOptions,
    issuer: &str,
) -> Arc<dyn Provider> {
    match plan {
        Plan::Chatgpt => match chatgpt(issuer) {
            Ok(auth) => Arc::new(CodexProvider::new(name, base_url, model, options, auth)),
            Err(e) => unavailable(name, format!("the ChatGPT plan: {e:#}")),
        },
        Plan::ClaudeCode => claude::client(name, model),
    }
}

fn unavailable(name: &str, why: String) -> Arc<dyn Provider> {
    Arc::new(Unavailable {
        name: name.to_string(),
        why,
    })
}

/// A plan that can't make calls here, and says why on each.
struct Unavailable {
    name: String,
    why: String,
}

#[async_trait]
impl Provider for Unavailable {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        Err(CoreError::Provider(self.why.clone()))
    }
}

/// What a missing sign-in reads as, where a keyed model says its key
/// isn't set.
pub fn not_signed_in(plan: Plan) -> &'static str {
    match plan {
        Plan::Chatgpt => ferrule_plans::chatgpt::NOT_SIGNED_IN,
        Plan::ClaudeCode => "not signed in to the Claude plan: run `ferrule login claude`",
    }
}

/// Where a plan's sign-in stands, read without opening anything sealed.
#[derive(Debug, Clone, PartialEq)]
pub enum SignIn {
    /// Signed in: who, and the plan's name when the sign-in said.
    In {
        email: Option<String>,
        plan: Option<String>,
    },
    /// A refresh was refused: signing in again is the only way back.
    Expired,
    Out,
    /// The store couldn't be read.
    Unreadable(String),
}

impl SignIn {
    pub fn word(&self) -> String {
        match self {
            SignIn::In { email, plan } => {
                let mut s = "signed in".to_string();
                if let Some(e) = email {
                    s.push_str(&format!(" as {e}"));
                }
                if let Some(p) = plan {
                    s.push_str(&format!(" ({p})"));
                }
                s
            }
            SignIn::Expired => "sign-in expired".into(),
            SignIn::Out => "not signed in".into(),
            SignIn::Unreadable(why) => format!("sign-in unreadable: {why}"),
        }
    }
}

/// The ChatGPT sign-in's state under `private`.
pub fn chatgpt_state_at(private: &std::path::Path) -> SignIn {
    match ferrule_plans::chatgpt::store::Store::new(private).meta() {
        Ok(Some(m)) if m.signed_out => SignIn::Expired,
        Ok(Some(m)) => SignIn::In {
            email: m.email,
            plan: m.plan,
        },
        Ok(None) => SignIn::Out,
        Err(e) => SignIn::Unreadable(format!("{e:#}")),
    }
}

pub fn state(plan: Plan) -> SignIn {
    match plan {
        Plan::Chatgpt => match crate::secrets::private_dir() {
            Ok(private) => chatgpt_state_at(&private),
            Err(e) => SignIn::Unreadable(format!("{e:#}")),
        },
        Plan::ClaudeCode => claude::state(),
    }
}

/// "signed in" or "not signed in — `ferrule login chatgpt`", for a
/// provider line in setup.
pub fn sign_in_word(plan: Plan) -> String {
    words(plan, state(plan))
}

fn words(plan: Plan, state: SignIn) -> String {
    match state {
        s @ SignIn::In { .. } => s.word(),
        s => format!("{} — `ferrule login {}`", s.word(), plan.login_word()),
    }
}

/// One line per plan the config uses: the sign-in and the usage windows.
/// For `ferrule status`, `/status` and doctor.
pub fn status_lines(cfg: &crate::config::Config) -> Vec<String> {
    let (Ok(private), Ok(data)) = (crate::secrets::private_dir(), crate::config::data_dir()) else {
        return vec![];
    };
    status_lines_at(cfg, &private, &data)
}

/// [`status_lines`] with the sign-ins under `private` and the usage file
/// under `data`.
pub fn status_lines_at(
    cfg: &crate::config::Config,
    private: &std::path::Path,
    data: &std::path::Path,
) -> Vec<String> {
    let mut plans: Vec<Plan> = cfg.providers.values().filter_map(|p| p.plan).collect();
    plans.sort_by_key(|p| p.as_str());
    plans.dedup();
    let usage = ferrule_plans::UsageFile::new(data);
    let now = now();
    plans
        .into_iter()
        .map(|plan| {
            let signed = match plan {
                Plan::Chatgpt => chatgpt_state_at(private),
                other => state(other),
            };
            let mut line = format!("{plan}: {}", words(plan, signed));
            if let Some(r) = usage.get(plan.as_str()) {
                line.push_str(&format!(" · {}", r.line(now)));
            }
            line
        })
        .collect()
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests;
