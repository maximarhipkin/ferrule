//! Reading and changing the models (docs/m21-models.md §4, §9): the one
//! API the CLI, the Telegram door and M22's dashboard call. The owner check
//! stays with the caller; `by` is only a label for the audit.

use super::*;
use crate::filewrite;
use crate::setup::{put, table, Target};
use anyhow::Context as _;
use ferrule_core::Message;
use toml_edit::{Item, Table};

/// Everything the owner sees about the models, as it is now.
#[derive(Debug, Clone, Serialize)]
pub struct ModelsView {
    pub models: Vec<ModelRow>,
    /// The default's `provider/model`, when there is one that's connected.
    pub default: Option<String>,
    /// `[models] default` as written ("" when unset).
    pub default_set: String,
    pub fallback: Vec<String>,
    pub pins: Vec<PinRow>,
    pub last_served: Vec<ServedRow>,
    /// Plain sentences: a default that's gone, a pin to a removed model, a
    /// config that no longer parses.
    pub problems: Vec<String>,
    /// M25: `[routing]`.
    pub routing: super::routing_admin::RoutingView,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRow {
    pub reference: String,
    pub provider: String,
    pub model: String,
    pub aliases: Vec<String>,
    pub primary: bool,
    pub default: bool,
    /// 1 for the first fallback.
    pub fallback_rank: Option<usize>,
    pub key_env: String,
    pub key_present: bool,
    pub profile: String,
    /// M23: `anthropic (inferred)`, `chat (set)`.
    pub driver: String,
    pub context_window: usize,
    pub pricing: Option<ProviderPricing>,
    pub price_source: Option<String>,
    /// Skipped for this long yet, after an outage.
    pub down_secs: Option<u64>,
    pub down_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PinRow {
    pub channel: String,
    pub chat: String,
    /// As pinned.
    pub reference: String,
    /// What it resolves to now, if it's connected.
    pub resolved: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServedRow {
    pub session: String,
    pub reference: String,
    /// RFC 3339.
    pub at: String,
}

/// What a change did: a sentence for the owner, and the view after it.
#[derive(Debug, Clone, Serialize)]
pub struct Done {
    pub said: String,
    pub view: ModelsView,
}

/// One real call to a model.
#[derive(Debug, Clone, Serialize)]
pub struct TestOutcome {
    pub reference: String,
    pub ok: bool,
    /// "answered in 812 ms", or why it failed, plainly.
    pub said: String,
}

/// How long `test` waits for an answer.
const TEST_WAIT: Duration = Duration::from_secs(60);

impl Models {
    /// The models, the default, the fallback list, the pins, what each
    /// session ran on last, and what's wrong.
    pub fn view(&self) -> ModelsView {
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st);
        self.refresh_pins(&mut st);
        let cat = st.catalog.clone();
        let now = Instant::now();
        let mut problems = Vec::new();
        if st.broken {
            problems.push(format!(
                "{} doesn't parse; these are the models it had",
                self.path.display()
            ));
        }
        let default = match cat.default_entry() {
            Ok((e, note)) => {
                problems.extend(note);
                Some(e.reference())
            }
            Err(e) => {
                problems.push(e);
                None
            }
        };
        problems.extend(cat.routing.problems.iter().cloned());
        let fallback: Vec<String> = cat.fallback.clone();
        for f in &fallback {
            if let Err(e) = cat.resolve(f) {
                problems.push(format!("fallback `{f}`: {e}"));
            }
        }
        for (alias, target) in &cat.aliases {
            if cat.resolve(alias).is_err() {
                problems.push(format!(
                    "the alias `{alias}` points at `{target}`, which isn't connected"
                ));
            }
        }
        let rank = |r: &str| {
            fallback
                .iter()
                .position(|f| cat.resolve(f).is_ok_and(|e| e.reference() == r))
                .map(|i| i + 1)
        };
        let models = cat
            .entries
            .iter()
            .map(|e| {
                let r = e.reference();
                let down = st.down.get(&r).filter(|d| d.until > now);
                let key_present = e.key().is_some_and(|k| !k.is_empty());
                if !key_present && default.as_deref() == Some(r.as_str()) {
                    problems.push(e.no_key());
                }
                ModelRow {
                    default: default.as_deref() == Some(r.as_str()),
                    fallback_rank: rank(&r),
                    key_present,
                    key_env: e.key_env.clone(),
                    profile: e.profile.clone(),
                    driver: e.driver(),
                    context_window: e.harness().context_window,
                    pricing: e.pricing,
                    price_source: e.price_source.clone(),
                    down_secs: down.map(|d| (d.until - now).as_secs()),
                    down_reason: down.map(|d| d.reason.clone()),
                    aliases: e.aliases.clone(),
                    primary: e.primary,
                    provider: e.provider.clone(),
                    model: e.model.clone(),
                    reference: r,
                }
            })
            .collect();
        let pins = st
            .pins
            .iter()
            .map(|(k, r)| {
                let (channel, chat) = k.split_once(':').unwrap_or((k.as_str(), ""));
                let resolved = cat.resolve(r).ok().map(Entry::reference);
                if resolved.is_none() {
                    problems.push(format!(
                        "{channel} chat {chat} is pinned to `{r}`, which isn't connected, so it's on the default"
                    ));
                }
                PinRow {
                    channel: channel.to_string(),
                    chat: chat.to_string(),
                    reference: r.clone(),
                    resolved,
                }
            })
            .collect();
        let mut last_served: Vec<ServedRow> = st
            .served
            .iter()
            .map(|(s, (r, at))| ServedRow {
                session: s.clone(),
                reference: r.clone(),
                at: at.to_rfc3339(),
            })
            .collect();
        // RFC 3339 in UTC sorts as text.
        last_served.sort_by(|a, b| b.at.cmp(&a.at));
        let routing = super::routing_admin::view_of(&mut st, &cat.routing);
        ModelsView {
            routing,
            models,
            default,
            default_set: cat.default.clone().unwrap_or_default(),
            fallback,
            pins,
            last_served,
            problems,
        }
    }

    /// The model `word` names now, or why it isn't connected.
    pub fn resolve(&self, word: &str) -> Result<Entry, String> {
        self.catalog().resolve(word).cloned()
    }

    /// `[models] default`.
    pub fn set_default(&self, word: &str, by: &str) -> anyhow::Result<Done> {
        let (from, to) = self.edit_config(|t, cat| {
            let e = cat.resolve(word).map_err(anyhow::Error::msg)?;
            let to = stored(cat, word, e);
            let from = cat.default_entry().ok().map(|(e, _)| e.reference());
            put(table(t.root(), &["models"])?, "default", to.as_str());
            Ok((from, to))
        })?;
        self.audit(
            "model.default",
            serde_json::json!({ "from": from, "to": to, "by": by }),
        );
        let now = self.resolve(&to).map(|e| e.reference()).unwrap_or(to);
        Ok(self.done(match from {
            Some(f) if f == now => format!("The default is {now} already."),
            Some(f) => format!("The default is now {now} (was {f})."),
            None => format!("The default is now {now}."),
        }))
    }

    /// Pin a chat to a model: its turns run on it, over the default.
    pub fn pin(&self, channel: &str, chat: &str, word: &str, by: &str) -> anyhow::Result<Done> {
        let cat = self.catalog();
        let e = cat.resolve(word).map_err(anyhow::Error::msg)?;
        let to = stored(&cat, word, e);
        let from = self.edit_pins(|pins| pins.insert(pin_key(channel, chat), to.clone()))?;
        self.audit(
            "model.pin",
            serde_json::json!({ "chat": pin_key(channel, chat), "from": from, "to": to, "by": by }),
        );
        let on = if super::routing::is_tier_ref(&to) && cat.routing.on() {
            format!(
                "the tier `{to}` ({}), moving up from there when a turn fails",
                e.reference()
            )
        } else {
            e.reference()
        };
        Ok(self.done(format!(
            "{channel} chat {chat} now runs on {on}, from its next message."
        )))
    }

    /// Back to the default.
    pub fn unpin(&self, channel: &str, chat: &str, by: &str) -> anyhow::Result<Done> {
        let from = self.edit_pins(|pins| pins.remove(&pin_key(channel, chat)))?;
        let default = self
            .catalog()
            .default_entry()
            .map(|(e, _)| e.reference())
            .unwrap_or_else(|e| e);
        let said = match &from {
            Some(_) => format!("{channel} chat {chat} is back on the default ({default})."),
            None => {
                format!("{channel} chat {chat} wasn't pinned; it's on the default ({default}).")
            }
        };
        if from.is_some() {
            self.audit(
                "model.unpin",
                serde_json::json!({ "chat": pin_key(channel, chat), "from": from, "by": by }),
            );
        }
        Ok(self.done(said))
    }

    /// `[models] fallback`; empty turns fallback off.
    pub fn set_fallback(&self, words: &[String], by: &str) -> anyhow::Result<Done> {
        let (from, to) = self.edit_config(|t, cat| {
            let mut to: Vec<String> = Vec::new();
            for w in words {
                let e = cat.resolve(w).map_err(anyhow::Error::msg)?;
                let r = e.reference();
                if !to.contains(&r) {
                    to.push(r);
                }
            }
            let from = cat.fallback.clone();
            let models = table(t.root(), &["models"])?;
            if to.is_empty() {
                models.remove("fallback");
            } else {
                put(
                    models,
                    "fallback",
                    toml_edit::Array::from_iter(to.iter().map(String::as_str)),
                );
            }
            Ok((from, to))
        })?;
        self.audit(
            "model.fallback",
            serde_json::json!({ "from": from, "to": to, "by": by }),
        );
        Ok(self.done(if to.is_empty() {
            "Fallback is off: a model that's down fails its turn.".into()
        } else {
            format!(
                "When a model stays down after its retries, turns continue on {}.",
                to.join(", then ")
            )
        }))
    }

    /// Another model on a provider already connected (same URL and key).
    pub fn add_model(
        &self,
        provider: &str,
        model: &str,
        alias: Option<&str>,
        by: &str,
    ) -> anyhow::Result<Done> {
        let model = model.trim();
        if model.is_empty() {
            anyhow::bail!("say which model: `ferrule model add {provider}/<model>`");
        }
        let reference = format!("{provider}/{model}");
        self.edit_config(|t, cat| {
            if !cat.entries.iter().any(|e| e.provider == provider) {
                anyhow::bail!(
                    "there's no provider `{provider}` (connected: {}); add it with `ferrule setup`",
                    providers(cat)
                );
            }
            if cat.entries.iter().any(|e| e.reference() == reference) {
                anyhow::bail!("{reference} is connected already");
            }
            let models = table(t.root(), &["providers", provider, "models"])?;
            models.insert(model, Item::Table(Table::new()));
            if let Some(a) = alias {
                check_alias(cat, a)?;
                put(
                    table(t.root(), &["models", "aliases"])?,
                    a,
                    reference.as_str(),
                );
            }
            Ok(())
        })?;
        self.audit(
            "model.add",
            serde_json::json!({ "to": reference, "alias": alias, "by": by }),
        );
        let named = alias.map(|a| format!(", as `{a}`")).unwrap_or_default();
        Ok(self.done(format!(
            "{reference} is connected{named}. `ferrule model test {reference}` makes one call to check it."
        )))
    }

    /// Disconnect a model that isn't a provider's own; it leaves the
    /// fallback list and its aliases go.
    pub fn remove_model(&self, word: &str, by: &str) -> anyhow::Result<Done> {
        let (reference, dropped) = self.edit_config(|t, cat| {
            let e = cat.resolve(word).map_err(anyhow::Error::msg)?.clone();
            let reference = e.reference();
            if e.primary {
                anyhow::bail!(
                    "{reference} is `{}`'s own model; change it, or remove the provider, in `ferrule setup`",
                    e.provider
                );
            }
            if cat
                .default_entry()
                .is_ok_and(|(d, _)| d.reference() == reference)
            {
                anyhow::bail!(
                    "{reference} is the default; pick another first with `ferrule model default <ref>`"
                );
            }
            let mut dropped = Vec::new();
            let fallback: Vec<String> = cat
                .fallback
                .iter()
                .filter(|f| cat.resolve(f).map(Entry::reference).ok() != Some(reference.clone()))
                .cloned()
                .collect();
            if fallback.len() != cat.fallback.len() {
                dropped.push("the fallback list".to_string());
                let models = table(t.root(), &["models"])?;
                if fallback.is_empty() {
                    models.remove("fallback");
                } else {
                    put(
                        models,
                        "fallback",
                        toml_edit::Array::from_iter(fallback.iter().map(String::as_str)),
                    );
                }
            }
            for a in &e.aliases {
                table(t.root(), &["models", "aliases"])?.remove(a);
                dropped.push(format!("the alias `{a}`"));
            }
            let models = table(t.root(), &["providers", &e.provider, "models"])?;
            models.remove(&e.model);
            if models.is_empty() {
                table(t.root(), &["providers", &e.provider])?.remove("models");
            }
            Ok((reference, dropped))
        })?;
        self.audit(
            "model.remove",
            serde_json::json!({ "from": reference, "by": by }),
        );
        let also = if dropped.is_empty() {
            String::new()
        } else {
            format!(" It's gone from {} too.", dropped.join(" and "))
        };
        Ok(self.done(format!("{reference} isn't connected anymore.{also}")))
    }

    /// `[models.aliases]`: `name` for `word`, or with `None`, remove it.
    pub fn set_alias(&self, name: &str, word: Option<&str>, by: &str) -> anyhow::Result<Done> {
        let name = name.trim();
        let (from, to) = self.edit_config(|t, cat| {
            let from = cat.aliases.get(name).cloned();
            let aliases = table(t.root(), &["models", "aliases"])?;
            let Some(word) = word else {
                if from.is_none() {
                    anyhow::bail!("there's no alias `{name}`");
                }
                aliases.remove(name);
                return Ok((from, None));
            };
            check_alias(cat, name)?;
            // The alias's target is another model, not another alias.
            let to = cat.resolve(word).map_err(anyhow::Error::msg)?.reference();
            put(table(t.root(), &["models", "aliases"])?, name, to.as_str());
            Ok((from, Some(to)))
        })?;
        self.audit(
            "model.alias",
            serde_json::json!({ "alias": name, "from": from, "to": to, "by": by }),
        );
        Ok(self.done(match to {
            Some(to) => format!("`{name}` now means {to}."),
            None => format!("The alias `{name}` is gone."),
        }))
    }

    /// One real call to `word`, with what failed said plainly (§4's table).
    pub async fn test(&self, word: &str) -> TestOutcome {
        let e = match self.resolve(word) {
            Ok(e) => e,
            Err(why) => {
                return TestOutcome {
                    reference: word.to_string(),
                    ok: false,
                    said: why,
                }
            }
        };
        test_entry(&e).await
    }

    pub(super) fn done(&self, said: String) -> Done {
        Done {
            said,
            view: self.view(),
        }
    }

    /// Read-modify-write the config as it is now, under its lock, and
    /// refuse an edit that leaves it unparsable or breaks a reference that
    /// worked before.
    pub(crate) fn edit_config<T>(
        &self,
        edit: impl FnOnce(&mut Target, &Catalog) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _lock = filewrite::Lock::take(&self.path)?;
        let mut t = Target::load(self.path.clone())?;
        let before = Catalog::from_config(&t.config()?);
        let out = edit(&mut t, &before)?;
        let after = Catalog::from_config(
            &t.config()
                .context("that change would break the config, so it wasn't saved")?,
        );
        let was = broken_refs(&before);
        if let Some(new) = broken_refs(&after).into_iter().find(|p| !was.contains(p)) {
            anyhow::bail!("that change wasn't saved: {new}");
        }
        t.save()?;
        let mut st = self.state.lock().unwrap();
        st.seen = None;
        self.refresh(&mut st);
        Ok(out)
    }

    /// The same for the pins file; returns what `edit` returned.
    fn edit_pins<T>(
        &self,
        edit: impl FnOnce(&mut BTreeMap<String, String>) -> T,
    ) -> anyhow::Result<T> {
        let Some(path) = &self.pins_path else {
            anyhow::bail!("pins aren't kept in this process");
        };
        let _lock = filewrite::Lock::take(path)?;
        let mut pins = read_pins(path);
        let out = edit(&mut pins);
        filewrite::write(path, serde_json::to_string_pretty(&pins)?.as_bytes())?;
        let mut st = self.state.lock().unwrap();
        st.pins_seen = None;
        self.refresh_pins(&mut st);
        Ok(out)
    }
}

/// What to write for `word`: an alias stays an alias (it follows the
/// alias), anything else becomes `provider/model`.
pub(super) fn stored(cat: &Catalog, word: &str, e: &Entry) -> String {
    // An alias or a tier ref (M25) is kept as written: it follows its
    // model.
    if cat.aliases.contains_key(word.trim()) || super::routing::is_tier_ref(word) {
        word.trim().to_string()
    } else {
        e.reference()
    }
}

fn providers(cat: &Catalog) -> String {
    let mut names: Vec<&str> = cat.entries.iter().map(|e| e.provider.as_str()).collect();
    names.dedup();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// An alias is one word that isn't a provider's name or a `p/m`.
fn check_alias(cat: &Catalog, name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
        anyhow::bail!("an alias is one word without `/`, like `fast`");
    }
    if cat.entries.iter().any(|e| e.provider == name) {
        anyhow::bail!("`{name}` is a provider's name; pick another alias");
    }
    Ok(())
}

/// References in `[models]` that don't resolve.
fn broken_refs(cat: &Catalog) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(d) = &cat.default {
        if let Err(e) = cat.resolve(d) {
            out.push(format!("the default `{d}`: {e}"));
        }
    }
    for f in &cat.fallback {
        if let Err(e) = cat.resolve(f) {
            out.push(format!("fallback `{f}`: {e}"));
        }
    }
    // M25: a tier that stops resolving turns routing off.
    out.extend(cat.routing.problems.iter().cloned());
    for a in cat.aliases.keys() {
        if let Err(e) = cat.resolve(a) {
            out.push(e);
        }
        if cat.entries.iter().any(|e| e.provider == *a) {
            out.push(format!("the alias `{a}` is also a provider's name"));
        }
    }
    out
}

/// One real call: a short prompt, no tools, no retries.
pub async fn test_entry(e: &Entry) -> TestOutcome {
    test_entry_with(e, e.key()).await
}

/// [`test_entry`] with a key that isn't in the environment yet (setup's,
/// just typed in).
pub async fn test_entry_with(e: &Entry, key: Option<String>) -> TestOutcome {
    let reference = e.reference();
    let fail = |said: String| TestOutcome {
        reference: reference.clone(),
        ok: false,
        said,
    };
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        return fail(e.no_key());
    };
    let client = e.client(key);
    let req = CompletionRequest {
        messages: vec![Message::user("Reply with the single word OK.")],
        tools: vec![],
        // Unset: newer OpenAI models refuse `max_tokens`, and that would
        // read as a broken model.
        max_output_tokens: None,
        temperature: None,
        stream: None,
    };
    let start = Instant::now();
    match tokio::time::timeout(TEST_WAIT, client.complete(req)).await {
        Ok(Ok(_)) => TestOutcome {
            reference: reference.clone(),
            ok: true,
            said: format!("answered in {} ms", start.elapsed().as_millis()),
        },
        Ok(Err(err)) => fail(explain(e, &err)),
        Err(_) => fail(format!(
            "no answer from {} in {} s",
            e.base_url,
            TEST_WAIT.as_secs()
        )),
    }
}

/// A provider error as the owner reads it.
pub fn explain(e: &Entry, err: &CoreError) -> String {
    let msg = match err {
        CoreError::Transient { message, .. } | CoreError::Provider(message) => message.clone(),
        other => other.to_string(),
    };
    let status = msg
        .split_once("HTTP ")
        .and_then(|(_, rest)| rest.get(..3))
        .and_then(|c| c.parse::<u16>().ok());
    let lower = msg.to_ascii_lowercase();
    let names_model = lower.contains("model")
        && [
            "not found",
            "does not exist",
            "not exist",
            "unknown",
            "invalid",
            "no such",
            "not supported",
            "unsupported model",
        ]
        .iter()
        .any(|w| lower.contains(w));
    let detail: String = msg.chars().take(200).collect();
    match status {
        Some(401 | 403) => format!(
            "the key was refused (HTTP {}). Check `${}`, or replace it in `ferrule setup`",
            status.unwrap(),
            e.key_env
        ),
        Some(404) => format!(
            "the provider doesn't know the model `{}` (HTTP 404), or {} isn't its API",
            e.model, e.base_url
        ),
        Some(429) => "rate-limited or out of credit (HTTP 429)".into(),
        Some(s) if s >= 500 => format!("the provider is failing right now (HTTP {s})"),
        _ if names_model => format!(
            "the provider doesn't know the model `{}`: {detail}",
            e.model
        ),
        _ if super::no_connection(&msg) => format!(
            "couldn't reach {}. Is the endpoint up? ({detail})",
            e.base_url
        ),
        _ => format!("the call failed: {detail}"),
    }
}
