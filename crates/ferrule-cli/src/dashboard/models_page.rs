//! M37's model pickers (docs/m37-control-room.md §2): what a select may
//! name, the fallback list checked before it's written, a provider's key
//! tested before it's saved (write-only: it goes to the secrets file and
//! never comes back), its model list, and the two plan sign-ins.

use super::api::{arg, bad, config, missing, need, ok, retire, setup_place, Answer};
use super::Ctx;
use crate::config::{Config, Plan};
use crate::models::Catalog;
use crate::probe;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What every select naming a model offers: the connected models (ready or
/// not, and why not), the aliases, and the providers that can be
/// connected with a key or a plan.
pub(super) fn choices(ctx: &Ctx) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let view = m.view();
    let cat = m.catalog();
    let models: Vec<Value> = view
        .models
        .iter()
        .map(|r| {
            let label = if r.aliases.is_empty() {
                r.reference.clone()
            } else {
                format!("{} ({})", r.reference, r.aliases.join(", "))
            };
            json!({
                "reference": r.reference,
                "label": label,
                "provider": r.provider,
                "ready": r.key_present,
                "missing": r.missing,
                "default": r.default,
                "fallback_rank": r.fallback_rank,
                "plan": r.plan,
            })
        })
        .collect();
    let place = setup_place(ctx).ok();
    let cfg = config(ctx);
    let mut providers: Vec<Value> = Vec::new();
    if let Some(cfg) = &cfg {
        for (name, p) in &cfg.providers {
            let ready = view
                .models
                .iter()
                .any(|r| &r.provider == name && r.key_present);
            let preset = crate::setup::presets()
                .iter()
                .find(|x| x.base_url == p.base_url);
            providers.push(json!({
                "name": name,
                "title": preset.map_or(name.as_str(), |x| x.label),
                "connected": true,
                "ready": ready,
                "plan": p.plan.map(|p| p.as_str()),
                "needs_key": p.plan.is_none() && preset.is_none_or(|x| !x.key_url.is_empty()),
                "key_url": preset.map(|x| x.key_url).filter(|u| !u.is_empty()),
            }));
        }
    }
    for x in crate::setup::presets() {
        let taken = cfg
            .as_ref()
            .is_some_and(|c| c.providers.contains_key(x.name));
        if taken {
            continue;
        }
        providers.push(json!({
            "name": x.name,
            "title": x.label,
            "connected": false,
            "ready": false,
            "plan": null,
            "needs_key": !x.key_url.is_empty(),
            "key_set": place.as_ref().is_some_and(|p| p.get(x.key_env).is_some()),
            "key_url": Some(x.key_url).filter(|u| !u.is_empty()),
        }));
    }
    for plan in [Plan::Chatgpt, Plan::ClaudeCode] {
        let on = cfg
            .as_ref()
            .is_some_and(|c| c.providers.values().any(|p| p.plan == Some(plan)));
        if !on {
            providers.push(json!({
                "name": plan.as_str(),
                "title": plan_title(plan),
                "connected": false,
                "ready": false,
                "plan": plan.as_str(),
                "needs_key": false,
            }));
        }
    }
    ok(json!({
        "models": models,
        "default": view.default,
        "fallback": view.fallback,
        "aliases": cat.aliases,
        "providers": providers,
    }))
}

fn plan_title(plan: Plan) -> &'static str {
    match plan {
        Plan::Chatgpt => "ChatGPT plan (Plus, Pro, Business)",
        Plan::ClaudeCode => "Claude plan (Pro, Max)",
    }
}

/// The fallback list as the page sends it, refused when it names a model
/// that isn't connected, one twice, or the default itself (the list is for
/// when the default is down). `Ok`: the references, in order.
pub(super) fn check_fallback(cat: &Catalog, words: &[String]) -> Result<Vec<String>, String> {
    let default = cat.default_entry().ok().map(|(e, _)| e.reference());
    let mut seen: Vec<String> = Vec::new();
    for w in words {
        let r = cat
            .resolve(w)
            .map_err(|e| format!("`{w}` can't be a fallback: {e}"))?
            .reference();
        if seen.contains(&r) {
            return Err(format!("{r} is in the list twice; each model goes in once"));
        }
        if default.as_deref() == Some(r.as_str()) {
            return Err(format!(
                "{r} is the default: the fallback list is what turns move to when it's down, so pick other models"
            ));
        }
        seen.push(r);
    }
    Ok(seen)
}

/// Where a provider's key goes and what it's checked against.
struct Wanted {
    name: String,
    title: String,
    base_url: String,
    key_env: String,
    profile: String,
    /// The model a new provider starts on ("" = the first listed).
    model: String,
    needs_key: bool,
    key_url: String,
    plan: Option<Plan>,
    /// Not in the config yet.
    new: bool,
}

fn wanted(cfg: &Config, name: &str, base_url: Option<&str>) -> Result<Wanted, String> {
    let base_url = match base_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => {
            let parsed = url::Url::parse(u)
                .map_err(|_| format!("`{u}` isn't an address (it starts with https://)"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("`{u}` isn't an http(s) address"));
            }
            Some(u.trim_end_matches('/').to_string())
        }
        None => None,
    };
    if let Some(p) = cfg.providers.get(name) {
        let preset = crate::setup::presets()
            .iter()
            .find(|x| x.base_url == p.base_url);
        return Ok(Wanted {
            name: name.into(),
            title: preset.map_or(name.to_string(), |x| x.label.to_string()),
            base_url: base_url.unwrap_or_else(|| p.base_url.clone()),
            key_env: p.api_key_env.clone(),
            profile: p.profile.clone(),
            model: p.model.clone(),
            needs_key: p.plan.is_none() && preset.is_none_or(|x| !x.key_url.is_empty()),
            key_url: preset.map(|x| x.key_url.to_string()).unwrap_or_default(),
            plan: p.plan,
            new: false,
        });
    }
    if let Some(x) = crate::setup::preset(name) {
        return Ok(Wanted {
            name: name.into(),
            title: x.label.into(),
            base_url: base_url.unwrap_or_else(|| x.base_url.into()),
            key_env: x.key_env.into(),
            profile: x.profile.into(),
            model: x.model.into(),
            needs_key: !x.key_url.is_empty(),
            key_url: x.key_url.into(),
            plan: None,
            new: true,
        });
    }
    if let Some(plan) = [Plan::Chatgpt, Plan::ClaudeCode]
        .into_iter()
        .find(|p| p.as_str() == name)
    {
        return Err(format!(
            "the {} signs in, it takes no key: use its Sign in button",
            plan_title(plan)
        ));
    }
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        return Err(
            "a provider's name is lowercase letters, digits, - and _ (like `local`)".into(),
        );
    }
    let Some(base_url) = base_url else {
        return Err(format!(
            "`{name}` isn't a provider ferrule knows: give its address (an OpenAI-compatible server's, ending in /v1)"
        ));
    };
    Ok(Wanted {
        name: name.into(),
        title: name.into(),
        base_url,
        key_env: format!("{}_API_KEY", name.to_uppercase().replace('-', "_")),
        profile: "generic".into(),
        model: String::new(),
        needs_key: false,
        key_url: String::new(),
        plan: None,
        new: true,
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

fn key_help(w: &Wanted) -> String {
    if w.key_url.is_empty() {
        String::new()
    } else {
        format!(" Make a new one at {}.", w.key_url)
    }
}

/// `{provider, key?, base_url?}`: the key is tested against the provider's
/// model list first; only a key that works is written, to the secrets file
/// alone. A provider that isn't in the config yet is added (its address,
/// the key's name, a model), never the key itself.
pub(super) async fn provider_save(ctx: &Ctx, body: &Value) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let place = need!(setup_place(ctx));
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let name = need!(arg(body, "provider")).to_lowercase();
    let w = match wanted(&cfg, &name, body.get("base_url").and_then(Value::as_str)) {
        Ok(w) => w,
        Err(e) => return bad(400, e),
    };
    if let Some(why) =
        crate::managed::kind_refusal(&w.name, &crate::managed::kind_of(w.plan, &w.base_url))
    {
        return bad(403, why);
    }
    if w.plan.is_some() {
        return bad(400, format!("`{}` is on a plan, it takes no key", w.name));
    }
    let key = body
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty());
    if key.is_some_and(|k| k.chars().any(char::is_whitespace)) {
        return bad(
            400,
            "the key has spaces or line breaks in it; paste it as one line. Nothing was saved.",
        );
    }
    let stored = place.get(&w.key_env);
    if key.is_none() && w.needs_key && stored.is_none() {
        return bad(400, format!("paste {}'s API key.{}", w.title, key_help(&w)));
    }
    let probe_key = key
        .map(str::to_string)
        .or(stored)
        .unwrap_or_else(|| "none".into());
    let listed = match probe::models(&http(), &w.base_url, &probe_key, w.profile == "anthropic")
        .await
    {
        Ok(list) => list,
        Err(probe::Check::Rejected(_)) => {
            return bad(
                400,
                format!(
                    "{} refused the key, so nothing was saved: it's mistyped, revoked or for another account.{}",
                    w.title,
                    key_help(&w)
                ),
            )
        }
        Err(e) => {
            return bad(
                400,
                format!(
                    "the key couldn't be checked ({e}), so nothing was saved. Is {} reachable from this machine?",
                    w.base_url
                ),
            )
        }
    };
    if let Some(k) = key {
        if let Err(e) = crate::secrets::set(&place.secrets, &w.key_env, k) {
            return bad(500, format!("the secrets file couldn't be written: {e:#}"));
        }
        crate::models::remember_key(&w.key_env, k);
    }
    let base_changed = cfg
        .providers
        .get(&w.name)
        .is_some_and(|p| p.base_url != w.base_url);
    let mut added = None;
    if w.new || base_changed {
        let model = if w.new {
            match (w.model.as_str(), listed.first()) {
                (m, _) if !m.is_empty() && (listed.is_empty() || listed.iter().any(|l| l == m)) => {
                    m.to_string()
                }
                (_, Some(first)) => first.clone(),
                (m, None) if !m.is_empty() => m.to_string(),
                _ => {
                    return bad(
                        400,
                        format!(
                            "{} lists no models, so there's nothing to connect. The key was {}.",
                            w.title,
                            if key.is_some() {
                                "saved"
                            } else {
                                "not changed"
                            }
                        ),
                    )
                }
            }
        } else {
            w.model.clone()
        };
        let written = (|| -> anyhow::Result<()> {
            let mut t = crate::setup::Target::load(place.config.clone())?;
            let p = crate::setup::table(t.root(), &["providers", &w.name])?;
            crate::setup::put(p, "base_url", w.base_url.as_str());
            if w.new {
                crate::setup::put(p, "api_key_env", w.key_env.as_str());
                crate::setup::put(p, "model", model.as_str());
                crate::setup::put(p, "profile", w.profile.as_str());
                let api = ferrule_providers::infer_api(&w.base_url);
                if api != ferrule_providers::Api::Chat {
                    crate::setup::put(p, "api", api.to_string().as_str());
                }
            }
            t.save()
        })();
        if let Err(e) = written {
            return bad(
                500,
                format!("the key works but the config couldn't be written: {e:#}"),
            );
        }
        if w.new {
            added = Some(format!("{}/{model}", w.name));
        }
    }
    retire(ctx, None);
    let mut said = match key {
        Some(_) => format!(
            "{}'s key works ({} models) and is saved in the secrets file; it never goes in the config.",
            w.title,
            listed.len()
        ),
        None => format!("{} answers ({} models).", w.title, listed.len()),
    };
    if let Some(r) = &added {
        said.push_str(&format!(
            " {r} is connected: make it the default or put it in the fallback list."
        ));
    }
    ok(json!({
        "ok": true,
        "said": said,
        "provider": w.name,
        "added": added,
        "models": listed,
        "view": m.view(),
    }))
}

/// `?provider=`: the model ids a connected provider (or a preset with a
/// saved key) offers, and which are connected already.
pub(super) async fn provider_list(ctx: &Ctx, req: &super::http::Request) -> Answer {
    let place = need!(setup_place(ctx));
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    let name = match req.query.get("provider").map(|p| p.trim()) {
        Some(p) if !p.is_empty() => p.to_lowercase(),
        _ => return bad(400, "`provider` is missing"),
    };
    let connected: Vec<String> = cfg
        .providers
        .get(&name)
        .map(|p| {
            std::iter::once(p.model.clone())
                .chain(p.models.keys().cloned())
                .collect()
        })
        .unwrap_or_default();
    if let Some(p) = cfg.providers.get(&name).filter(|p| p.plan.is_some()) {
        let models = match p.plan {
            Some(Plan::Chatgpt) => {
                let issuer = cfg.plans.chatgpt.issuer.clone().unwrap_or_default();
                crate::subscription::chatgpt_models(&issuer).await
            }
            _ => crate::subscription::CLAUDE_CODE_MODELS
                .iter()
                .map(|m| m.to_string())
                .collect(),
        };
        return ok(json!({ "provider": name, "models": models, "connected": connected }));
    }
    let w = match wanted(&cfg, &name, None) {
        Ok(w) => w,
        Err(e) => return bad(400, e),
    };
    let key = match place.get(&w.key_env) {
        Some(k) => k,
        None if !w.needs_key => "none".into(),
        None => {
            return bad(
                400,
                format!("{} has no key saved yet: paste one first.", w.title),
            )
        }
    };
    match probe::models(&http(), &w.base_url, &key, w.profile == "anthropic").await {
        Ok(models) => ok(json!({ "provider": name, "models": models, "connected": connected })),
        Err(probe::Check::Rejected(_)) => bad(
            400,
            format!(
                "{} refused the saved key: paste a new one.{}",
                w.title,
                key_help(&w)
            ),
        ),
        Err(e) => bad(
            400,
            format!("{}'s model list couldn't be read: {e}", w.title),
        ),
    }
}

// ---- Plans -------------------------------------------------------------

/// A ChatGPT device sign-in started from the page: the code the owner
/// types at OpenAI, and what came of it.
struct Flow {
    page: String,
    user_code: String,
    started: Instant,
    outcome: Arc<Mutex<Option<Result<String, String>>>>,
    task: tokio::task::AbortHandle,
}

/// The page's plan sign-ins, one ChatGPT flow at a time.
#[derive(Default)]
pub struct PlanFlows {
    chatgpt: Mutex<Option<Flow>>,
}

/// A device code is good for this long (OpenAI's 15 minutes).
const DEVICE_LIMIT: Duration = Duration::from_secs(15 * 60);

fn private_and_data(ctx: &Ctx) -> Result<(std::path::PathBuf, std::path::PathBuf), Answer> {
    match &ctx.data {
        Some(d) => Ok((d.join("private"), d.clone())),
        None => Err(missing("the data directory")),
    }
}

/// `POST plans/chatgpt/start`: a device code to type at OpenAI's page
/// (the same one while it's still waiting); the sign-in finishes on its
/// own and `poll` says how it went.
pub(super) async fn chatgpt_start(ctx: &Ctx) -> Answer {
    let (private, data) = need!(private_and_data(ctx));
    let issuer = config(ctx)
        .and_then(|c| c.plans.chatgpt.issuer)
        .unwrap_or_default();
    {
        let flow = ctx.plans.chatgpt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = flow.as_ref() {
            let waiting = f.outcome.lock().unwrap().is_none();
            if waiting && f.started.elapsed() < DEVICE_LIMIT {
                return ok(json!({ "page": f.page, "user_code": f.user_code, "state": "waiting" }));
            }
        }
    }
    let plan = crate::subscription::chatgpt_at(&private, &data, &issuer);
    let code = match ferrule_plans::chatgpt::auth::start_device(plan.http(), plan.issuer()).await {
        Ok(Some(code)) => code,
        Ok(None) => {
            return bad(
                400,
                "Device sign-in is off for this ChatGPT account. Turn it on in ChatGPT \
                 (Settings → Security → device code sign-in) and press Sign in again.",
            )
        }
        Err(_) => {
            return bad(
                502,
                "OpenAI's sign-in didn't answer from this machine; try again in a minute.",
            )
        }
    };
    let outcome: Arc<Mutex<Option<Result<String, String>>>> = Arc::default();
    let (page, user_code) = (code.page.clone(), code.user_code.clone());
    let config_path = ctx.config_path.clone();
    let retire_all = ctx.live.as_ref().map(|l| l.retire.clone());
    let task = tokio::spawn({
        let outcome = outcome.clone();
        async move {
            let said = match ferrule_plans::chatgpt::auth::finish_device(
                plan.http(),
                plan.issuer(),
                &code,
            )
            .await
            {
                Ok(tokens) => match plan.sign_in(tokens).await {
                    Ok(meta) => {
                        let who = crate::subscription::SignIn::In {
                            email: meta.email,
                            plan: meta.plan,
                        }
                        .word();
                        let added = config_path.as_deref().and_then(|p| {
                            crate::subscription::login::add_provider_at(
                                p,
                                Plan::Chatgpt,
                                crate::subscription::login::CHATGPT_MODEL,
                            )
                            .ok()
                            .flatten()
                        });
                        if let Some(r) = retire_all {
                            r(None);
                        }
                        Ok(match added {
                            Some(_) => format!(
                                "{who} on the ChatGPT plan; `chatgpt/{}` is connected.",
                                crate::subscription::login::CHATGPT_MODEL
                            ),
                            None => format!("{who} on the ChatGPT plan."),
                        })
                    }
                    Err(e) => Err(format!(
                        "The sign-in came back but couldn't be stored: {e:#}"
                    )),
                },
                Err(e) => Err(format!("The ChatGPT sign-in didn't complete: {e:#}.")),
            };
            *outcome.lock().unwrap() = Some(said);
        }
    })
    .abort_handle();
    let mut flow = ctx.plans.chatgpt.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = flow.take() {
        old.task.abort();
    }
    *flow = Some(Flow {
        page: page.clone(),
        user_code: user_code.clone(),
        started: Instant::now(),
        outcome,
        task,
    });
    ok(json!({ "page": page, "user_code": user_code, "state": "waiting" }))
}

/// `GET plans/chatgpt/poll`: `none`, `waiting` (with the code), `done` or
/// `failed` (with a sentence).
pub(super) fn chatgpt_poll(ctx: &Ctx) -> Answer {
    let flow = ctx.plans.chatgpt.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = flow.as_ref() else {
        return ok(json!({ "state": "none" }));
    };
    let outcome = f.outcome.lock().unwrap().clone();
    ok(match outcome {
        None if f.started.elapsed() >= DEVICE_LIMIT => json!({
            "state": "failed",
            "said": "The code expired (15 minutes); press Sign in for a new one.",
        }),
        None => json!({ "state": "waiting", "page": f.page, "user_code": f.user_code }),
        Some(Ok(said)) => json!({ "state": "done", "said": said }),
        Some(Err(said)) => json!({ "state": "failed", "said": said }),
    })
}

/// `POST plans/chatgpt/cancel`: the waiting sign-in stops.
pub(super) fn chatgpt_cancel(ctx: &Ctx) -> Answer {
    let mut flow = ctx.plans.chatgpt.lock().unwrap_or_else(|e| e.into_inner());
    let had = flow.take().inspect(|f| f.task.abort()).is_some();
    ok(json!({ "ok": had }))
}

/// `POST plans/claude {token}`: a setup-token from `claude setup-token`,
/// checked for its shape and stored sealed; it never comes back, and only
/// the claude process ever gets it.
pub(super) fn claude_token(ctx: &Ctx, body: &Value) -> Answer {
    let (private, _) = need!(private_and_data(ctx));
    let token = need!(arg(body, "token"));
    if let Err(e) = crate::subscription::claude::save_setup_token(&private, token) {
        return bad(400, format!("{e:#}. Nothing was saved."));
    }
    let added = ctx.config_path.as_deref().and_then(|p| {
        crate::subscription::login::add_provider_at(
            p,
            Plan::ClaudeCode,
            crate::subscription::claude::MODEL,
        )
        .ok()
        .flatten()
    });
    retire(ctx, None);
    let mut said = "The setup-token is saved, sealed; only the claude process gets it. It lasts a \
                    year and doctor warns 30 days before."
        .to_string();
    if let Some(line) = added {
        said.push(' ');
        said.push_str(&line);
    }
    ok(json!({ "ok": true, "said": said }))
}
