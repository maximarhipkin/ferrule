//! The Gemini / Antigravity plan in the binary:
//! - By default, speaks Google OAuth 2.0 PKCE + Cloud Code `v1internal`
//!   natively over HTTPS (`GeminiPlan` + `CloudCodeProvider`), with tokens
//!   sealed in `private/plans/gemini.json` — zero external CLI (`agy` or
//!   `gemini`) required on the server.
//! - If `[plans.gemini_cli] binary` is explicitly set in `ferrule.toml`, can
//!   also run through that CLI binary as a subprocess engine.

use super::login::Flow;
use super::SignIn;
use crate::config::{self, Config, GeminiCliPlanConfig, Plan};
use anyhow::{bail, Result};
use ferrule_core::provider::Provider;
use ferrule_plans::gemini::auth::{self, Callback, Issuer, Pkce, Tokens};
use ferrule_plans::gemini::cli;
use ferrule_plans::gemini::store::Store;
use ferrule_plans::gemini::{EngineConfig, GeminiCli, GeminiPlan};
use ferrule_providers::{CloudCodeProvider, DriverOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The hosts Cloud Code / Antigravity and Google OAuth talk to.
pub const GOOGLE_HOSTS: &[&str] = &[
    "daily-cloudcode-pa.googleapis.com",
    "autopush-cloudcode-pa.googleapis.com",
    "cloudcode-pa.googleapis.com",
    "generativelanguage.googleapis.com",
    "oauth2.googleapis.com",
    "accounts.google.com",
    "www.googleapis.com",
];

/// What works under the native Gemini / Antigravity plan.
pub const FEATURES: &str = "\
  ferrule feature            under the Gemini plan (native Cloud Code HTTPS driver)
  tools and approvals        all native ferrule tools, sandbox, and approval gates
  memory, tasks, messaging   yes, native
  compaction & repo-map      yes, native (1M context window profile)
  routing & fallbacks        yes, native
  streaming, sub-agents      yes, native
  external CLI required      no (signs in and runs directly from ferrule)
  cost                       $0 on the ledger (covered by Google / Antigravity plan)";

/// The plan's default model a new `[providers.gemini-cli]` starts on.
pub const MODEL: &str = "gemini-4-argon-medium";

const BROWSER_LIMIT: Duration = Duration::from_secs(10 * 60);

/// `[plans.gemini_cli]`, the defaults when there's no config.
pub fn settings() -> GeminiCliPlanConfig {
    Config::load()
        .map(|(c, _)| c.plans.gemini_cli)
        .unwrap_or_default()
}

pub fn config_dir(s: &GeminiCliPlanConfig) -> Result<PathBuf> {
    Ok(s.config_dir(&config::data_dir()?))
}

/// The process's native Gemini sign-in for `issuer` ("" is Google's): one
/// per store, so a refused refresh is seen by every lane.
pub fn plan(issuer: &str) -> Result<Arc<GeminiPlan>> {
    let private = crate::secrets::private_dir()?;
    let data = crate::config::data_dir()?;
    let p = plan_at(&private, &data, issuer);
    if let Ok(dir) = config_dir(&settings()) {
        let _ = p.store().import_from_cli_if_empty(&dir);
    }
    Ok(p)
}

/// [`plan`] with explicit `private` and `data` directories (for the dashboard
/// and tests).
pub fn plan_at(private: &Path, data: &Path, issuer: &str) -> Arc<GeminiPlan> {
    type SignIns = HashMap<(PathBuf, String), Arc<GeminiPlan>>;
    static SHARED: OnceLock<Mutex<SignIns>> = OnceLock::new();
    let private = private.to_path_buf();
    let mut all = SHARED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    all.entry((private.clone(), issuer.to_string()))
        .or_insert_with(|| Arc::new(GeminiPlan::new(&private, Some(data), Issuer::new(issuer))))
        .clone()
}

pub fn engine_config(cfg: &Config) -> Result<EngineConfig> {
    let s = &cfg.plans.gemini_cli;
    let data = config::data_dir()?;
    let dir = s.config_dir(&data);
    let workspace = std::env::current_dir().unwrap_or_else(|_| data.clone());
    let mut e = EngineConfig::new(s.binary(), dir.clone(), workspace.clone());
    e.data_dir = Some(data);
    e.turn_timeout = Duration::from_secs(s.turn_timeout_minutes.max(1) * 60);
    e.max_output_bytes = s.max_output_mb.max(1) * 1024 * 1024;
    e.bridge_command = std::env::current_exe()
        .ok()
        .map(|exe| (exe, vec!["claude-mcp".to_string()]));
    e.sandbox = Some(crate::engine_sandbox(cfg, &dir)?);
    e.guest_hidden = crate::owner_mode::guest_hidden(&workspace);
    Ok(e)
}

/// Builds the provider driver for a `plan = "gemini-cli"` entry:
/// uses the native `CloudCodeProvider` over HTTPS by default, or the
/// `GeminiCli` subprocess engine if `[plans.gemini_cli] binary` was
/// explicitly configured.
pub fn client(
    name: &str,
    base_url: &str,
    model: &str,
    options: DriverOptions,
    issuer: &str,
) -> Arc<dyn Provider> {
    if let Ok((cfg, _)) = Config::load() {
        if !cfg.plans.gemini_cli.binary.trim().is_empty() {
            if let Ok(e) = engine_config(&cfg) {
                if cli::find(&e.binary).is_some() {
                    return Arc::new(GeminiCli::new(name, model, e));
                }
            }
        }
    }
    match plan(issuer) {
        Ok(auth) => Arc::new(CloudCodeProvider::new(name, base_url, model, options, auth)),
        Err(e) => super::unavailable(name, format!("the Gemini plan: {e:#}")),
    }
}

/// Models on the Gemini / Antigravity plan: the account's live list from
/// `v1internal:fetchAvailableModels` when signed in, else the built-in list.
pub async fn models(issuer: &str) -> Vec<String> {
    let builtin = || {
        super::GEMINI_CLI_MODELS
            .iter()
            .map(|m| m.to_string())
            .collect()
    };
    if !matches!(state(), SignIn::In { .. }) {
        return builtin();
    }
    let Ok(auth) = plan(issuer) else {
        return builtin();
    };
    let cc = CloudCodeProvider::new(
        "gemini-cli",
        ferrule_providers::cloudcode::DEFAULT_BASE_URL,
        MODEL,
        DriverOptions::default(),
        auth,
    );
    match cc.list_models().await {
        Ok(list) if !list.is_empty() => list,
        _ => builtin(),
    }
}

/// The Gemini sign-in state under `private`, without opening sealed tokens.
pub fn state_at(private: &Path) -> SignIn {
    match Store::new(private).meta() {
        Ok(Some(m)) if m.signed_out => SignIn::Expired,
        Ok(Some(m)) => SignIn::In {
            email: m.email,
            plan: m.plan,
        },
        Ok(None) => SignIn::Out,
        Err(e) => SignIn::Unreadable(format!("{e:#}")),
    }
}

/// Where the Gemini plan's sign-in stands on this machine (importing an
/// existing local Antigravity / Gemini CLI token into `private/plans/gemini.json`
/// if one is present and no sealed sign-in exists yet).
pub fn state() -> SignIn {
    let Ok(private) = crate::secrets::private_dir() else {
        return SignIn::Out;
    };
    let store = Store::new(&private);
    if store.meta().ok().flatten().is_none() {
        if let Ok(dir) = config_dir(&settings()) {
            let _ = store.import_from_cli_if_empty(&dir);
        }
    }
    match state_at(&private) {
        SignIn::Out => {
            if let Ok(dir) = config_dir(&settings()) {
                let a = cli::auth_status(&dir);
                if a.is_plan() {
                    return SignIn::In {
                        email: a.email,
                        plan: a.auth_method.or_else(|| Some("antigravity".into())),
                    };
                }
            }
            SignIn::Out
        }
        other => other,
    }
}

/// Native Google OAuth 2.0 PKCE sign-in at the terminal (`ferrule login gemini`,
/// `--browser`, or `--paste`), storing the sealed tokens in `private/plans/gemini.json`.
pub async fn login(flow: Flow) -> Result<()> {
    let who = sign_in_gemini("", flow).await?;
    println!("{who}.");
    if let Some(line) = super::login::add_provider(Plan::GeminiCli, MODEL)? {
        println!("{line}");
    }
    println!("Models on the plan: {}.", models("").await.join(", "));
    Ok(())
}

/// Signs in to the Gemini plan at the terminal and stores the sealed tokens.
pub async fn sign_in_gemini(issuer: &str, flow: Flow) -> Result<String> {
    let p = plan(issuer)?;
    let tokens = gemini_tokens(&p, flow).await?;
    let meta = p.sign_in(tokens).await?;
    let who = SignIn::In {
        email: meta.email,
        plan: meta.plan,
    }
    .word();
    let mut chars = who.chars();
    let cap = chars
        .next()
        .map(|f| f.to_uppercase().chain(chars).collect::<String>())
        .unwrap_or_default();
    Ok(cap)
}

async fn gemini_tokens(plan: &GeminiPlan, flow: Flow) -> Result<Tokens> {
    let (http, issuer) = (plan.http(), plan.issuer());
    let callback = match flow {
        Flow::Paste => None,
        _ => match Callback::bind(issuer).await {
            Ok(cb) => Some(cb),
            Err(e) => {
                println!("{e:#}");
                None
            }
        },
    };
    let redirect = match &callback {
        Some(cb) => cb.redirect.clone(),
        None => format!(
            "http://localhost:{}/auth/callback",
            issuer.ports.first().copied().unwrap_or(51121)
        ),
    };
    let pkce = Pkce::new();
    let state = auth::new_state();
    println!(
        "Open this address in a browser and sign in with your Google account:\n\n{}\n",
        auth::authorize_url(issuer, &redirect, &pkce, &state)
    );
    let code = match callback {
        Some(cb) => {
            println!(
                "If the browser is on another machine (e.g. over SSH), the localhost page it \
                 redirects to won't load: copy that page's full address from the browser's \
                 address bar and paste it here."
            );
            tokio::select! {
                code = cb.wait(&state, BROWSER_LIMIT) => code?,
                code = pasted(&state) => code?,
            }
        }
        None => {
            println!(
                "After signing in, the localhost page it redirects to won't load: \
                 copy that page's full address from the browser's address bar and paste it here."
            );
            pasted(&state).await?
        }
    };
    auth::exchange(http, issuer, &code, &pkce.verifier, &redirect).await
}

async fn pasted(state: &str) -> Result<String> {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match auth::code_from_redirect(line.trim(), state) {
            Ok(code) => return Ok(code),
            Err(e) => println!("{e:#}. Paste the whole redirect address."),
        }
    }
    bail!("no redirect address was pasted")
}

/// `ferrule logout gemini`: revokes the refresh token with Google and deletes
/// the sealed `private/plans/gemini.json` store.
pub async fn logout() -> Result<()> {
    // Not `plan("")`: that imports the CLI's token first, and logout would
    // then revoke the CLI's own sign-in.
    let private = crate::secrets::private_dir()?;
    let data = crate::config::data_dir()?;
    let out = plan_at(&private, &data, "").log_out().await?;
    match (out.was_signed_in, out.revoked) {
        (false, _) => println!("There was no Gemini plan sign-in here."),
        (true, true) => println!("Signed out of the Gemini plan; Google revoked the sign-in."),
        (true, false) => println!("Signed out of the Gemini plan here."),
    }
    Ok(())
}

pub async fn setup_step(t: &mut crate::setup::Target) -> Result<bool> {
    use crate::setup::{info, ok};
    info("The Gemini plan signs in directly with your Google / Antigravity subscription (no external CLI needed):");
    println!("{FEATURES}");
    match state() {
        s @ SignIn::In { .. } => ok(format!("the Gemini plan: {}", s.word())),
        _ => {
            const WAYS: [&str; 3] = [
                "Sign in with Google (browser or paste redirect URL)",
                "Paste redirect URL (for a remote / SSH server)",
                "Not now",
            ];
            let way = inquire::Select::new("How should the Gemini plan sign in?", WAYS.to_vec())
                .raw_prompt()?
                .index;
            let flow = match way {
                0 => Flow::Browser,
                1 => Flow::Paste,
                _ => return Ok(false),
            };
            let who = crate::setup::interruptible(sign_in_gemini("", flow)).await??;
            ok(who);
        }
    }
    let cfg = t.config()?;
    let existing = cfg
        .providers
        .iter()
        .find(|(_, p)| p.plan == Some(Plan::GeminiCli))
        .map(|(n, p)| (n.clone(), p.model.clone()));
    let name = existing
        .as_ref()
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "gemini-cli".into());
    let current = existing.map(|(_, m)| m).unwrap_or_else(|| MODEL.into());
    let avail = models("").await;
    let model = crate::setup::pick_model(&avail, &current)?;
    let tbl = crate::setup::table(t.root(), &["providers", &name])?;
    crate::setup::put(tbl, "plan", "gemini-cli");
    crate::setup::put(tbl, "model", model.as_str());
    crate::setup::ask_default(t, &cfg, &name, &model)?;
    t.save()?;
    ok(format!("saved `{name}` · {model} on the Gemini plan"));
    Ok(true)
}
