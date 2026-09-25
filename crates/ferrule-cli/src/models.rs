//! M21: several models at once, a default, and a model per agent
//! (docs/m21-models.md). The catalog of connected models is read from the
//! config; every agent gets a [`RoutedProvider`] that picks its model per
//! call from the agent's [`Scope`], so a change reaches a running lane at
//! its next call.

use crate::config::{Config, ProviderConfig};
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
}

impl Entry {
    /// A driver for this model with `key`.
    pub fn client(&self, key: impl Into<String>) -> Arc<dyn Provider> {
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
        let mut p = HarnessProfile::by_name(&self.profile);
        if let Some(w) = self.context_window {
            p.context_window = w;
        }
        p
    }

    pub fn key(&self) -> Option<String> {
        std::env::var(&self.key_env).ok()
    }

    /// Why a call can't be made: the key isn't in the env or the secrets
    /// file.
    pub fn no_key(&self) -> String {
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
    /// M25: `[routing]`, its tiers resolved.
    pub routing: routing::Routing,
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
            routing: routing::Routing::default(),
        };
        let named: Vec<(String, String)> = cat
            .aliases
            .keys()
            .filter_map(|a| Some((a.clone(), cat.resolve(a).ok()?.reference())))
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
    /// tier's model.
    pub fn resolve(&self, word: &str) -> Result<&Entry, String> {
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
            None => Err(
                "no default model: run `ferrule model default <ref>`, or set default_provider"
                    .into(),
            ),
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
        base_url: p.base_url.clone(),
        key_env: p.api_key_env.clone(),
        profile: mc.profile.clone().unwrap_or_else(|| p.profile.clone()),
        context_window: mc.context_window,
        pricing,
        price_source: mc.price_source.clone(),
        aliases: Vec::new(),
        api: p.api(),
        api_set: p.api.is_some(),
        options: p.driver_options(model),
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
                            "{from} isn't answering ({}), so {reference} answered instead. I'll try {from} again in {} minutes.",
                            d.reason,
                            DOWN_FOR.as_secs() / 60
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
        if !error.is_transient() || self.catalog().fallback.is_empty() {
            return None;
        }
        let from = served.reference();
        let reason = short_reason(error);
        {
            let mut st = self.state.lock().unwrap();
            st.down.insert(
                from.clone(),
                Down {
                    until: Instant::now() + DOWN_FOR,
                    reason: reason.clone(),
                    told: false,
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

/// "HTTP 503 after its retries", "no connection", or the start of the
/// message.
fn short_reason(error: &CoreError) -> String {
    let msg = match error {
        CoreError::Transient { message, .. } => message.as_str(),
        _ => return error.to_string(),
    };
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
            m.fail_over(&s, &a, &CoreError::Provider("HTTP 401".into())),
            None,
            "a refused key isn't an outage"
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
}
