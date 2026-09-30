//! M21: several models at once, a default, and a model per agent
//! (docs/m21-models.md). The catalog of connected models is read from the
//! config; every agent gets a [`RoutedProvider`] that picks its model per
//! call from the agent's [`Scope`], so a change reaches a running lane at
//! its next call.

use crate::config::{Config, Plan, ProviderConfig};
use crate::ledger::{Prices, ProviderPricing};
use chrono::{DateTime, Utc};
use ferrule_core::provider::{CompletionRequest, CompletionResponse, Provider};
use ferrule_core::{CoreError, Escalation, FailOver, HarnessProfile, RouteTag, Served, Signal};
use ferrule_providers::{Api, DriverOptions};
use ferrule_trust::Hub;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

mod admin;
pub mod catalog;
mod cli;
#[cfg(test)]
mod cross_driver;
mod door;
pub mod routing;
pub mod routing_admin;
pub use admin::*;
pub use cli::{cmd, render, ModelCmd};
pub use door::{status_lines, ModelDoor, Retire};

/// How long a model that stayed down after its retries is skipped for.
pub const DOWN_FOR: Duration = Duration::from_secs(5 * 60);

/// One connected model: a provider's own `model` (its primary) or one
/// under `[providers.X.models]`, with the provider's fields filled in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub provider: String,
    pub model: String,
    pub primary: bool,
    pub base_url: String,
    pub key_env: String,
    pub profile: String,
    pub context_window: Option<usize>,
    pub pricing: Option<ProviderPricing>,
    /// The model's `price_source`: who wrote its prices, if not by hand.
    pub price_source: Option<String>,
    pub aliases: Vec<String>,
    /// M23: the driver, and whether the config names it (else inferred).
    pub api: Api,
    pub api_set: bool,
    #[serde(skip)]
    pub options: DriverOptions,
    /// M35: the subscription it signs in with; `None` is a keyed model.
    pub plan: Option<Plan>,
    /// M35: `[plans.chatgpt] issuer`.
    #[serde(skip)]
    pub issuer: String,
}

/// The window ferrule assumes for a model run through Claude Code.
pub const CLAUDE_CODE_WINDOW: usize = 1_000_000;

impl Entry {
    /// Whether this model sees photos: the config's `vision`, else its name.
    /// A model run through Claude Code never does (its engine is text).
    pub fn sees_images(&self) -> bool {
        self.plan != Some(crate::config::Plan::ClaudeCode)
            && self
                .options
                .vision
                .unwrap_or_else(|| ferrule_providers::vision::by_name(&self.model))
    }

    /// A driver for this model with `key`.
    pub fn client(&self, key: impl Into<String>) -> Arc<dyn Provider> {
        if let Some(plan) = self.plan {
            return crate::subscription::client(
                plan,
                &self.provider,
                &self.base_url,
                &self.model,
                self.options.clone(),
                &self.issuer,
            );
        }
        ferrule_providers::build(
            self.api,
            self.provider.clone(),
            &self.base_url,
            key,
            &self.model,
            self.options.clone(),
        )
    }

    /// `anthropic (inferred)`, `responses (set)`: for doctor and the page.
    pub fn driver(&self) -> String {
        let how = if self.api_set { "set" } else { "inferred" };
        format!("{} ({how})", self.api)
    }

    /// `provider/model`.
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// The harness profile, with the model's own window if it has one.
    pub fn harness(&self) -> HarnessProfile {
        let p = HarnessProfile::by_name(&self.profile);
        match self.context_window {
            Some(w) => p.fitted(w),
            None => p,
        }
    }

    /// The key from the env, or one saved from the page since the process
    /// started (M37). A plan has none and needs none (`""`): its
    /// credential is read per call from its own store.
    pub fn key(&self) -> Option<String> {
        if self.plan.is_some() {
            return Some(String::new());
        }
        std::env::var(&self.key_env)
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| saved_key(&self.key_env))
    }

    /// A call has what it needs: the key is set, or the plan is signed in.
    pub fn ready(&self) -> bool {
        match self.plan {
            Some(plan) => matches!(
                crate::subscription::state(plan),
                crate::subscription::SignIn::In { .. }
            ),
            None => self.key().is_some_and(|k| !k.is_empty()),
        }
    }

    /// What a call is missing, short: "key missing ($X)" or the plan's
    /// "not signed in — `ferrule login chatgpt`".
    pub fn missing(&self) -> String {
        match self.plan {
            Some(plan) => crate::subscription::sign_in_word(plan),
            None => format!("key missing (${})", self.key_env),
        }
    }

    /// Why a call can't be made: the key isn't in the env or the secrets
    /// file, or the plan isn't signed in.
    pub fn no_key(&self) -> String {
        if let Some(plan) = self.plan {
            return format!(
                "{} ({})",
                crate::subscription::not_signed_in(plan),
                self.reference()
            );
        }
        format!(
            "no key: `${}` isn't set ({}); run `ferrule setup`, or export it",
            self.key_env,
            self.reference()
        )
    }
}

/// Every connected model, the aliases, the default and the fallback list,
/// as the config says now.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
    pub aliases: BTreeMap<String, String>,
    pub default: Option<String>,
    pub default_provider: Option<String>,
    pub fallback: Vec<String>,
    /// `[models] deny`, as written.
    pub deny: Vec<String>,
    /// `[models.exact]`, as written: the ref, and the id it must be.
    pub exact: BTreeMap<String, String>,
    /// `exact` resolved: a connected model's reference → the id it must be.
    pinned: BTreeMap<String, String>,
    /// M25: `[routing]`, its tiers resolved.
    pub routing: routing::Routing,
    /// M44: providers the panel's policy forbids, with its reason.
    pub managed: BTreeMap<String, String>,
}

impl Catalog {
    pub fn from_config(cfg: &Config) -> Self {
        let mut entries = Vec::new();
        for (name, p) in &cfg.providers {
            let own = p.models.get(&p.model).cloned().unwrap_or_default();
            entries.push(entry(name, p, &p.model, true, &own));
            for (model, mc) in &p.models {
                if *model != p.model {
                    entries.push(entry(name, p, model, false, mc));
                }
            }
        }
        let mut cat = Self {
            entries,
            aliases: cfg.models.aliases.clone(),
            default: cfg.models.default.clone(),
            default_provider: cfg.default_provider.clone(),
            fallback: cfg.models.fallback.clone(),
            deny: cfg.models.deny.clone(),
            exact: cfg.models.exact.clone(),
            pinned: BTreeMap::new(),
            routing: routing::Routing::default(),
            managed: crate::managed::blocked_providers(cfg),
        };
        cat.pinned = cat
            .exact
            .iter()
            .filter_map(|(word, id)| {
                let id = id.trim();
                if id.is_empty() {
                    return None;
                }
                let reference = cat.resolve_unchecked(word).ok()?.reference();
                Some((reference, id.to_string()))
            })
            .collect();
        let named: Vec<(String, String)> = cat
            .aliases
            .keys()
            .filter_map(|a| Some((a.clone(), cat.resolve_unchecked(a).ok()?.reference())))
            .collect();
        for (alias, target) in named {
            if let Some(e) = cat.entries.iter_mut().find(|e| e.reference() == target) {
                e.aliases.push(alias);
            }
        }
        cat.routing = routing::Routing::from_config(&cfg.routing, &cat);
        cat
    }

    /// The connected model `word` names (docs/m21-models.md §1): an alias,
    /// then a provider (its primary), then `provider/model`, then a model
    /// id only one provider has. M25: a tier ref (`tier:strong`) is that
    /// tier's model. A model `[models]` denies, or whose `exact` pin the
    /// connected id doesn't match, is refused with the reason.
    pub fn resolve(&self, word: &str) -> Result<&Entry, String> {
        let e = self.resolve_unchecked(word)?;
        if let Some(why) = self.forbidden(e) {
            return Err(format!("`{}` is {why}", e.reference()));
        }
        Ok(e)
    }

    /// [`resolve`] without the `[models] deny`/`exact` check: for listing
    /// what's connected, and for changing or removing a denied model.
    pub fn resolve_unchecked(&self, word: &str) -> Result<&Entry, String> {
        let word = word.trim();
        if let Some(tier) = self.routing.tier_of(word) {
            return tier.map(|i| &self.routing.tiers[i].entry);
        }
        if let Some(target) = self.aliases.get(word) {
            return self
                .resolve_plain(target)
                .map_err(|e| format!("the alias `{word}` points at `{target}`, but {e}"));
        }
        self.resolve_plain(word)
    }

    /// Why `e` may not run, when `[models]` forbids it: it matches a `deny`
    /// word (its `provider/model`, its bare model id, its provider, or an
    /// alias for it), or an `exact` pin names it but the connected id
    /// isn't the pinned one. Phrased to follow "`ref` is …".
    pub fn forbidden(&self, e: &Entry) -> Option<String> {
        self.denied(e).or_else(|| self.mispinned(e))
    }

    /// Why `e` is denied, when a `[models] deny` word names it (its
    /// `provider/model`, its bare model id, its provider, or an alias for
    /// it). Phrased to follow "`ref` is …".
    pub fn denied(&self, e: &Entry) -> Option<String> {
        if let Some(why) = self.managed.get(&e.provider) {
            return Some(format!("not allowed on this bot: {why}"));
        }
        let reference = e.reference();
        for w in &self.deny {
            let w = w.trim();
            if w.is_empty() {
                continue;
            }
            let hit = w == reference
                || w == e.model
                || w == e.provider
                || self
                    .aliases
                    .get(w)
                    .and_then(|t| self.resolve_plain(t).ok())
                    .is_some_and(|t| t.reference() == reference);
            if hit {
                return Some(format!("denied by [models] deny (`{w}`)"));
            }
        }
        None
    }

    /// Why `e` is mis-pinned, when `[models.exact]` names it but the
    /// connected id isn't the pinned one. Phrased to follow "`ref` is …".
    fn mispinned(&self, e: &Entry) -> Option<String> {
        let id = self.exact_pin(e)?;
        (e.model != id).then(|| {
            format!(
                "pinned to `{id}` by [models] exact, but `{}` is connected; connect the pinned id and use it instead",
                e.model
            )
        })
    }

    /// The exact id `[models] exact` pins `e` to, when it does.
    pub fn exact_pin(&self, e: &Entry) -> Option<&str> {
        self.pinned.get(&e.reference()).map(String::as_str)
    }

    fn resolve_plain(&self, word: &str) -> Result<&Entry, String> {
        if word.is_empty() {
            return Err("no model was named".into());
        }
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| e.primary && e.provider == word)
        {
            return Ok(e);
        }
        if let Some((p, m)) = word.split_once('/') {
            if self.entries.iter().any(|e| e.provider == p) {
                return self
                    .entries
                    .iter()
                    .find(|e| e.provider == p && e.model == m)
                    .ok_or_else(|| {
                        let theirs: Vec<&str> = self
                            .entries
                            .iter()
                            .filter(|e| e.provider == p)
                            .map(|e| e.model.as_str())
                            .collect();
                        format!(
                            "`{m}` isn't connected on `{p}` (connected there: {}); `ferrule model add {p}/{m}` connects it",
                            theirs.join(", ")
                        )
                    });
            }
        }
        let hits: Vec<&Entry> = self.entries.iter().filter(|e| e.model == word).collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(format!(
                "`{word}` isn't a connected model; `ferrule model list` shows them"
            )),
            many => Err(format!(
                "`{word}` is connected on more than one provider ({}); say which",
                many.iter()
                    .map(|e| e.reference())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// `[models] default`, else `default_provider`'s primary, and a note
    /// when the first names a model that isn't connected.
    pub fn default_entry(&self) -> Result<(&Entry, Option<String>), String> {
        let mut note = None;
        if let Some(d) = &self.default {
            match self.resolve(d) {
                Ok(e) => return Ok((e, None)),
                Err(e) => {
                    note = Some(format!(
                        "[models] default = \"{d}\": {e}; using default_provider instead"
                    ))
                }
            }
        }
        match &self.default_provider {
            Some(p) => self
                .entries
                .iter()
                .find(|e| e.primary && e.provider == *p)
                .map(|e| (e, note))
                .ok_or_else(|| format!("provider `{p}` not in config")),
            None => {
                if self.entries.is_empty() {
                    // Nothing is connected at all: `model default` can't
                    // help, setup can.
                    Err("no model is connected yet — run `ferrule setup` to add one".into())
                } else {
                    Err(
                        "no default model: run `ferrule model default <ref>`, or set default_provider"
                            .into(),
                    )
                }
            }
        }
    }

    /// The price of a call that ran on `provider`'s `model`: the model's
    /// own, else its provider's (a model the eval named by hand).
    pub fn price(&self, provider: &str, model: &str) -> Option<ProviderPricing> {
        self.entries
            .iter()
            .find(|e| e.provider == provider && e.model == model)
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.primary && e.provider == provider)
            })
            .and_then(|e| e.pricing)
    }

    /// The plan `provider` signs in with (M35), `None` for a keyed one.
    pub fn plan_of(&self, provider: &str) -> Option<Plan> {
        self.entries
            .iter()
            .find(|e| e.provider == provider)
            .and_then(|e| e.plan)
    }
}

fn entry(
    name: &str,
    p: &ProviderConfig,
    model: &str,
    primary: bool,
    mc: &crate::config::ModelConfig,
) -> Entry {
    // Field by field, the model's own, else the provider's; and then all
    // three prices or none (M19's rule).
    let pricing = (|| {
        let input = mc.price_input_per_mtok.or(p.price_input_per_mtok)?;
        Some(ProviderPricing {
            input,
            cached_input: mc
                .price_cached_input_per_mtok
                .or(p.price_cached_input_per_mtok)?,
            output: mc.price_output_per_mtok.or(p.price_output_per_mtok)?,
            cache_write: crate::ledger::write_price(
                mc.price_cache_write_per_mtok
                    .or(p.price_cache_write_per_mtok),
                p.api(),
                input,
            ),
        })
    })();
    Entry {
        provider: name.to_string(),
        model: model.to_string(),
        primary,
        base_url: p.endpoint(),
        key_env: p.api_key_env.clone(),
        profile: mc.profile.clone().unwrap_or_else(|| p.profile.clone()),
        // M35: claude keeps (and compacts) the conversation itself; ferrule's
        // compaction would drop the session it resumes, so it waits long.
        context_window: mc
            .context_window
            .or((p.plan == Some(crate::config::Plan::ClaudeCode)).then_some(CLAUDE_CODE_WINDOW)),
        pricing,
        price_source: mc.price_source.clone(),
        aliases: Vec::new(),
        api: p.api(),
        api_set: p.api.is_some(),
        options: p.driver_options(model),
        plan: p.plan,
        issuer: p.issuer.clone(),
    }
}

/// Who an agent is, for picking its model: what was asked for it
/// explicitly, and the task or chat it runs for (docs/m21-models.md §3).
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Its session: the key `/status` shows the last model under.
    pub session: String,
    /// A one-off (`--model`, `--provider`, `spawn_agent(model)`) or its
    /// role's model: it runs on that or fails with the reason.
    pub fixed: Option<Fixed>,
    /// The scheduled task it runs for.
    pub task: Option<String>,
    /// The chat it answers, `(channel, chat)`.
    pub chat: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Fixed {
    pub word: String,
    /// Who asked, for the error: "--model", "role verifier".
    pub by: String,
}

impl Scope {
    /// A gateway session `<channel>__<chat>`, or a scheduler one: the chat
    /// or the task. Any other id is a session of its own.
    pub fn for_session(session: &str) -> Self {
        let mut s = Self {
            session: session.to_string(),
            ..Self::default()
        };
        match session.split_once("__") {
            Some((ch, task)) if ch == ferrule_gateway::SCHEDULER_PSEUDO_CHANNEL => {
                s.task = Some(task.to_string())
            }
            Some((ch, chat)) => s.chat = Some((ch.to_string(), chat.to_string())),
            None => {}
        }
        s
    }

    pub fn fixed(mut self, word: Option<String>, by: &str) -> Self {
        if let Some(word) = word {
            self.fixed = Some(Fixed {
                word,
                by: by.to_string(),
            });
        }
        self
    }
}

/// A task's model, looked up per call so `ferrule tasks model` reaches a
/// running lane.
pub type TaskModels = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A driver is reused while everything it was built from stays the same.
type ClientKey = (String, String, String, String, Api, DriverOptions);

struct Down {
    until: Instant,
    reason: String,
    told: bool,
    /// When it's tried again, in words: "in 5 minutes", or a plan's reset.
    again: String,
}

#[derive(Default)]
struct State {
    catalog: Arc<Catalog>,
    seen: Option<(SystemTime, u64)>,
    broken: bool,
    pins: BTreeMap<String, String>,
    pins_seen: Option<(SystemTime, u64)>,
    down: HashMap<String, Down>,
    served: HashMap<String, (String, DateTime<Utc>)>,
    clients: HashMap<ClientKey, Arc<dyn Provider>>,
    warned: HashSet<String>,
    routing: routing::RoutingState,
}

/// The process's models: the catalog as the config file says now, the chat
/// pins, outages and which model each session ran on last. One per
/// process ([`shared`]); every lane's [`RoutedProvider`] shares it.
pub struct Models {
    path: PathBuf,
    pins_path: Option<PathBuf>,
    state: Mutex<State>,
    hub: Mutex<Option<Arc<Hub>>>,
    tasks: Mutex<Option<TaskModels>>,
}

static SHARED: OnceLock<Arc<Models>> = OnceLock::new();

/// Provider keys saved to the secrets file while the process runs (the
/// dashboard's key form): `set_var` isn't safe once threads run, so they
/// are kept here, in memory only, and read by [`Entry::key`].
static SAVED_KEYS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// A key just written to the secrets file, for this process's next calls.
pub fn remember_key(name: &str, value: &str) {
    SAVED_KEYS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), value.to_string());
}

fn saved_key(name: &str) -> Option<String> {
    SAVED_KEYS
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

/// This process's models, from the config `Config::load` finds.
pub fn shared() -> anyhow::Result<Arc<Models>> {
    if let Some(m) = SHARED.get() {
        return Ok(m.clone());
    }
    let (cfg, path) = Config::load()?;
    let pins = crate::config::data_dir()
        .ok()
        .map(|d| d.join("models").join("pins.json"));
    let m = Arc::new(Models::new(path, pins, &cfg));
    if let Ok(ledger) = crate::ledger::ledger_path() {
        m.set_ledger(ledger);
    }
    Ok(SHARED.get_or_init(|| m).clone())
}

/// What a ledger row costs: the shared catalog's prices when there is one,
/// else `cfg`'s.
pub fn prices(cfg: &Config) -> Prices {
    match shared() {
        Ok(m) => Arc::new(move |p: &str, model: &str| m.catalog().price(p, model)),
        Err(_) => {
            let cat = Catalog::from_config(cfg);
            Arc::new(move |p: &str, model: &str| cat.price(p, model))
        }
    }
}

/// Which plan a ledger row's provider is on (M35), from the same catalog
/// as [`prices`].
pub fn plans(cfg: &Config) -> crate::ledger::PlanOf {
    match shared() {
        Ok(m) => Arc::new(move |p: &str| m.catalog().plan_of(p)),
        Err(_) => {
            let cat = Catalog::from_config(cfg);
            Arc::new(move |p: &str| cat.plan_of(p))
        }
    }
}

impl Models {
    /// `cfg` is what the file at `path` says now; `pins` is the pins file.
    pub fn new(path: PathBuf, pins: Option<PathBuf>, cfg: &Config) -> Self {
        let seen = stamp(&path);
        let me = Self {
            path,
            pins_path: pins,
            state: Mutex::new(State {
                catalog: Arc::new(Catalog::from_config(cfg)),
                seen,
                ..State::default()
            }),
            hub: Mutex::new(None),
            tasks: Mutex::new(None),
        };
        me.refresh_pins(&mut me.state.lock().unwrap());
        me
    }

    /// Where model events are audited, and the owner told of an outage.
    pub fn attach_hub(&self, hub: Arc<Hub>) {
        *self.hub.lock().unwrap() = Some(hub);
    }

    pub fn set_task_models(&self, f: TaskModels) {
        *self.tasks.lock().unwrap() = Some(f);
    }

    /// The catalog, re-read if the file changed since.
    pub fn catalog(&self) -> Arc<Catalog> {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        st.catalog.clone()
    }

    /// The model `scope` asks for, before outages are considered.
    #[cfg(test)]
    pub fn wanted(&self, scope: &Scope) -> Result<Entry, String> {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        self.pick(&mut st, scope)
    }

    /// Whether the model `scope` asks for sees photos (before outages).
    pub fn wanted_sees_images(&self, scope: &Scope) -> bool {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        self.pick(&mut st, scope)
            .map(|e| e.sees_images())
            .unwrap_or(false)
    }

    fn pick(&self, st: &mut State, scope: &Scope) -> Result<Entry, String> {
        self.pick_floor(st, scope).map(|(e, _)| e)
    }

    /// The model this call goes to: the wanted one, or while that's down,
    /// the first fallback that isn't.
    pub fn route(&self, scope: &Scope) -> Result<Route, String> {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        let wanted = self.pick(&mut st, scope)?;
        finish(&mut st, wanted)
    }

    /// A call on `route` came back: an answer clears an outage mark on
    /// that model, and the first answer from a fallback tells the owner.
    fn served(&self, scope: &Scope, route: &Route, ok: bool) {
        let reference = route.entry.reference();
        let mut tell = None;
        let mut up = false;
        {
            let mut st = self.state.lock().unwrap();
            st.served
                .insert(scope.session.clone(), (reference.clone(), Utc::now()));
            if !ok {
                return;
            }
            if st.down.remove(&reference).is_some() {
                up = true;
            }
            if let Some(from) = &route.instead_of {
                if let Some(d) = st.down.get_mut(from) {
                    if !d.told {
                        d.told = true;
                        tell = Some(format!(
                            "{from} isn't answering ({}), so {reference} answered instead. I'll try {from} again {}.",
                            d.reason, d.again
                        ));
                    }
                }
            }
        }
        if up {
            self.audit("model.up", serde_json::json!({ "model": reference }));
        }
        if let Some(text) = tell {
            if let Some(hub) = self.hub.lock().unwrap().clone() {
                hub.tell_owner(text);
            }
        }
    }

    /// M35: a plan's sign-in that expired or was revoked is said to the
    /// owner once (until a call on it works again): the chat that hit it
    /// may be someone else's, or a task nobody reads.
    pub fn plan_signin(&self, entry: &Entry, result: Result<(), &CoreError>) {
        let Some(plan) = entry.plan else { return };
        let key = format!("signin\n{}", entry.provider);
        let tell = {
            let mut st = self.state.lock().unwrap();
            match result {
                Ok(()) => {
                    st.warned.remove(&key);
                    return;
                }
                Err(e) if e.to_string().contains("expired or was revoked") => st.warned.insert(key),
                Err(_) => return,
            }
        };
        if tell {
            if let Some(hub) = self.hub.lock().unwrap().clone() {
                hub.tell_owner(format!(
                    "The {} plan's sign-in for {} expired or was revoked. \
                     Run `ferrule login {}` on the server{}.",
                    plan.title(),
                    entry.provider,
                    plan.login_word(),
                    if plan == Plan::Chatgpt {
                        " (or send /login chatgpt here)"
                    } else {
                        ""
                    }
                ));
            }
        }
    }

    /// `served` stayed down after its retries: mark it, and say which
    /// model the next call goes to. Nothing without a fallback list, or
    /// when there's nowhere else to go.
    #[cfg(test)]
    pub fn fail_over(&self, scope: &Scope, served: &Served, error: &CoreError) -> Option<FailOver> {
        self.fail_over_from(scope, None, served, error)
    }

    /// [`Models::fail_over`] for a call that wanted `wanted` (a routed
    /// tier's model) rather than what `scope` picks.
    fn fail_over_from(
        &self,
        scope: &Scope,
        wanted: Option<Entry>,
        served: &Served,
        error: &CoreError,
    ) -> Option<FailOver> {
        // M36 §6.2: any provider error falls back, not only an outage.
        if !ferrule_core::failure::falls_back(error) || self.catalog().fallback.is_empty() {
            return None;
        }
        let from = served.reference();
        let reason = short_reason(error);
        let (down_for, again) = down_for(error);
        {
            let mut st = self.state.lock().unwrap();
            st.down.insert(
                from.clone(),
                Down {
                    until: Instant::now() + down_for,
                    reason: reason.clone(),
                    told: false,
                    again,
                },
            );
        }
        self.audit(
            "model.down",
            serde_json::json!({ "model": from, "reason": reason }),
        );
        let next = match wanted {
            Some(w) => finish(&mut self.state.lock().unwrap(), w),
            None => self.route(scope),
        }
        .ok()?
        .entry
        .reference();
        let st = self.state.lock().unwrap();
        (next != from && !is_down(&st, &next)).then_some(FailOver { from, to: next })
    }

    /// The model `session` ran its last call on, and when.
    #[cfg(test)]
    pub fn last_served(&self, session: &str) -> Option<(String, DateTime<Utc>)> {
        self.state.lock().unwrap().served.get(session).cloned()
    }

    /// Models marked down, with how long they're skipped for.
    #[cfg(test)]
    pub fn down(&self) -> Vec<(String, Duration, String)> {
        let st = self.state.lock().unwrap();
        let now = Instant::now();
        st.down
            .iter()
            .filter(|(_, d)| d.until > now)
            .map(|(r, d)| (r.clone(), d.until - now, d.reason.clone()))
            .collect()
    }

    fn audit(&self, event: &str, detail: serde_json::Value) {
        if let Some(hub) = self.hub.lock().unwrap().clone() {
            hub.audit().record(Utc::now(), event, None, None, detail);
        }
    }

    /// Re-reads the config when its modification time or length changed.
    /// A file that no longer parses leaves the last good catalog, with one
    /// warning until it parses again.
    fn refresh(&self, st: &mut State) {
        let now = stamp(&self.path);
        if now.is_none() || now == st.seen {
            return;
        }
        st.seen = now;
        let parsed = std::fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|t| toml::from_str::<Config>(&t).map_err(|e| e.to_string()));
        match parsed {
            Ok(cfg) => {
                st.catalog = Arc::new(Catalog::from_config(&cfg));
                st.broken = false;
            }
            Err(e) if !st.broken => {
                st.broken = true;
                tracing::warn!(
                    "models: {} doesn't parse anymore ({e}); keeping the models it had",
                    self.path.display()
                );
            }
            Err(_) => {}
        }
    }

    fn refresh_pins(&self, st: &mut State) {
        let Some(path) = &self.pins_path else {
            return;
        };
        let now = stamp(path);
        if now == st.pins_seen {
            return;
        }
        st.pins_seen = now;
        st.pins = read_pins(path);
    }
}

/// `wanted`, or while it's down the first fallback that isn't, and its
/// driver.
fn finish(st: &mut State, wanted: Entry) -> Result<Route, String> {
    let mut entry = wanted.clone();
    let mut instead_of = None;
    if is_down(st, &wanted.reference()) {
        let cat = st.catalog.clone();
        let next = cat
            .fallback
            .iter()
            .filter_map(|f| cat.resolve(f).ok())
            .find(|e| e.reference() != wanted.reference() && !is_down(st, &e.reference()));
        if let Some(next) = next {
            entry = next.clone();
            instead_of = Some(wanted.reference());
        }
    }
    let key = entry.key().ok_or_else(|| entry.no_key())?;
    let client = st
        .clients
        .entry((
            entry.provider.clone(),
            entry.base_url.clone(),
            key.clone(),
            entry.model.clone(),
            entry.api,
            entry.options.clone(),
        ))
        .or_insert_with(|| entry.client(key))
        .clone();
    Ok(Route {
        entry,
        client,
        instead_of,
    })
}

/// `channel:chat`, the key of a pin.
pub fn pin_key(channel: &str, chat: &str) -> String {
    format!("{channel}:{chat}")
}

/// The pins file: `{"telegram:42": "ref"}`. Missing or unreadable: none.
pub fn read_pins(path: &Path) -> BTreeMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("models: {} doesn't parse ({e}); no pins", path.display());
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn is_down(st: &State, reference: &str) -> bool {
    st.down
        .get(reference)
        .is_some_and(|d| d.until > Instant::now())
}

fn warn_once(st: &mut State, key: &str, text: &str) {
    if st.warned.insert(format!("{key}\n{text}")) {
        tracing::warn!("models: {text}");
    }
}

/// The longest a model is skipped for when its error says to wait (a
/// plan's weekly window resets within a week).
const DOWN_AT_MOST: Duration = Duration::from_secs(8 * 24 * 3600);

/// How long a model that stayed down is skipped, and that in words:
/// [`DOWN_FOR`], or longer when the error says when to come back (M35: a
/// plan's usage limit, until it resets).
fn down_for(error: &CoreError) -> (Duration, String) {
    use ferrule_core::failure;
    let kind = failure::classify(error);
    let wait = kind
        .down_for(failure::retry_after(error))
        .or(failure::retry_after(error))
        .unwrap_or(DOWN_FOR)
        .max(DOWN_FOR)
        .min(DOWN_AT_MOST);
    if wait > DOWN_FOR && failure::retry_after(error).is_some_and(|w| w >= wait) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let at = ferrule_providers::codex::describe_reset(now + wait.as_secs(), now);
        return (wait, format!("when it resets, {at}"));
    }
    let minutes = wait.as_secs() / 60;
    let words = if minutes >= 120 && minutes.is_multiple_of(60) {
        format!("in {} hours", minutes / 60)
    } else {
        format!("in {minutes} minutes")
    };
    (wait, words)
}

/// "HTTP 503 after its retries", "no connection", "the ChatGPT plan's
/// usage limit is reached", or the start of the message.
fn short_reason(error: &CoreError) -> String {
    let msg = match error {
        CoreError::Transient { message, .. } => message.as_str(),
        _ => {
            let kind = ferrule_core::failure::classify(error);
            if kind == ferrule_core::failure::Kind::Unknown {
                return error.to_string().chars().take(120).collect();
            }
            return kind.short_reason().to_string();
        }
    };
    if let Some((_, rest)) = msg.split_once("usage_limit_reached: ") {
        return rest.split(';').next().unwrap_or(rest).trim().to_string();
    }
    if let Some(code) = msg
        .split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 3 && matches!(w.as_bytes()[0], b'4' | b'5'))
    {
        if msg.contains("HTTP") || msg.contains("status") {
            return format!("HTTP {code} after its retries");
        }
    }
    if no_connection(msg) {
        return "no connection, after its retries".into();
    }
    let short: String = msg.chars().take(80).collect();
    format!("{short} after its retries")
}

/// The drivers' words for a call that never got an answer.
fn no_connection(msg: &str) -> bool {
    ["request failed", "could not connect", "request timed out"]
        .iter()
        .any(|p| msg.starts_with(p))
}

/// Where one call goes.
pub struct Route {
    pub entry: Entry,
    pub client: Arc<dyn Provider>,
    /// The model it stands in for, which is down.
    pub instead_of: Option<String>,
}

/// An agent's provider: resolves its scope on every call, so a new default
/// or pin reaches it without a rebuild, and falls over to the fallback
/// list on an outage. M25: when its scope is routed, it keeps the agent's
/// place on the tiers.
pub struct RoutedProvider {
    models: Arc<Models>,
    scope: Scope,
    /// The provider it was built on, for rows of calls that never ran.
    name: String,
    lane: Mutex<routing::Lane>,
}

impl RoutedProvider {
    pub fn new(models: Arc<Models>, scope: Scope, name: String) -> Self {
        Self {
            models,
            scope,
            name,
            lane: Mutex::default(),
        }
    }

    fn lane(&self) -> std::sync::MutexGuard<'_, routing::Lane> {
        self.lane.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait::async_trait]
impl Provider for RoutedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn sees_images(&self) -> bool {
        // Advisory: the model that serves a call decides for itself.
        self.models.wanted_sees_images(&self.scope)
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        self.complete_routed(req).await.1
    }

    async fn complete_routed(
        &self,
        req: CompletionRequest,
    ) -> (Option<Served>, Result<CompletionResponse, CoreError>) {
        let routed = self.models.route_lane(&self.scope, &mut self.lane());
        let (route, level) = match routed {
            Ok(r) => r,
            Err(e) => return (None, Err(CoreError::Provider(e))),
        };
        let served = Served {
            provider: route.entry.provider.clone(),
            model: route.entry.model.clone(),
        };
        let result = route.client.complete(req).await;
        self.models.served(&self.scope, &route, result.is_ok());
        self.models
            .plan_signin(&route.entry, result.as_ref().map(|_| ()));
        if let (Some(level), Ok(r)) = (level, &result) {
            self.models
                .count_spend(level, route.entry.pricing, &r.usage);
        }
        (Some(served), result)
    }

    fn fail_over(&self, served: Option<&Served>, error: &CoreError) -> Option<FailOver> {
        let wanted = self.lane().wanted.clone();
        self.models
            .fail_over_from(&self.scope, wanted, served?, error)
    }

    fn routes(&self) -> bool {
        self.models.routed(&self.scope)
    }

    fn begin_turn(&self) {
        self.lane().turn = true;
    }

    fn escalate(&self, signal: &Signal) -> Option<Escalation> {
        self.models
            .escalate_lane(&self.scope, &mut self.lane(), signal)
    }

    fn route_tag(&self) -> Option<RouteTag> {
        self.lane().tag.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
default_provider = "a"

[models]
fallback = ["b/b-large"]

[models.aliases]
fast = "b/b-small"

[providers.a]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "a-one"
profile = "openai"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 0.5
price_output_per_mtok = 2.0

[providers.a.models."a-two"]
price_output_per_mtok = 8.0
context_window = 1000

[providers.b]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "b-large"

[providers.b.models."b-small"]
profile = "kimi"

[providers.b.models."vendor/shared"]

[providers.c]
base_url = "http://127.0.0.1:3/v1"
api_key_env = "FERRULE_M21_TEST_KEY_THAT_IS_NEVER_SET"
model = "vendor/shared"
"#;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_provider_the_policy_forbids_is_denied_with_the_reason() {
        let mut cat = Catalog::from_config(&cfg(CONFIG));
        cat.managed.insert("b".into(), "beta".into());
        let on = |p: &str| {
            let e = cat.entries.iter().find(|e| e.provider == p).unwrap();
            cat.denied(e)
        };
        assert_eq!(on("b").as_deref(), Some("not allowed on this bot: beta"));
        assert_eq!(on("a"), None);
    }

    fn models(text: &str) -> (tempfile::TempDir, Models) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ferrule.toml");
        std::fs::write(&path, text).unwrap();
        let m = Models::new(path, Some(dir.path().join("pins.json")), &cfg(text));
        (dir, m)
    }

    #[test]
    fn a_word_resolves_alias_then_provider_then_ref_then_unique_id() {
        let cat = Catalog::from_config(&cfg(CONFIG));
        let r = |w: &str| cat.resolve(w).map(Entry::reference);
        assert_eq!(r("a").unwrap(), "a/a-one");
        assert_eq!(r("fast").unwrap(), "b/b-small");
        assert_eq!(r("a/a-two").unwrap(), "a/a-two");
        assert_eq!(r("a-two").unwrap(), "a/a-two");
        // OpenRouter-style ids: only the first `/` splits.
        assert_eq!(r("b/vendor/shared").unwrap(), "b/vendor/shared");
        assert_eq!(r("c/vendor/shared").unwrap(), "c/vendor/shared");
        let e = r("vendor/shared").unwrap_err();
        assert!(
            e.contains("b/vendor/shared") && e.contains("c/vendor/shared"),
            "{e}"
        );
        let e = r("a/gpt-9").unwrap_err();
        assert!(
            e.contains("isn't connected on `a`") && e.contains("a-one, a-two"),
            "{e}"
        );
        assert!(r("nope").unwrap_err().contains("isn't a connected model"));
        assert!(r("").is_err());
        assert_eq!(cat.resolve("b/b-small").unwrap().aliases, vec!["fast"]);
    }

    /// `[models] deny`: a word is a `provider/model`, a bare id (every
    /// provider), a provider, or an alias — and a denied model stays
    /// visible to `resolve_unchecked` (listing, removing).
    #[test]
    fn deny_refuses_by_ref_id_provider_and_alias() {
        let text = r#"
default_provider = "a"

[models]
deny = ["b/b-large", "b-small", "c", "old"]

[models.aliases]
old = "a/a-two"
fast = "a/a-one"

[providers.a]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "a-one"

[providers.a.models."a-two"]

[providers.b]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "b-large"

[providers.b.models."b-small"]

[providers.c]
base_url = "http://127.0.0.1:3/v1"
api_key_env = "PATH"
model = "c-one"
"#;
        let cat = Catalog::from_config(&cfg(text));
        let denied = |w: &str| {
            let e = cat.resolve(w).unwrap_err();
            assert!(e.contains("denied by [models] deny"), "{w}: {e}");
            e
        };
        // By `provider/model`, by bare id, by provider, by an alias for it.
        assert!(denied("b/b-large").contains("`b/b-large`"));
        assert!(denied("b/b-small").contains("`b-small`"));
        assert!(denied("c").contains("`c`"));
        assert!(denied("c/c-one").contains("`c`"));
        assert!(denied("a/a-two").contains("`old`"));
        assert!(denied("old").contains("`old`"));
        // Not denied: the other provider's model, and an alias for a
        // model nobody denies.
        assert_eq!(cat.resolve("a/a-one").unwrap().reference(), "a/a-one");
        assert_eq!(cat.resolve("fast").unwrap().reference(), "a/a-one");
        // Unchecked still sees a denied model.
        assert_eq!(
            cat.resolve_unchecked("b/b-large").unwrap().reference(),
            "b/b-large"
        );
        // The default falls back to default_provider, with a note.
        let text_d = text.replace("deny = [", "default = \"b/b-large\"\ndeny = [");
        let cat = Catalog::from_config(&cfg(&text_d));
        let (e, note) = cat.default_entry().unwrap();
        assert_eq!(e.reference(), "a/a-one");
        assert!(note.unwrap().contains("denied by [models] deny"));
    }

    /// `[models.exact]`: the connected id must be the pinned one; a
    /// provider that silently retargets is refused, not run.
    #[test]
    fn an_exact_pin_refuses_a_retargeted_id() {
        let text = |id: &str| {
            format!(
                r#"
default_provider = "a"

[models.exact]
"a/a-one" = "{id}"

[providers.a]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "a-one"
"#
            )
        };
        // The pin matches the connected id: resolves, and the pin shows.
        let cat = Catalog::from_config(&cfg(&text("a-one")));
        let e = cat.resolve("a/a-one").unwrap();
        assert_eq!(cat.exact_pin(e), Some("a-one"));
        assert!(cat.forbidden(e).is_none());
        // The provider retargeted: refused, by ref and by alias-word.
        let cat = Catalog::from_config(&cfg(&text("a-one-2026-01-01")));
        let err = cat.resolve("a").unwrap_err();
        assert!(
            err.contains("pinned to `a-one-2026-01-01` by [models] exact")
                && err.contains("`a-one` is connected"),
            "{err}"
        );
        // Unchecked still sees it (so it can be changed or removed).
        let e = cat.resolve_unchecked("a/a-one").unwrap();
        assert_eq!(cat.exact_pin(e), Some("a-one-2026-01-01"));
        // An empty pin pins nothing.
        let cat = Catalog::from_config(&cfg(&text("  ")));
        assert!(cat.resolve("a/a-one").is_ok());
        assert_eq!(cat.exact_pin(cat.resolve_unchecked("a").unwrap()), None);
    }

    /// Nothing connected at all: the error points at `ferrule setup`, not
    /// at `model default` (which needs a model to point at).
    #[test]
    fn with_no_providers_the_error_says_setup() {
        let cat = Catalog::from_config(&cfg(""));
        let e = cat.default_entry().unwrap_err();
        assert!(e.contains("run `ferrule setup`"), "{e}");
    }

    /// M23: a v0.3.0 config (no `api` anywhere) loads as it did, except
    /// that Anthropic's own URL now gets the native driver; `api = "chat"`
    /// keeps the old route, and an explicit word beats the URL.
    #[test]
    fn the_driver_is_inferred_from_the_url_unless_the_config_names_it() {
        let v030 = r#"
default_provider = "anthropic"

[providers.anthropic]
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-5"
profile = "anthropic"
price_input_per_mtok = 2.0
price_cached_input_per_mtok = 0.2
price_output_per_mtok = 10.0

[providers.anthropic.models."claude-opus-5"]

[providers.kimi]
base_url = "https://api.moonshot.ai/v1"
api_key_env = "MOONSHOT_API_KEY"
model = "kimi-k2.6"
profile = "kimi"
"#;
        let cat = Catalog::from_config(&cfg(v030));
        let sonnet = cat.resolve("anthropic/claude-sonnet-5").unwrap();
        assert_eq!((sonnet.api, sonnet.api_set), (Api::Anthropic, false));
        assert_eq!(sonnet.driver(), "anthropic (inferred)");
        // Cache writes default to 1.25× input on the native driver.
        assert_eq!(sonnet.pricing.unwrap().cache_write, Some(2.5));
        let opus = cat.resolve("anthropic/claude-opus-5").unwrap();
        assert_eq!(opus.api, Api::Anthropic, "a model takes its provider's");
        let kimi = cat.resolve("kimi").unwrap();
        assert_eq!(kimi.api, Api::Chat);

        let pinned = v030.replace(
            "profile = \"anthropic\"",
            "profile = \"anthropic\"\napi = \"chat\"",
        );
        let cat = Catalog::from_config(&cfg(&pinned));
        let sonnet = cat.resolve("anthropic").unwrap();
        assert_eq!(
            (sonnet.api, sonnet.driver().as_str()),
            (Api::Chat, "chat (set)")
        );
        assert_eq!(sonnet.pricing.unwrap().cache_write, None);

        let gateway = r#"
[providers.gw]
base_url = "https://gateway.example/v1"
api_key_env = "GW_KEY"
model = "gpt-5.5"
api = "responses"
effort = "low"

[providers.gw.models."o-mini"]
effort = "high"
max_tokens = 32000
"#;
        let cat = Catalog::from_config(&cfg(gateway));
        let main = cat.resolve("gw/gpt-5.5").unwrap();
        assert_eq!((main.api, main.api_set), (Api::Responses, true));
        assert_eq!(main.options.effort.as_deref(), Some("low"));
        let mini = cat.resolve("gw/o-mini").unwrap();
        assert_eq!(mini.options.effort.as_deref(), Some("high"));
        assert_eq!(mini.options.max_tokens, Some(32_000));

        let bad = toml::from_str::<Config>(&gateway.replace("\"responses\"", "\"grpc\""));
        let e = bad.unwrap_err().to_string();
        assert!(e.contains("unknown api \"grpc\""), "{e}");
    }

    #[test]
    fn a_model_takes_its_providers_fields_one_by_one() {
        let cat = Catalog::from_config(&cfg(CONFIG));
        let two = cat.resolve("a/a-two").unwrap();
        assert_eq!(
            two.pricing,
            Some(ProviderPricing {
                input: 1.0,
                cached_input: 0.5,
                output: 8.0,
                cache_write: None,
            })
        );
        assert_eq!(two.profile, "openai");
        assert_eq!(two.harness().context_window, 1000);
        assert_eq!(cat.resolve("fast").unwrap().profile, "kimi");
        assert_eq!(
            cat.resolve("b").unwrap().pricing,
            None,
            "no prices, no cost"
        );
        // A row priced by the model that ran, else its provider's.
        assert_eq!(cat.price("a", "a-two").unwrap().output, 8.0);
        assert_eq!(cat.price("a", "hand-picked").unwrap().output, 2.0);
        assert_eq!(cat.price("zz", "a-one"), None);
    }

    #[test]
    fn a_model_sees_photos_by_its_name_unless_the_config_says() {
        let text = r#"
default_provider = "a"
[providers.a]
base_url = "https://api.example.com/v1"
api_key_env = "PATH"
model = "gpt-4o"
[providers.a.models."deepseek-chat"]
[providers.a.models."gpt-4o-mini"]
vision = false
[providers.a.models."my-tuned-model"]
vision = true
[providers.a.models."claude-sonnet-5-5"]
"#;
        let cat = Catalog::from_config(&cfg(text));
        let sees = |m: &str| cat.resolve(&format!("a/{m}")).unwrap().sees_images();
        assert!(sees("gpt-4o"));
        assert!(!sees("deepseek-chat"), "a text-only name");
        assert!(!sees("gpt-4o-mini"), "the config says no");
        assert!(sees("my-tuned-model"), "the config says yes");
        assert!(sees("claude-sonnet-5-5"));
        // The claim goes to the driver too.
        let e = cat.resolve("a/my-tuned-model").unwrap();
        assert_eq!(e.options.vision, Some(true));
    }

    #[test]
    fn an_old_config_is_one_model_per_provider() {
        let old = r#"
default_provider = "kimi"
[providers.kimi]
base_url = "https://api.moonshot.ai/v1"
api_key_env = "PATH"
model = "kimi-k2.6"
profile = "kimi"
"#;
        let cat = Catalog::from_config(&cfg(old));
        assert_eq!(cat.entries.len(), 1);
        let (e, note) = cat.default_entry().unwrap();
        assert_eq!((e.reference(), note), ("kimi/kimi-k2.6".into(), None));
        let (h, kimi) = (e.harness(), HarnessProfile::by_name("kimi"));
        assert_eq!((h.name, h.context_window), (kimi.name, kimi.context_window));
        assert_eq!(e.base_url, "https://api.moonshot.ai/v1");
    }

    #[test]
    fn the_default_falls_back_to_default_provider_with_a_note() {
        let mut c = cfg(CONFIG);
        c.models.default = Some("fast".into());
        assert_eq!(
            Catalog::from_config(&c)
                .default_entry()
                .unwrap()
                .0
                .reference(),
            "b/b-small"
        );
        c.models.default = Some("gone/model".into());
        let cat = Catalog::from_config(&c);
        let (e, note) = cat.default_entry().unwrap();
        assert_eq!(e.reference(), "a/a-one");
        assert!(note.unwrap().contains("gone/model"));
        c.default_provider = None;
        let e = Catalog::from_config(&c).default_entry().unwrap_err();
        assert!(e.contains("ferrule model default"), "{e}");
    }

    #[test]
    fn precedence_is_fixed_then_task_then_pin_then_default() {
        let (dir, m) = models(CONFIG);
        let chat = Scope::for_session("telegram__42");
        let other = Scope::for_session("telegram__7");
        let task = Scope::for_session("scheduler__t1");
        assert_eq!(chat.chat, Some(("telegram".into(), "42".into())));
        assert_eq!(task.task.as_deref(), Some("t1"));
        let want = |s: &Scope| m.wanted(s).map(|e| e.reference());

        assert_eq!(want(&chat).unwrap(), "a/a-one");
        std::fs::write(
            dir.path().join("pins.json"),
            r#"{"telegram:42": "fast", "scheduler:t1": "a/a-two"}"#,
        )
        .unwrap();
        assert_eq!(want(&chat).unwrap(), "b/b-small", "the pin");
        assert_eq!(want(&other).unwrap(), "a/a-one", "another chat stays");

        m.set_task_models(Arc::new(|t: &str| (t == "t1").then(|| "b".to_string())));
        assert_eq!(want(&task).unwrap(), "b/b-large", "the task's model");
        let fixed = chat.clone().fixed(Some("a/a-two".into()), "role verifier");
        assert_eq!(want(&fixed).unwrap(), "a/a-two", "a role or one-off wins");

        // Asked for on purpose: a removed model is an error, not the default.
        let e = want(&chat.clone().fixed(Some("x/y".into()), "role verifier")).unwrap_err();
        assert!(e.starts_with("role verifier asks for `x/y`"), "{e}");
        m.set_task_models(Arc::new(|_: &str| Some("gone".to_string())));
        assert!(want(&task).unwrap_err().contains("scheduled task `t1`"));
        // A pin to a removed model: the chat is on the default.
        std::fs::write(dir.path().join("pins.json"), r#"{"telegram:42": "gone"}"#).unwrap();
        assert_eq!(want(&chat).unwrap(), "a/a-one");
    }

    #[test]
    fn a_hand_edit_is_read_at_the_next_call_and_a_broken_one_is_not() {
        let (dir, m) = models(CONFIG);
        let path = dir.path().join("ferrule.toml");
        let s = Scope::default();
        assert_eq!(m.wanted(&s).unwrap().reference(), "a/a-one");
        std::fs::write(
            &path,
            format!("{CONFIG}\n# edited\n")
                .replace("default_provider = \"a\"", "default_provider = \"b\""),
        )
        .unwrap();
        assert_eq!(m.wanted(&s).unwrap().reference(), "b/b-large");
        std::fs::write(&path, "this is [not toml").unwrap();
        assert_eq!(
            m.wanted(&s).unwrap().reference(),
            "b/b-large",
            "the last good one"
        );
    }

    #[test]
    fn an_outage_moves_calls_to_the_fallback_until_the_mark_clears() {
        let (_dir, m) = models(CONFIG);
        let s = Scope::for_session("telegram__42");
        let a = Served {
            provider: "a".into(),
            model: "a-one".into(),
        };
        let outage = CoreError::Transient {
            message: "HTTP 503 Service Unavailable: {}".into(),
            retry_after: None,
        };
        assert_eq!(
            m.fail_over(&s, &a, &CoreError::Io(std::io::Error::other("x"))),
            None,
            "not the provider's"
        );
        assert!(m.down().is_empty());
        let over = m.fail_over(&s, &a, &outage).unwrap();
        assert_eq!(
            (over.from.as_str(), over.to.as_str()),
            ("a/a-one", "b/b-large")
        );
        let route = m.route(&s).unwrap();
        assert_eq!(route.entry.reference(), "b/b-large");
        assert_eq!(route.instead_of.as_deref(), Some("a/a-one"));
        let down = m.down();
        assert_eq!(down[0].0, "a/a-one");
        assert_eq!(down[0].2, "HTTP 503 after its retries");

        // The fallback failing too: nowhere left to go.
        let b = Served {
            provider: "b".into(),
            model: "b-large".into(),
        };
        assert_eq!(m.fail_over(&s, &b, &outage), None);

        // The primary answering again clears its mark.
        m.state.lock().unwrap().down.clear();
        m.fail_over(&s, &a, &outage).unwrap();
        m.state
            .lock()
            .unwrap()
            .down
            .get_mut("a/a-one")
            .unwrap()
            .until = Instant::now();
        let route = m.route(&s).unwrap();
        assert_eq!(
            route.entry.reference(),
            "a/a-one",
            "tried again after the mark"
        );
        m.served(&s, &route, true);
        assert!(m.state.lock().unwrap().down.is_empty());
        assert_eq!(m.last_served("telegram__42").unwrap().0, "a/a-one");
    }

    #[test]
    fn no_fallback_list_means_no_fallback() {
        let (_dir, m) = models(&CONFIG.replace("fallback = [\"b/b-large\"]", ""));
        let a = Served {
            provider: "a".into(),
            model: "a-one".into(),
        };
        let outage = CoreError::Transient {
            message: "connection refused".into(),
            retry_after: None,
        };
        assert_eq!(m.fail_over(&Scope::default(), &a, &outage), None);
        assert!(m.down().is_empty(), "nothing marked either");
    }

    #[test]
    fn a_missing_key_is_said_plainly() {
        let (_dir, m) = models(CONFIG);
        let s = Scope::default().fixed(Some("c".into()), "--model");
        let e = m.route(&s).err().unwrap();
        assert!(
            e.starts_with(
                "no key: `$FERRULE_M21_TEST_KEY_THAT_IS_NEVER_SET` isn't set (c/vendor/shared)"
            ),
            "{e}"
        );
    }

    #[test]
    fn a_change_is_written_into_the_file_as_it_is_now_and_keeps_its_comments() {
        let (dir, m) = models(&format!("# mine\n{CONFIG}"));
        let path = dir.path().join("ferrule.toml");
        // A hand edit after the process started, which the change keeps.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("# mine", "# mine, edited")).unwrap();
        let done = m.set_default("fast", "cli").unwrap();
        assert_eq!(done.said, "The default is now b/b-small (was a/a-one).");
        assert_eq!(done.view.default.as_deref(), Some("b/b-small"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine, edited"), "{text}");
        // An alias stays an alias, so it follows the alias.
        assert!(text.contains("default = \"fast\""), "{text}");
        assert!(!dir.path().join("ferrule.toml.lock").exists());
        // A new process reads the same.
        let again = Models::new(path.clone(), None, &cfg(&text));
        assert_eq!(
            again.wanted(&Scope::default()).unwrap().reference(),
            "b/b-small"
        );
        // Nothing connected by that name: nothing written, and why.
        let err = m.set_default("nope", "cli").unwrap_err().to_string();
        assert!(err.contains("isn't a connected model"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn pins_fallback_add_remove_and_aliases() {
        let (dir, m) = models(CONFIG);
        let chat = Scope::for_session("telegram__42");
        m.pin("telegram", "42", "a/a-two", "telegram chat 42")
            .unwrap();
        assert_eq!(m.wanted(&chat).unwrap().reference(), "a/a-two");
        let pins = std::fs::read_to_string(dir.path().join("pins.json")).unwrap();
        assert!(pins.contains("\"telegram:42\": \"a/a-two\""), "{pins}");
        let done = m.unpin("telegram", "42", "cli").unwrap();
        assert!(
            done.said.contains("back on the default (a/a-one)"),
            "{}",
            done.said
        );
        assert_eq!(m.wanted(&chat).unwrap().reference(), "a/a-one");

        let done = m
            .set_fallback(&["fast".into(), "b".into(), "b/b-small".into()], "cli")
            .unwrap();
        assert_eq!(done.view.fallback, ["b/b-small", "b/b-large"]);
        m.set_fallback(&[], "cli").unwrap();
        assert!(m.catalog().fallback.is_empty());
        let text = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(!text.contains("fallback"), "{text}");

        let done = m.add_model("a", "a-three", Some("cheap"), "cli").unwrap();
        assert!(
            done.said.starts_with("a/a-three is connected, as `cheap`."),
            "{}",
            done.said
        );
        assert_eq!(m.resolve("cheap").unwrap().reference(), "a/a-three");
        // Its prices are the provider's.
        assert_eq!(m.resolve("cheap").unwrap().pricing.unwrap().input, 1.0);
        let err = m.add_model("zz", "x", None, "cli").unwrap_err().to_string();
        assert!(err.contains("there's no provider `zz`"), "{err}");
        let err = m
            .add_model("a", "a-two", None, "cli")
            .unwrap_err()
            .to_string();
        assert!(err.contains("connected already"), "{err}");
        let err = m.set_alias("a", Some("b"), "cli").unwrap_err().to_string();
        assert!(err.contains("a provider's name"), "{err}");

        m.set_fallback(&["cheap".into()], "cli").unwrap();
        let done = m.remove_model("cheap", "cli").unwrap();
        assert_eq!(
            done.said,
            "a/a-three isn't connected anymore. It's gone from the fallback list and the alias `cheap` too."
        );
        assert!(m.resolve("a/a-three").is_err());
        let err = m.remove_model("a", "cli").unwrap_err().to_string();
        assert!(err.contains("`a`'s own model"), "{err}");
        m.set_default("b/b-small", "cli").unwrap();
        let err = m.remove_model("b/b-small", "cli").unwrap_err().to_string();
        assert!(err.contains("is the default"), "{err}");

        m.set_alias("big", Some("fast"), "cli").unwrap();
        assert_eq!(m.resolve("big").unwrap().reference(), "b/b-small");
        m.set_alias("big", None, "cli").unwrap();
        assert!(m.resolve("big").is_err());
    }

    #[test]
    fn provider_errors_read_plainly() {
        let cat = Catalog::from_config(&cfg(CONFIG));
        let e = cat.resolve("a").unwrap();
        let t = |m: &str| explain(e, &CoreError::Provider(m.into()));
        assert!(t("HTTP 401 Unauthorized: {}")
            .starts_with("the key was refused (HTTP 401). Check `$PATH`"));
        assert!(t("HTTP 404 Not Found: {}").contains("doesn't know the model `a-one`"));
        assert!(
            t("HTTP 400 Bad Request: {\"error\":\"model 'x' does not exist\"}")
                .contains("doesn't know the model `a-one`")
        );
        let x = explain(
            e,
            &CoreError::Transient {
                message: "HTTP 503 Service Unavailable: {}".into(),
                retry_after: None,
            },
        );
        assert_eq!(x, "the provider is failing right now (HTTP 503)");
        assert!(t("HTTP 429 Too Many Requests: {}").contains("rate-limited"));
        assert!(t("request failed: connection refused")
            .starts_with("couldn't reach http://127.0.0.1:1/v1"));
    }

    #[tokio::test]
    async fn a_test_of_a_model_without_a_key_says_so_without_a_call() {
        let (_dir, m) = models(CONFIG);
        let out = m.test("c").await;
        assert!(!out.ok);
        assert!(
            out.said
                .starts_with("no key: `$FERRULE_M21_TEST_KEY_THAT_IS_NEVER_SET`"),
            "{}",
            out.said
        );
        let out = m.test("a").await;
        assert!(!out.ok);
        assert!(out.said.starts_with("couldn't reach"), "{}", out.said);
    }

    /// The owner's chat, as the hub's notifier sees it.
    #[derive(Default)]
    struct Told(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ferrule_trust::Notifier for Told {
        async fn send(&self, _chat: i64, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn told_hub(dir: &Path) -> (Arc<Hub>, Arc<Told>) {
        let hub = Arc::new(
            Hub::new(
                Default::default(),
                dir,
                &dir.join("ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        );
        hub.set_owner(Some(42));
        let told = Arc::new(Told::default());
        hub.set_notifier(Some(told.clone()));
        (hub, told)
    }

    async fn settle(told: &Told, n: usize) -> Vec<String> {
        for _ in 0..100 {
            if told.0.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Anything more would have arrived by now.
        tokio::time::sleep(Duration::from_millis(30)).await;
        told.0.lock().unwrap().clone()
    }

    const PLAN_CONFIG: &str = r#"
default_provider = "chatgpt"

[models]
fallback = ["b/b-large"]

[providers.chatgpt]
plan = "chatgpt"
model = "gpt-5.5"

[providers.b]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "b-large"
"#;

    /// M35: the plan's usage limit sends calls to the fallback until the
    /// plan resets, and the owner hears when that is.
    #[tokio::test]
    async fn a_plan_at_its_usage_limit_falls_back_until_it_resets() {
        let (dir, m) = models(PLAN_CONFIG);
        let (hub, told) = told_hub(dir.path());
        m.attach_hub(hub);
        let s = Scope::for_session("telegram__42");
        let plan = Served {
            provider: "chatgpt".into(),
            model: "gpt-5.5".into(),
        };
        let two_days = 2 * 86_400;
        let limit = CoreError::Transient {
            message: "HTTP 429 usage_limit_reached: the ChatGPT plan's usage limit is reached (plus); it resets in 2 d 0 h (10:00 UTC)".into(),
            retry_after: Some(Duration::from_secs(two_days)),
        };
        let over = m.fail_over(&s, &plan, &limit).unwrap();
        assert_eq!(
            (over.from.as_str(), over.to.as_str()),
            ("chatgpt/gpt-5.5", "b/b-large")
        );
        let down = m.down();
        assert_eq!(down[0].0, "chatgpt/gpt-5.5");
        assert!(
            down[0].1 > Duration::from_secs(two_days - 60),
            "{:?}",
            down[0].1
        );
        assert_eq!(
            down[0].2,
            "the ChatGPT plan's usage limit is reached (plus)"
        );

        let route = m.route(&s).unwrap();
        assert_eq!(route.entry.reference(), "b/b-large");
        m.served(&s, &route, true);
        m.served(&s, &route, true);
        let said = settle(&told, 1).await;
        assert_eq!(said.len(), 1, "told once: {said:?}");
        assert!(
            said[0].contains("usage limit is reached")
                && said[0].contains("again when it resets, in 2 d 0 h"),
            "{}",
            said[0]
        );
    }

    /// M35: an expired or revoked sign-in is said to the owner once, and
    /// again only after a call on the plan worked in between.
    #[tokio::test]
    async fn an_expired_sign_in_is_said_to_the_owner_once() {
        let (dir, m) = models(PLAN_CONFIG);
        let (hub, told) = told_hub(dir.path());
        m.attach_hub(hub);
        let cat = Catalog::from_config(&cfg(PLAN_CONFIG));
        let plan = cat.resolve("chatgpt").unwrap().clone();
        let keyed = cat.resolve("b").unwrap().clone();
        let expired = CoreError::Provider(ferrule_plans::chatgpt::EXPIRED.into());
        let other = CoreError::Provider("HTTP 500".into());
        m.plan_signin(&plan, Err(&expired));
        m.plan_signin(&plan, Err(&expired));
        m.plan_signin(&plan, Err(&other));
        m.plan_signin(&keyed, Err(&expired));
        let said = settle(&told, 1).await;
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("The ChatGPT plan's sign-in for chatgpt expired or was revoked")
                && said[0].contains("`ferrule login chatgpt`")
                && said[0].contains("/login chatgpt"),
            "{}",
            said[0]
        );
        m.plan_signin(&plan, Ok(()));
        m.plan_signin(&plan, Err(&expired));
        assert_eq!(settle(&told, 2).await.len(), 2);
    }

    /// M35: a plan's call is $0 in the ledger with what it would have
    /// cost at API prices; the budget counts nothing for it.
    #[test]
    fn a_plan_call_is_free_with_its_notional_price() {
        // Priced on the plan; `b` has no prices.
        let text = PLAN_CONFIG.replace(
            "model = \"gpt-5.5\"\n",
            "model = \"gpt-5.5\"\nprice_input_per_mtok = 1.0\n\
             price_cached_input_per_mtok = 0.5\nprice_output_per_mtok = 2.0\n",
        );
        let c = cfg(&text);
        let cat = Arc::new(Catalog::from_config(&c));
        let (p, q) = (cat.clone(), cat.clone());
        let prices: crate::ledger::Prices = Arc::new(move |pr: &str, mo: &str| p.price(pr, mo));
        let plans: crate::ledger::PlanOf = Arc::new(move |pr: &str| q.plan_of(pr));
        let mut row: ferrule_core::LedgerRecord = serde_json::from_value(serde_json::json!({
            "timestamp": "2026-09-27T00:00:00Z", "session_id": "s", "task_shape": "chat",
            "provider": "chatgpt", "model": "gpt-5.5", "iteration": 0,
            "input_tokens": 1_000_000, "cached_input_tokens": 0, "output_tokens": 1_000_000,
            "tool_calls": 0, "latency_ms": 1, "outcome": "ok"
        }))
        .unwrap();
        let budget = crate::trust::pricer(prices.clone(), plans.clone());
        assert_eq!(budget(&row), Some(0.0));
        crate::ledger::price_row(&mut row, &prices, Some(&plans));
        assert_eq!(row.plan.as_deref(), Some("chatgpt"));
        assert_eq!(row.cost_usd, Some(0.0));
        assert!((row.notional_usd.unwrap() - 3.0).abs() < 1e-9);
        // A keyed provider's row is priced as before, with no plan.
        let mut keyed = row.clone();
        (
            keyed.provider,
            keyed.model,
            keyed.plan,
            keyed.notional_usd,
            keyed.cost_usd,
        ) = ("b".into(), "b-large".into(), None, None, None);
        crate::ledger::price_row(&mut keyed, &prices, Some(&plans));
        assert_eq!(
            (keyed.plan, keyed.notional_usd, keyed.cost_usd),
            (None, None, None)
        );

        let rows = crate::ledger::aggregate(&[row]);
        let table = crate::ledger::render_table(&rows);
        assert!(
            table.contains("0.0000 (plan; 3.0000 at API prices)"),
            "{table}"
        );
    }
}
