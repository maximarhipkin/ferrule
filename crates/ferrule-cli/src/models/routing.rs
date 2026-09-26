//! M25: routing over M21's models (docs/m25-routing.md §4–6). `[routing]
//! tiers` name connected models, cheap first; a turn starts on its floor
//! (tier 0 for the default, or the tier a `tier:` ref names) and the loop's
//! failure signals move it up. The owner's `/model strong`, the gateway
//! watchdog and the daily cap on spend above tier 0 live here too.

use super::{pin_key, warn_once, Catalog, Entry, Models, Route, Scope, State};
use crate::config::RoutingConfig;
use crate::ledger::ProviderPricing;
use chrono::{NaiveDate, Utc};
use ferrule_core::{Escalation, HarnessProfile, Ladder, Policy, RouteTag, Signal, Usage};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;

/// What a tier ref starts with: `tier:cheap`, `tier:strong`, `tier:1`.
pub const TIER_PREFIX: &str = "tier:";

/// `[routing]` as the catalog resolved it.
#[derive(Debug, Clone, Default)]
pub struct Routing {
    pub enabled: bool,
    /// The tiers that resolve, cheap first.
    pub tiers: Vec<RouteTier>,
    /// Why a tier as written was left out.
    pub problems: Vec<String>,
    pub policy: Policy,
    pub strong_daily_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteTier {
    /// As written in `tiers`: the name rows and events use.
    pub name: String,
    pub entry: Entry,
}

impl Routing {
    pub fn from_config(cfg: &RoutingConfig, cat: &Catalog) -> Self {
        let mut tiers: Vec<RouteTier> = Vec::new();
        let mut problems = Vec::new();
        for word in &cfg.tiers {
            let word = word.trim();
            if word.starts_with(TIER_PREFIX) {
                problems.push(format!("tier `{word}`: a tier names a model, not a tier"));
                continue;
            }
            match cat.resolve(word) {
                Ok(e) if tiers.iter().any(|t| t.entry.reference() == e.reference()) => problems
                    .push(format!(
                        "tier `{word}`: {} is a tier already",
                        e.reference()
                    )),
                Ok(e) => tiers.push(RouteTier {
                    name: word.to_string(),
                    entry: e.clone(),
                }),
                Err(e) => problems.push(format!("tier `{word}`: {e}")),
            }
        }
        if !cfg.tiers.is_empty() && tiers.len() < 2 {
            problems.push(format!(
                "routing needs two tiers that resolve and has {}; it's off",
                tiers.len()
            ));
        }
        Self {
            enabled: cfg.enabled,
            tiers,
            problems,
            policy: cfg.policy(),
            strong_daily_usd: cfg.strong_daily_usd,
        }
    }

    /// Two tiers or more: tier refs resolve.
    pub fn usable(&self) -> bool {
        self.tiers.len() >= 2
    }

    /// Turns are routed: the default and tier refs start a ladder.
    pub fn on(&self) -> bool {
        self.enabled && self.usable()
    }

    pub fn names(&self) -> Vec<String> {
        self.tiers.iter().map(|t| t.name.clone()).collect()
    }

    /// The tier `word` names, when it's a tier ref: `cheap` (0), `strong`
    /// (the top), a 0-based index, or a tier's own name.
    pub fn tier_of(&self, word: &str) -> Option<Result<usize, String>> {
        let what = word.trim().strip_prefix(TIER_PREFIX)?.trim();
        if !self.usable() {
            return Some(Err(format!(
                "`{}` is a tier, and routing has no tiers (`[routing] tiers` needs two connected models)",
                word.trim()
            )));
        }
        let top = self.tiers.len() - 1;
        let found = match what {
            "cheap" => Some(0),
            "strong" => Some(top),
            _ => what
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= top)
                .or_else(|| self.tiers.iter().position(|t| t.name == what)),
        };
        Some(found.ok_or_else(|| {
            format!(
                "there's no tier `{what}`; the tiers are {} (or cheap, strong, 0–{top})",
                self.names().join(", ")
            )
        }))
    }

    /// The floor a turn on `word` starts from: a tier ref, while routing
    /// is on. Off, a tier ref is just that tier's model.
    pub fn floor(&self, word: &str) -> Option<usize> {
        if !self.on() {
            return None;
        }
        self.tier_of(word)?.ok()
    }
}

/// Is `word` a tier ref?
pub fn is_tier_ref(word: &str) -> bool {
    word.trim().starts_with(TIER_PREFIX)
}

/// The routing half of [`State`].
#[derive(Default)]
pub(super) struct RoutingState {
    /// Chats (`channel:chat`) whose next turn starts on the top tier.
    hints: HashSet<String>,
    /// Sessions the gateway's watchdog saw stall.
    stalled: HashSet<String>,
    /// Today's spend above tier 0: the day, and the dollars.
    spend: Option<(NaiveDate, f64)>,
    /// The day the cap was last hit (one note a day).
    capped_on: Option<NaiveDate>,
    /// Where the ledger is, to seed today's spend.
    pub(super) ledger: Option<PathBuf>,
}

/// One agent's routing: its ladder, and what the last call was.
#[derive(Default)]
pub(super) struct Lane {
    /// The ladder, and what it was built from: a config edit resets it.
    ladder: Option<(LadderKey, Ladder)>,
    /// A turn started and the next call hasn't seen it yet.
    pub(super) turn: bool,
    pub(super) tag: Option<RouteTag>,
    /// The tier's model the last call wanted, before an outage swapped it.
    pub(super) wanted: Option<Entry>,
}

type LadderKey = (Vec<String>, usize, Policy);

/// What to audit and say once the state lock is dropped.
pub(super) enum Said {
    Escalated(Escalation),
    Capped(String),
}

impl Models {
    /// Where the ledger is, for the daily cap's first count.
    pub fn set_ledger(&self, path: PathBuf) {
        self.state.lock().unwrap().routing.ledger = Some(path);
    }

    /// The model `scope` asks for and, when routed, the tier its turns
    /// start on (§2: fixed → task → pin → default, a tier ref allowed at
    /// every level).
    pub(super) fn pick_floor(
        &self,
        st: &mut State,
        scope: &Scope,
    ) -> Result<(Entry, Option<usize>), String> {
        let cat = st.catalog.clone();
        let r = &cat.routing;
        if let Some(f) = &scope.fixed {
            return cat
                .resolve(&f.word)
                .map(|e| (e.clone(), r.floor(&f.word)))
                .map_err(|e| format!("{} asks for `{}`, but {e}", f.by, f.word));
        }
        if let Some(task) = &scope.task {
            let lookup = self.tasks.lock().unwrap().clone();
            if let Some(word) = lookup.and_then(|f| f(task)) {
                return cat
                    .resolve(&word)
                    .map(|e| (e.clone(), r.floor(&word)))
                    .map_err(|e| format!("scheduled task `{task}` runs on `{word}`, but {e}"));
            }
        }
        if let Some((ch, chat)) = &scope.chat {
            if let Some(word) = st.pins.get(&pin_key(ch, chat)).cloned() {
                match cat.resolve(&word) {
                    Ok(e) => return Ok((e.clone(), r.floor(&word))),
                    Err(e) => warn_once(
                        st,
                        &format!("pin {ch}:{chat}"),
                        &format!(
                            "{ch} chat {chat} is pinned to `{word}`, but {e}; it's on the default"
                        ),
                    ),
                }
            }
        }
        if r.on() {
            return Ok((r.tiers[0].entry.clone(), Some(0)));
        }
        let (e, note) = cat.default_entry()?;
        let e = e.clone();
        if let Some(note) = note {
            warn_once(st, "default", &note);
        }
        Ok((e, None))
    }

    /// Whether `scope`'s turns are routed now.
    pub fn routed(&self, scope: &Scope) -> bool {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        let cat = st.catalog.clone();
        cat.routing.on()
            && self
                .pick_floor(&mut st, scope)
                .is_ok_and(|(_, f)| f.is_some())
    }

    /// The model `scope` would run on now, and the harness profile for
    /// it: routed, the smallest window of the tiers, so compaction holds
    /// whichever tier answers.
    pub fn wanted_profile(&self, scope: &Scope) -> Result<(Entry, HarnessProfile), String> {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        let (entry, floor) = self.pick_floor(&mut st, scope)?;
        let mut profile = entry.harness();
        if floor.is_some() {
            let tiers = &st.catalog.routing.tiers;
            if let Some(w) = tiers.iter().map(|t| t.entry.harness().context_window).min() {
                profile.context_window = profile.context_window.min(w);
            }
        }
        Ok((entry, profile))
    }

    /// Where the next call of `lane` goes: its ladder's tier (after a new
    /// turn's reset, the owner's hint, a watchdog stall and the cap), then
    /// M21's outage fallback on that tier's model. The level it's on when
    /// routed.
    pub(super) fn route_lane(
        &self,
        scope: &Scope,
        lane: &mut Lane,
    ) -> Result<(Route, Option<usize>), String> {
        let mut said = Vec::new();
        let result = {
            let mut st = self.state.lock().unwrap();
            self.refresh(&mut st);
            self.refresh_pins(&mut st);
            let stalled = st.routing.stalled.remove(&scope.session);
            let (wanted, floor) = self.pick_floor(&mut st, scope)?;
            let cat = st.catalog.clone();
            let r = &cat.routing;
            let (entry, level) = match floor {
                None => {
                    lane.ladder = None;
                    lane.tag = None;
                    (wanted, None)
                }
                Some(floor) => {
                    let key = (r.names(), floor, r.policy.clone());
                    if lane.ladder.as_ref().map(|(k, _)| k) != Some(&key) {
                        let ladder = Ladder::new(key.0.clone(), floor, key.2.clone());
                        lane.ladder = Some((key, ladder));
                    }
                    let ladder = &mut lane.ladder.as_mut().expect("just set").1;
                    if std::mem::take(&mut lane.turn) {
                        let force = scope
                            .chat
                            .as_ref()
                            .is_some_and(|(ch, c)| st.routing.hints.remove(&pin_key(ch, c)));
                        if let Some(up) = ladder.begin_turn(force) {
                            said.push(Said::Escalated(up));
                        }
                        if ladder.level() > 0 && self.capped(&mut st, r) {
                            ladder.clamp(0);
                            said.extend(cap_note(&mut st, r));
                        }
                    }
                    if stalled {
                        said.extend(self.climb(&mut st, r, ladder, &Signal::Watchdog));
                    }
                    let (level, tag) = ladder.serve();
                    lane.tag = Some(tag);
                    (r.tiers[level].entry.clone(), Some(level))
                }
            };
            lane.turn = false;
            lane.wanted = Some(entry.clone());
            super::finish(&mut st, entry).map(|route| (route, level))
        };
        self.say(scope, said);
        result
    }

    /// The loop saw `signal`: one tier up, if the ladder, the tier's key
    /// and the cap allow it.
    pub(super) fn escalate_lane(
        &self,
        scope: &Scope,
        lane: &mut Lane,
        signal: &Signal,
    ) -> Option<Escalation> {
        let said = {
            let mut st = self.state.lock().unwrap();
            let cat = st.catalog.clone();
            let r = &cat.routing;
            let (key, ladder) = lane.ladder.as_mut()?;
            if key.0 != r.names() {
                // The tiers changed; the next call starts a new ladder.
                return None;
            }
            self.climb(&mut st, r, ladder, signal)
        };
        let up = said.iter().find_map(|s| match s {
            Said::Escalated(e) => Some(e.clone()),
            Said::Capped(_) => None,
        });
        self.say(scope, said);
        up
    }

    fn climb(
        &self,
        st: &mut State,
        r: &Routing,
        ladder: &mut Ladder,
        signal: &Signal,
    ) -> Vec<Said> {
        let mut said = Vec::new();
        let up = ladder.escalate(signal, |to| {
            let tier = &r.tiers[to];
            if tier.entry.key().is_none() {
                let why = tier.entry.no_key();
                warn_once(
                    st,
                    &format!("tier {}", tier.name),
                    &format!("routing can't move up to `{}`: {why}", tier.name),
                );
                return false;
            }
            if self.capped(st, r) {
                said.extend(cap_note(st, r));
                return false;
            }
            true
        });
        said.extend(up.map(Said::Escalated));
        said
    }

    /// Today's spend above tier 0 is at the cap.
    fn capped(&self, st: &mut State, r: &Routing) -> bool {
        match r.strong_daily_usd {
            Some(cap) => spent(st, r) >= cap,
            None => false,
        }
    }

    /// A call on tier `level` answered: what it cost counts toward the cap.
    pub(super) fn count_spend(&self, level: usize, pricing: Option<ProviderPricing>, u: &Usage) {
        let (Some(p), true) = (pricing, level > 0) else {
            return;
        };
        let mut st = self.state.lock().unwrap();
        let cat = st.catalog.clone();
        if cat.routing.strong_daily_usd.is_none() {
            return;
        }
        spent(&mut st, &cat.routing);
        if let Some((_, usd)) = &mut st.routing.spend {
            *usd += p.cost(
                u.input_tokens,
                u.cached_input_tokens,
                u.cache_write_input_tokens,
                u.output_tokens,
            );
        }
    }

    /// `/model strong`: the chat's next turn starts on the top tier. Why
    /// not, when the chat isn't routed.
    pub fn force_strong(&self, channel: &str, chat: &str) -> Result<String, String> {
        let scope = Scope {
            session: format!("{channel}__{chat}"),
            chat: Some((channel.to_string(), chat.to_string())),
            ..Scope::default()
        };
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        let (entry, floor) = self.pick_floor(&mut st, &scope)?;
        let r = st.catalog.routing.clone();
        if floor.is_none() {
            return Err(if r.on() {
                format!(
                    "This chat is pinned to {}, which isn't routed; `/model use default` puts it back on the tiers.",
                    entry.reference()
                )
            } else {
                "Routing is off: `ferrule model route set <cheap> <strong>` turns it on.".into()
            });
        }
        let top = r.tiers.last().expect("routing is on");
        st.routing.hints.insert(pin_key(channel, chat));
        Ok(format!(
            "The next turn here runs on the strong tier, {} ({}), then back to {}.",
            top.name,
            top.entry.reference(),
            r.tiers[floor.unwrap_or(0)].name
        ))
    }

    /// The gateway's watchdog saw `session` stall: its next call moves up.
    pub fn stalled(&self, session: &str) {
        self.state
            .lock()
            .unwrap()
            .routing
            .stalled
            .insert(session.to_string());
    }

    /// Today's spend above tier 0 (the views read it from
    /// [`super::routing_admin::RoutingView`]).
    #[cfg(test)]
    pub fn strong_spend_today(&self) -> f64 {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        let cat = st.catalog.clone();
        spent(&mut st, &cat.routing)
    }

    fn say(&self, scope: &Scope, said: Vec<Said>) {
        for s in said {
            match s {
                Said::Escalated(e) => {
                    tracing::info!(session = %scope.session, from = %e.from, to = %e.to, reason = %e.reason, "routing: moved up a tier");
                    self.audit(
                        "routing.escalate",
                        serde_json::json!({
                            "session": scope.session,
                            "from": e.from,
                            "to": e.to,
                            "reason": e.reason,
                        }),
                    );
                }
                Said::Capped(text) => {
                    self.audit(
                        "routing.capped",
                        serde_json::json!({ "session": scope.session, "said": text }),
                    );
                    if let Some(hub) = self.hub.lock().unwrap().clone() {
                        hub.tell_owner(text);
                    }
                }
            }
        }
    }
}

/// Today's (UTC) spend above tier 0: counted from the ledger the first
/// time each day, then added to as calls answer.
pub(super) fn spent(st: &mut State, r: &Routing) -> f64 {
    let today = Utc::now().date_naive();
    if let Some((day, usd)) = st.routing.spend {
        if day == today {
            return usd;
        }
    }
    let cheap = r.tiers.first().map(|t| t.name.clone());
    let since = today.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    let usd = st
        .routing
        .ledger
        .as_deref()
        .and_then(|p| crate::ledger::read_records(p, Some(since)).ok())
        .map(|(rows, _)| {
            rows.iter()
                .filter(|row| {
                    row.route
                        .as_ref()
                        .is_some_and(|t| Some(&t.tier) != cheap.as_ref())
                })
                .filter_map(|row| row.cost_usd)
                .sum()
        })
        .unwrap_or(0.0);
    st.routing.spend = Some((today, usd));
    usd
}

/// The cap's note, once a day.
fn cap_note(st: &mut State, r: &Routing) -> Option<Said> {
    let today = Utc::now().date_naive();
    if st.routing.capped_on == Some(today) {
        return None;
    }
    st.routing.capped_on = Some(today);
    let cap = r.strong_daily_usd.unwrap_or_default();
    Some(Said::Capped(format!(
        "Today's spend above the cheap tier reached ${cap:.2} ([routing] strong_daily_usd), so turns stay on {} until tomorrow (UTC).",
        r.tiers.first().map(|t| t.name.as_str()).unwrap_or("the cheap tier")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::models::{read_pins, RoutedProvider};
    use ferrule_core::lifecycle::{Hook, HookHandler, HookInput, HookRun, HookSource, Matcher};
    use ferrule_core::provider::Served;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse, Provider};
    use ferrule_core::tool::ToolContext;
    use ferrule_core::{
        Agent, AgentConfig, CoreError, HookEvent, HookSet, LedgerRecord, LedgerSink, Message,
        ToolRegistry,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const CONFIG: &str = r#"
default_provider = "c"

[models]
fallback = ["f"]

[models.aliases]
cheap = "c/c-small"
strong = "s/s-big"

[routing]
enabled = true
tiers = ["cheap", "strong"]
strong_daily_usd = 1.0

[providers.c]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "c-small"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 1.0
price_output_per_mtok = 1.0

[providers.c.models."c-small"]
context_window = 200000

[providers.s]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "s-big"
price_input_per_mtok = 4.0
price_cached_input_per_mtok = 4.0
price_output_per_mtok = 4.0

[providers.s.models."s-big"]
context_window = 50000

[providers.m]
base_url = "http://127.0.0.1:3/v1"
api_key_env = "PATH"
model = "m-mid"

[providers.f]
base_url = "http://127.0.0.1:4/v1"
api_key_env = "PATH"
model = "f-one"

[providers.n]
base_url = "http://127.0.0.1:5/v1"
api_key_env = "FERRULE_M25_TEST_KEY_THAT_IS_NEVER_SET"
model = "n-top"
"#;

    type Log = Arc<Mutex<Vec<String>>>;

    /// Answers "ok" as the model it stands in for, 100k tokens in.
    struct Fake {
        reference: String,
        log: Log,
    }

    #[async_trait::async_trait]
    impl Provider for Fake {
        fn name(&self) -> &str {
            &self.reference
        }
        async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            self.log.lock().unwrap().push(self.reference.clone());
            Ok(CompletionResponse {
                message: Message::assistant(Some("ok".into()), vec![], None),
                usage: Usage {
                    input_tokens: 100_000,
                    ..Usage::default()
                },
            })
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        models: Arc<Models>,
        log: Log,
    }

    impl Rig {
        fn new(config: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ferrule.toml");
            std::fs::write(&path, config).unwrap();
            let cfg: Config = toml::from_str(config).unwrap();
            let models = Arc::new(Models::new(path, Some(dir.path().join("pins.json")), &cfg));
            models.set_ledger(dir.path().join("ledger.jsonl"));
            let hub = Arc::new(
                ferrule_trust::Hub::new(
                    Default::default(),
                    dir.path(),
                    &dir.path().join("ledger.jsonl"),
                    Arc::new(ferrule_trust::SystemClock),
                    vec![],
                )
                .unwrap(),
            );
            models.attach_hub(hub);
            let rig = Self {
                dir,
                models,
                log: Log::default(),
            };
            rig.plug();
            rig
        }

        /// Every model with a key answers through a [`Fake`]: its driver
        /// is put in the cache `finish` builds drivers into.
        fn plug(&self) {
            let mut st = self.models.state.lock().unwrap();
            let cat = st.catalog.clone();
            for e in &cat.entries {
                let Some(key) = e.key() else { continue };
                st.clients.insert(
                    (
                        e.provider.clone(),
                        e.base_url.clone(),
                        key,
                        e.model.clone(),
                        e.api,
                        e.options.clone(),
                    ),
                    Arc::new(Fake {
                        reference: e.reference(),
                        log: self.log.clone(),
                    }),
                );
            }
        }

        fn provider(&self, scope: Scope) -> RoutedProvider {
            RoutedProvider::new(self.models.clone(), scope, "c".into())
        }

        fn chat(&self) -> Scope {
            Scope::for_session("telegram__42")
        }

        fn audit(&self, event: &str) -> Vec<serde_json::Value> {
            std::fs::read_to_string(self.dir.path().join("trust/audit.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .filter(|v| v["event"] == event)
                .collect()
        }

        fn rewrite(&self, config: &str) {
            std::fs::write(self.dir.path().join("ferrule.toml"), config).unwrap();
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_output_tokens: None,
            temperature: None,
            stream: None,
        }
    }

    /// One call: which model served it, and its row's route tag as
    /// `tier` or `tier+reason`.
    async fn call(p: &RoutedProvider) -> (String, String) {
        let (served, result) = p.complete_routed(req()).await;
        result.unwrap();
        let served = served.unwrap();
        let tag = match p.route_tag() {
            None => "-".to_string(),
            Some(RouteTag {
                tier,
                escalated: None,
            }) => tier,
            Some(RouteTag {
                tier,
                escalated: Some(why),
            }) => format!("{tier}+{why}"),
        };
        (format!("{}/{}", served.provider, served.model), tag)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[tokio::test]
    async fn the_default_starts_cheap_moves_up_on_a_signal_and_the_next_turn_starts_cheap() {
        let rig = Rig::new(CONFIG);
        let p = rig.provider(rig.chat());
        assert!(p.routes());
        p.begin_turn();
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
        let up = p.escalate(&Signal::CheckFailed).unwrap();
        assert_eq!((up.from.as_str(), up.to.as_str()), ("cheap", "strong"));
        assert_eq!(call(&p).await, pair("s/s-big", "strong+check_failed"));
        // Sticky: the rest of the turn stays up, and the top can't go higher.
        assert_eq!(call(&p).await, pair("s/s-big", "strong"));
        assert!(p.escalate(&Signal::StopHook).is_none());
        p.begin_turn();
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
        let audit = rig.audit("routing.escalate");
        assert_eq!(audit.len(), 1, "{audit:?}");
        assert_eq!(audit[0]["detail"]["reason"], "check_failed");
        assert_eq!(audit[0]["detail"]["session"], "telegram__42");
    }

    #[tokio::test]
    async fn with_de_escalate_off_the_level_holds_for_the_session() {
        let rig = Rig::new(&CONFIG.replace(
            "strong_daily_usd = 1.0",
            "strong_daily_usd = 1.0\nde_escalate = false",
        ));
        let p = rig.provider(rig.chat());
        p.begin_turn();
        call(&p).await;
        p.escalate(&Signal::CheckFailed).unwrap();
        call(&p).await;
        p.begin_turn();
        assert_eq!(call(&p).await, pair("s/s-big", "strong"));
    }

    #[tokio::test]
    async fn off_nothing_routes_and_a_tier_ref_is_just_its_model() {
        let off = CONFIG.replace("enabled = true", "enabled = false");
        let rig = Rig::new(&off);
        let p = rig.provider(rig.chat());
        assert!(!p.routes());
        p.begin_turn();
        assert_eq!(call(&p).await, pair("c/c-small", "-"));
        assert!(p.escalate(&Signal::CheckFailed).is_none());
        let pinned = rig.provider(Scope::default().fixed(Some("tier:strong".into()), "--model"));
        assert!(!pinned.routes());
        assert_eq!(call(&pinned).await, pair("s/s-big", "-"));
        // No [routing] at all: a tier ref is an error that says why.
        let none = Rig::new(&CONFIG.replace("tiers = [\"cheap\", \"strong\"]", ""));
        let why = none.models.resolve("tier:strong").unwrap_err();
        assert!(why.contains("routing has no tiers"), "{why}");
    }

    #[tokio::test]
    async fn tier_refs_set_the_floor_and_a_concrete_model_is_not_routed() {
        let three = CONFIG.replace(
            "tiers = [\"cheap\", \"strong\"]",
            "tiers = [\"cheap\", \"m\", \"strong\"]",
        );
        let rig = Rig::new(&three);
        // A role or --model on `tier:1` starts on the middle tier and can
        // still go up to the top.
        let p = rig.provider(Scope::default().fixed(Some("tier:1".into()), "role planner"));
        p.begin_turn();
        assert_eq!(call(&p).await, pair("m/m-mid", "m"));
        p.escalate(&Signal::Stuck).unwrap();
        assert_eq!(call(&p).await, pair("s/s-big", "strong+no_progress"));
        // The next turn is back on its floor, not tier 0.
        p.begin_turn();
        assert_eq!(call(&p).await, pair("m/m-mid", "m"));

        // A chat pinned to `tier:strong` keeps the ref as written.
        rig.models
            .pin("telegram", "42", "tier:strong", "test")
            .unwrap();
        assert_eq!(
            read_pins(&rig.dir.path().join("pins.json"))["telegram:42"],
            "tier:strong"
        );
        let chat = rig.provider(rig.chat());
        chat.begin_turn();
        assert_eq!(call(&chat).await, pair("s/s-big", "strong"));

        // A concrete pin runs on that model, unrouted.
        rig.models
            .pin("telegram", "42", "c/c-small", "test")
            .unwrap();
        assert!(!chat.routes());
        chat.begin_turn();
        assert_eq!(call(&chat).await, pair("c/c-small", "-"));
        assert!(chat.escalate(&Signal::CheckFailed).is_none());

        // A task on a tier name.
        rig.models
            .set_task_models(Arc::new(|_| Some("tier:m".to_string())));
        let task = rig.provider(Scope::for_session(&format!(
            "{}__t1",
            ferrule_gateway::SCHEDULER_PSEUDO_CHANNEL
        )));
        task.begin_turn();
        assert_eq!(call(&task).await, pair("m/m-mid", "m"));

        let bad = rig.models.resolve("tier:9").unwrap_err();
        assert!(bad.contains("no tier `9`"), "{bad}");
    }

    #[tokio::test]
    async fn model_strong_forces_one_turn() {
        let rig = Rig::new(CONFIG);
        let p = rig.provider(rig.chat());
        let said = rig.models.force_strong("telegram", "42").unwrap();
        assert!(said.contains("strong tier"), "{said}");
        p.begin_turn();
        assert_eq!(call(&p).await, pair("s/s-big", "strong+owner"));
        p.begin_turn();
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
        assert_eq!(
            rig.audit("routing.escalate")[0]["detail"]["reason"],
            "owner"
        );
        // Not routed: said so, nothing stored.
        rig.models.pin("telegram", "42", "f", "test").unwrap();
        let no = rig.models.force_strong("telegram", "42").unwrap_err();
        assert!(no.contains("isn't routed"), "{no}");
    }

    #[tokio::test]
    async fn the_watchdog_moves_the_next_call_up() {
        let rig = Rig::new(CONFIG);
        let p = rig.provider(rig.chat());
        p.begin_turn();
        call(&p).await;
        rig.models.stalled("telegram__42");
        assert_eq!(call(&p).await, pair("s/s-big", "strong+watchdog"));
        // Off in the triggers: no move.
        let off = Rig::new(&CONFIG.replace(
            "strong_daily_usd = 1.0",
            "strong_daily_usd = 1.0\n[routing.triggers]\nwatchdog = false",
        ));
        let p = off.provider(off.chat());
        p.begin_turn();
        off.models.stalled("telegram__42");
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
    }

    #[tokio::test]
    async fn an_escalation_and_the_fallback_compose() {
        let rig = Rig::new(CONFIG);
        let down = |r: &str| {
            rig.models.state.lock().unwrap().down.insert(
                r.to_string(),
                super::super::Down {
                    until: std::time::Instant::now() + std::time::Duration::from_secs(60),
                    reason: "HTTP 503".into(),
                    told: false,
                },
            );
        };
        let p = rig.provider(rig.chat());
        p.begin_turn();
        // The strong tier is down: the escalated turn runs on the fallback,
        // still tagged strong.
        down("s/s-big");
        call(&p).await;
        p.escalate(&Signal::CheckFailed).unwrap();
        assert_eq!(call(&p).await, pair("f/f-one", "strong+check_failed"));
        // The cheap tier down: the fallback answers and nothing escalates.
        rig.models.state.lock().unwrap().down.clear();
        down("c/c-small");
        p.begin_turn();
        assert_eq!(call(&p).await, pair("f/f-one", "cheap"));
        // An outage on the tier asks the fallback list about that tier's
        // model, not the default's.
        let outage = CoreError::Transient {
            message: "HTTP 503".into(),
            retry_after: None,
        };
        let served = Served {
            provider: "f".into(),
            model: "f-one".into(),
        };
        assert!(p.fail_over(Some(&served), &outage).is_none());
    }

    #[tokio::test]
    async fn the_daily_cap_stops_escalation_counted_from_the_ledger_and_live() {
        let rig = Rig::new(CONFIG);
        // $0.60 above tier 0 today, and a cheap row that doesn't count.
        let row = |tier: &str, cost: f64| {
            serde_json::json!({
                "timestamp": Utc::now().to_rfc3339(), "session_id": "x",
                "task_shape": "chat", "provider": "s", "model": "s-big",
                "iteration": 0, "input_tokens": 1, "cached_input_tokens": 0,
                "output_tokens": 1, "tool_calls": 0, "latency_ms": 1,
                "outcome": "ok", "cost_usd": cost, "route": {"tier": tier},
            })
            .to_string()
                + "\n"
        };
        std::fs::write(
            rig.dir.path().join("ledger.jsonl"),
            row("strong", 0.6) + &row("cheap", 5.0),
        )
        .unwrap();
        assert!((rig.models.strong_spend_today() - 0.6).abs() < 1e-9);
        let p = rig.provider(rig.chat());
        p.begin_turn();
        call(&p).await;
        p.escalate(&Signal::CheckFailed).unwrap();
        // 100k tokens at $4/M: $0.40, and the cap of $1.00 is reached.
        call(&p).await;
        assert!((rig.models.strong_spend_today() - 1.0).abs() < 1e-9);
        p.begin_turn();
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
        assert!(p.escalate(&Signal::CheckFailed).is_none());
        // A floor above 0 starts on tier 0 while capped.
        let planner = rig.provider(Scope::default().fixed(Some("tier:strong".into()), "role"));
        planner.begin_turn();
        assert_eq!(call(&planner).await, pair("c/c-small", "cheap"));
        // Audited and said once a day.
        assert_eq!(rig.audit("routing.capped").len(), 1);
    }

    #[tokio::test]
    async fn a_strong_tier_without_a_key_is_not_moved_to() {
        let rig = Rig::new(&CONFIG.replace(
            "tiers = [\"cheap\", \"strong\"]",
            "tiers = [\"cheap\", \"n\"]",
        ));
        let p = rig.provider(rig.chat());
        p.begin_turn();
        call(&p).await;
        assert!(p.escalate(&Signal::CheckFailed).is_none());
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
    }

    #[tokio::test]
    async fn an_edit_to_the_tiers_resets_the_ladder_at_the_next_call() {
        let rig = Rig::new(CONFIG);
        let p = rig.provider(rig.chat());
        p.begin_turn();
        call(&p).await;
        p.escalate(&Signal::CheckFailed).unwrap();
        rig.rewrite(&CONFIG.replace(
            "tiers = [\"cheap\", \"strong\"]",
            "tiers = [\"cheap\", \"m\", \"strong\"]",
        ));
        rig.plug();
        // The old ladder's move doesn't carry over to new tiers.
        assert!(p.escalate(&Signal::StopHook).is_none());
        assert_eq!(call(&p).await, pair("c/c-small", "cheap"));
        p.escalate(&Signal::StopHook).unwrap();
        assert_eq!(call(&p).await, pair("m/m-mid", "m+stop_hook"));
    }

    #[test]
    fn the_profile_has_the_smallest_window_and_problems_are_said() {
        let rig = Rig::new(CONFIG);
        let (entry, profile) = rig.models.wanted_profile(&rig.chat()).unwrap();
        assert_eq!(entry.reference(), "c/c-small");
        assert_eq!(profile.context_window, 50_000);
        let off = Rig::new(&CONFIG.replace("enabled = true", "enabled = false"));
        let (_, profile) = off.models.wanted_profile(&off.chat()).unwrap();
        assert_eq!(profile.context_window, 200_000);

        let broken = Rig::new(&CONFIG.replace(
            "tiers = [\"cheap\", \"strong\"]",
            "tiers = [\"cheap\", \"gone/model\"]",
        ));
        let problems = broken.models.view().problems;
        assert!(
            problems.iter().any(|p| p.starts_with("tier `gone/model`")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("it's off")),
            "{problems:?}"
        );
        // Under two tiers, the default is `[models]`'s, unrouted.
        assert!(!broken.provider(broken.chat()).routes());
    }

    struct Rows(Mutex<Vec<LedgerRecord>>);

    impl LedgerSink for Rows {
        fn record(&self, r: LedgerRecord) {
            self.0.lock().unwrap().push(r);
        }
    }

    /// A Stop hook that turns the first answer back.
    struct Once(AtomicBool);

    #[async_trait::async_trait]
    impl HookHandler for Once {
        fn command(&self) -> String {
            "check-answer".into()
        }
        async fn run(&self, _: &HookInput, _: &ToolContext) -> HookRun {
            let again = self.0.swap(true, Ordering::SeqCst);
            HookRun {
                exit_code: Some(if again { 0 } else { 2 }),
                stderr: "say which file you changed".into(),
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn through_the_agent_loop_a_stop_hook_rejection_finishes_on_the_strong_tier() {
        let rig = Rig::new(CONFIG);
        let provider = Arc::new(rig.provider(rig.chat()));
        let rows = Arc::new(Rows(Mutex::default()));
        let mut hooks = HookSet::new();
        hooks.add(Hook::new(
            HookEvent::Stop,
            Matcher::parse(None),
            HookSource::User,
            Arc::new(Once(AtomicBool::new(false))),
        ));
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            ferrule_core::HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_ledger(rows.clone(), "chat", None, "c-small")
        .with_hooks(hooks);
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        agent.run("fix it", tx).await.unwrap();
        let rows = rows.0.lock().unwrap();
        let got: Vec<(String, Option<RouteTag>)> = rows
            .iter()
            .map(|r| (r.model.clone(), r.route.clone()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "c-small".to_string(),
                    Some(RouteTag {
                        tier: "cheap".into(),
                        escalated: None
                    })
                ),
                (
                    "s-big".to_string(),
                    Some(RouteTag {
                        tier: "strong".into(),
                        escalated: Some("stop_hook".into())
                    })
                ),
            ]
        );
        let mut moved = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ferrule_core::AgentEvent::Escalated { .. }) {
                moved += 1;
            }
        }
        assert_eq!(moved, 1);
        assert_eq!(*rig.log.lock().unwrap(), ["c/c-small", "s/s-big"]);
    }
}
