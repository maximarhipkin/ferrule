//! M22: the models a provider offers, not only the connected ones
//! (docs/m22-dashboard.md §Models). Each connected provider's
//! `{base_url}/models`, plus OpenRouter's public list as a price reference,
//! cached under `<data>/models/catalog/` and refetched at most hourly; the
//! cache answers when the network doesn't. Only OpenRouter's shape carries
//! prices and tool support; any other list gives ids with "price unknown".

use super::{Catalog, Done, Entry, Models};
use crate::config::Config;
use crate::ledger::ProviderPricing;
use crate::setup::{put, table};
use chrono::{DateTime, Utc};
use ferrule_core::LedgerRecord;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const OPENROUTER_MODELS: &str = "https://openrouter.ai/api/v1/models";

/// A list younger than this isn't fetched again.
pub const REFRESH: Duration = Duration::from_secs(60 * 60);

/// Why the page hides models without tool calls unless asked.
pub const TOOLS_REASON: &str =
    "ferrule's agents call tools (files, the web, tasks) on nearly every turn; a model without tool calls can only chat";

/// What `:free` means on OpenRouter.
pub const FREE_CAVEAT: &str =
    "free: a shared pool with tight rate limits (a few calls a minute, a daily cap), slower when busy, prompts may be logged by the host, and it can vanish without notice";

const RECOMMENDED: &str = include_str!("recommended.toml");

/// One model a provider lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Tokens.
    #[serde(default)]
    pub context: Option<u64>,
    /// USD per 1M tokens; `None`: the list doesn't say.
    #[serde(default)]
    pub pricing: Option<ProviderPricing>,
    /// `None`: the list doesn't say.
    #[serde(default)]
    pub tools: Option<bool>,
    #[serde(default)]
    pub free: bool,
}

/// Where a list comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// The connected provider's name, or "openrouter" for the reference.
    pub name: String,
    /// `Some`: a connected provider; a model from it can be added.
    pub provider: Option<String>,
    pub url: String,
    pub key_env: Option<String>,
}

impl Source {
    /// Its cache file's name.
    fn file(&self) -> String {
        let name: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        match self.provider {
            Some(_) => format!("{name}.json"),
            None => format!("reference-{name}.json"),
        }
    }

    /// OpenRouter's list: a provider at openrouter.ai, or the reference
    /// source (OpenRouter's, or the mirror `[models] catalog_url` names).
    pub fn is_openrouter(&self) -> bool {
        self.url.contains("openrouter.ai") || self.provider.is_none()
    }
}

/// Every provider's list, and OpenRouter's public one as a price reference
/// when no connected provider is OpenRouter (unless `[models] catalog_url`
/// is "").
pub fn sources(cfg: &Config) -> Vec<Source> {
    let mut out: Vec<Source> = cfg
        .providers
        .iter()
        .map(|(name, p)| Source {
            name: name.clone(),
            provider: Some(name.clone()),
            url: format!("{}/models", p.base_url.trim_end_matches('/')),
            key_env: Some(p.api_key_env.clone()),
        })
        .collect();
    let reference = cfg
        .models
        .catalog_url
        .clone()
        .unwrap_or_else(|| OPENROUTER_MODELS.to_string());
    if !reference.trim().is_empty() && !out.iter().any(Source::is_openrouter) {
        out.push(Source {
            name: "openrouter".into(),
            provider: None,
            url: reference,
            key_env: None,
        });
    }
    out
}

/// `<data>/models/catalog`.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::data_dir()?.join("models").join("catalog"))
}

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    /// Unix seconds.
    fetched_at: i64,
    url: String,
    models: Vec<Listed>,
}

/// One source's list, as fetched or cached.
#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub source: String,
    pub provider: Option<String>,
    /// "live", "cache" or "none".
    pub from: &'static str,
    /// Unix seconds.
    pub fetched_at: Option<i64>,
    /// Why the list couldn't be fetched now (the cache, if any, answered).
    pub error: Option<String>,
    pub models: Vec<Listed>,
}

impl Listing {
    pub fn find(&self, id: &str) -> Option<&Listed> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// `src`'s list: the cache when it's younger than [`REFRESH`] (or `force`
/// is false and the network fails), else a fresh fetch.
pub async fn load(src: &Source, dir: &Path, force: bool) -> Listing {
    let path = dir.join(src.file());
    let cached: Option<Cached> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|c: &Cached| c.url == src.url);
    let fresh = cached.as_ref().is_some_and(|c| {
        let age = Utc::now().timestamp() - c.fetched_at;
        (0..REFRESH.as_secs() as i64).contains(&age)
    });
    let listing = |from, c: Cached, error| Listing {
        source: src.name.clone(),
        provider: src.provider.clone(),
        from,
        fetched_at: Some(c.fetched_at),
        error,
        models: c.models,
    };
    if fresh && !force {
        return listing("cache", cached.unwrap(), None);
    }
    match fetch(src).await {
        Ok(models) => {
            let c = Cached {
                fetched_at: Utc::now().timestamp(),
                url: src.url.clone(),
                models,
            };
            let _ = std::fs::create_dir_all(dir);
            if let Ok(bytes) = serde_json::to_vec(&c) {
                if let Err(e) = crate::filewrite::write(&path, &bytes) {
                    tracing::warn!("model catalog cache {}: {e:#}", path.display());
                }
            }
            listing("live", c, None)
        }
        Err(e) => match cached {
            Some(c) => listing("cache", c, Some(e)),
            None => Listing {
                source: src.name.clone(),
                provider: src.provider.clone(),
                from: "none",
                fetched_at: None,
                error: Some(e),
                models: Vec::new(),
            },
        },
    }
}

/// Every source's list, fetched side by side.
pub async fn load_all(sources: &[Source], dir: &Path, force: bool) -> Vec<Listing> {
    let handles: Vec<_> = sources
        .iter()
        .map(|s| {
            let (s, dir) = (s.clone(), dir.to_path_buf());
            tokio::spawn(async move { load(&s, &dir, force).await })
        })
        .collect();
    let mut out = Vec::new();
    for (h, s) in handles.into_iter().zip(sources) {
        out.push(h.await.unwrap_or_else(|e| Listing {
            source: s.name.clone(),
            provider: s.provider.clone(),
            from: "none",
            fetched_at: None,
            error: Some(e.to_string()),
            models: Vec::new(),
        }));
    }
    out
}

async fn fetch(src: &Source) -> Result<Vec<Listed>, String> {
    let mut req = crate::probe::client().get(&src.url);
    if let Some(key) = src
        .key_env
        .as_deref()
        .and_then(|k| std::env::var(k).ok())
        .filter(|k| !k.is_empty())
    {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!(
            "{} answered HTTP {}",
            src.name,
            resp.status().as_u16()
        ));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("{}: not a model list ({})", src.name, e.without_url()))?;
    parse(&body).ok_or_else(|| format!("{}: not a model list", src.name))
}

/// OpenAI's `{"data": [{"id": …}]}`, with OpenRouter's prices (USD per
/// token, as strings), `context_length` and `supported_parameters` when
/// there.
pub fn parse(body: &serde_json::Value) -> Option<Vec<Listed>> {
    let data = body.get("data")?.as_array()?;
    let per_m = |v: Option<&serde_json::Value>| -> Option<f64> {
        let v = v?;
        let per_token = match v {
            serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
            serde_json::Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        // "-1" is OpenRouter's "varies" (a router, not a model).
        (per_token >= 0.0).then(|| (per_token * 1e12).round() / 1e6)
    };
    let mut out: Vec<Listed> = data
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let p = m.get("pricing");
            let pricing = p.and_then(|p| {
                let input = per_m(p.get("prompt"))?;
                Some(ProviderPricing {
                    input,
                    cached_input: per_m(p.get("input_cache_read")).unwrap_or(input),
                    output: per_m(p.get("completion"))?,
                })
            });
            let tools = m
                .get("supported_parameters")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().any(|x| x.as_str() == Some("tools")));
            Some(Listed {
                free: id.ends_with(":free"),
                name: m.get("name").and_then(|n| n.as_str()).map(str::to_string),
                context: m.get("context_length").and_then(|c| c.as_u64()),
                pricing,
                tools,
                id,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

/// How the page asks for a list.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub search: Option<String>,
    /// Also the models that can't call tools.
    pub all: bool,
    /// "in" (default), "out", "context" or "name".
    pub sort: String,
}

/// One row of the filtered list.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub source: String,
    /// Where it can be added; `None`: only a price reference.
    pub provider: Option<String>,
    #[serde(flatten)]
    pub model: Listed,
    /// `provider/model` when it's connected already.
    pub connected: Option<String>,
    pub caveat: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Filtered {
    pub rows: Vec<Row>,
    /// Hidden because they can't call tools.
    pub hidden_no_tools: usize,
    pub tools_reason: &'static str,
    pub sources: Vec<SourceState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceState {
    pub source: String,
    pub from: &'static str,
    /// Unix seconds.
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
    pub models: usize,
}

pub fn filter(listings: &[Listing], cat: &Catalog, q: &Query) -> Filtered {
    let needle = q.search.as_deref().unwrap_or("").trim().to_lowercase();
    let mut hidden = 0;
    let mut rows = Vec::new();
    for l in listings {
        for m in &l.models {
            let hay = format!("{} {}", m.id, m.name.as_deref().unwrap_or("")).to_lowercase();
            if !needle.is_empty() && !needle.split_whitespace().all(|w| hay.contains(w)) {
                continue;
            }
            if !q.all && m.tools == Some(false) {
                hidden += 1;
                continue;
            }
            rows.push(Row {
                source: l.source.clone(),
                provider: l.provider.clone(),
                connected: connected(cat, l.provider.as_deref(), &m.id),
                caveat: m.free.then_some(FREE_CAVEAT),
                model: m.clone(),
            });
        }
    }
    let unknown = f64::INFINITY;
    let key = |r: &Row, out: bool| {
        r.model
            .pricing
            .map(|p| if out { p.output } else { p.input })
            .unwrap_or(unknown)
    };
    match q.sort.as_str() {
        "out" => rows.sort_by(|a, b| key(a, true).total_cmp(&key(b, true))),
        "context" => rows.sort_by_key(|r| std::cmp::Reverse(r.model.context)),
        "name" => rows.sort_by(|a, b| a.model.id.cmp(&b.model.id)),
        _ => rows.sort_by(|a, b| key(a, false).total_cmp(&key(b, false))),
    }
    Filtered {
        rows,
        hidden_no_tools: hidden,
        tools_reason: TOOLS_REASON,
        sources: states(listings),
    }
}

pub fn states(listings: &[Listing]) -> Vec<SourceState> {
    listings
        .iter()
        .map(|l| SourceState {
            source: l.source.clone(),
            from: l.from,
            fetched_at: l.fetched_at,
            error: l.error.clone(),
            models: l.models.len(),
        })
        .collect()
}

fn connected(cat: &Catalog, provider: Option<&str>, id: &str) -> Option<String> {
    let p = provider?;
    cat.entries
        .iter()
        .find(|e| e.provider == p && e.model == id)
        .map(Entry::reference)
}

/// This machine's sources and their lists.
pub async fn listings(force: bool) -> anyhow::Result<(Vec<Source>, Vec<Listing>)> {
    let (cfg, _) = Config::load()?;
    Ok(listings_at(&cfg, &cache_dir()?, force).await)
}

/// `cfg`'s sources and their lists, cached in `dir`.
pub async fn listings_at(cfg: &Config, dir: &Path, force: bool) -> (Vec<Source>, Vec<Listing>) {
    let sources = sources(cfg);
    let listings = load_all(&sources, dir, force).await;
    (sources, listings)
}

/// [`recommend`] with this machine's lists and ledger.
pub async fn recommended(models: &Models, force: bool) -> anyhow::Result<Recommended> {
    let (sources, listings) = listings(force).await?;
    let ledger = crate::ledger::ledger_path().ok();
    Ok(recommended_with(
        models,
        &sources,
        &listings,
        ledger.as_deref(),
    ))
}

/// [`recommend`] with these lists and the last 30 days of `ledger`.
pub fn recommended_with(
    models: &Models,
    sources: &[Source],
    listings: &[Listing],
    ledger: Option<&Path>,
) -> Recommended {
    let usage = ledger
        .and_then(|p| {
            crate::ledger::read_records(p, Some(Utc::now() - chrono::Duration::days(30))).ok()
        })
        .map(|(r, _)| Usage::from_records(&r, Utc::now()))
        .unwrap_or_default();
    let or = openrouter(listings, sources);
    recommend(
        or.as_ref().map(|(l, _)| l),
        or.as_ref().and_then(|(_, p)| p.clone()),
        &models.catalog(),
        usage,
    )
}

fn money(p: Option<ProviderPricing>) -> String {
    match p {
        Some(p) => format!("${}/${}", p.input, p.output),
        None => "price unknown".into(),
    }
}

/// The filtered list as text.
pub fn render(f: &Filtered) -> String {
    let mut out = String::new();
    for s in &f.sources {
        let note = match (&s.error, s.from) {
            (Some(e), "cache") => format!("cached copy; now: {e}"),
            (Some(e), _) => e.clone(),
            (None, from) => from.to_string(),
        };
        out.push_str(&format!("{}: {} models ({note})\n", s.source, s.models));
    }
    out.push_str("\nper 1M in/out · context · model\n");
    for r in &f.rows {
        let tools = match r.model.tools {
            Some(true) => "",
            Some(false) => " · no tools",
            None => " · tools unknown",
        };
        let ctx = r
            .model
            .context
            .map(|c| format!("{}k", c / 1000))
            .unwrap_or_else(|| "?".into());
        let mark = match &r.connected {
            Some(_) => " · connected",
            None if r.provider.is_none() => " · reference",
            None => "",
        };
        out.push_str(&format!(
            "{} · {ctx} · {}{tools}{mark}{}\n",
            money(r.model.pricing),
            r.model.id,
            if r.model.free {
                " · free (limits apply)"
            } else {
                ""
            }
        ));
    }
    if f.hidden_no_tools > 0 {
        out.push_str(&format!(
            "\n{} without tool calls hidden: {}\n",
            f.hidden_no_tools, f.tools_reason
        ));
    }
    out
}

pub fn render_recommended(r: &Recommended) -> String {
    let mut out = format!("Recommended (checked {}):\n", r.checked);
    for t in &r.tiers {
        out.push_str(&format!("\n{}:\n", t.name));
        for p in &t.picks {
            let month = p
                .monthly_usd
                .map(|m| format!(" · ~${m:.2}/month at your usage"))
                .unwrap_or_default();
            let on = p
                .connected
                .as_ref()
                .map(|c| format!(" · connected as {c}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {} · {} per 1M in/out{month}{on}\n  {}\n",
                p.id,
                money(p.pricing),
                p.why
            ));
        }
    }
    if r.usage.days > 0 {
        out.push_str(&format!(
            "\nEstimates use your last {} day(s) of calls, scaled to 30.\n",
            r.usage.days
        ));
    }
    if r.provider.is_none() {
        out.push_str("OpenRouter isn't connected; `ferrule setup` → Model provider → OpenRouter to use these.\n");
    }
    for m in &r.missing {
        out.push_str(&format!("hidden: {m}\n"));
    }
    out
}

/// The OpenRouter list: a connected OpenRouter provider's, else the
/// reference one.
pub fn openrouter(listings: &[Listing], sources: &[Source]) -> Option<(Listing, Option<String>)> {
    sources
        .iter()
        .zip(listings)
        .find(|(s, l)| s.is_openrouter() && !l.models.is_empty())
        .or_else(|| {
            sources
                .iter()
                .zip(listings)
                .find(|(s, _)| s.is_openrouter())
        })
        .map(|(s, l)| (l.clone(), s.provider.clone()))
}

#[derive(Debug, Deserialize)]
struct Curated {
    checked: String,
    #[serde(default)]
    value: Vec<Pick>,
    #[serde(default)]
    strongest: Vec<Pick>,
    #[serde(default)]
    free: Vec<Pick>,
}

#[derive(Debug, Clone, Deserialize)]
struct Pick {
    id: String,
    why: String,
}

/// The last 30 days of this machine's calls, for a monthly estimate.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub days: u32,
    pub uncached_input: u64,
    pub cached_input: u64,
    pub output: u64,
}

impl Usage {
    /// `records` from the ledger; eval runs don't count.
    pub fn from_records(records: &[LedgerRecord], now: DateTime<Utc>) -> Self {
        let since = now - chrono::Duration::days(30);
        let mut u = Usage::default();
        let mut first: Option<DateTime<Utc>> = None;
        for r in records {
            if r.eval.is_some() || r.call_kind == "eval_result" {
                continue;
            }
            let Ok(at) = DateTime::parse_from_rfc3339(&r.timestamp) else {
                continue;
            };
            let at = at.with_timezone(&Utc);
            if at < since || at > now {
                continue;
            }
            first = Some(first.map_or(at, |f| f.min(at)));
            u.uncached_input += r.input_tokens.saturating_sub(r.cached_input_tokens);
            u.cached_input += r.cached_input_tokens;
            u.output += r.output_tokens;
        }
        u.days = first.map_or(0, |f| ((now - f).num_hours() / 24 + 1).clamp(1, 30) as u32);
        u
    }

    /// USD for 30 days at this pace.
    pub fn monthly(&self, p: &ProviderPricing) -> Option<f64> {
        if self.days == 0 {
            return None;
        }
        let usd = (self.uncached_input as f64 * p.input
            + self.cached_input as f64 * p.cached_input
            + self.output as f64 * p.output)
            / 1_000_000.0;
        Some((usd * 30.0 / self.days as f64 * 100.0).round() / 100.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommended {
    /// When the list was last checked by hand.
    pub checked: String,
    pub tiers: Vec<Tier>,
    /// Entries the live list no longer has, or has without tool calls;
    /// hidden from the tiers.
    pub missing: Vec<String>,
    pub usage: Usage,
    /// Where they can be added; `None`: OpenRouter isn't connected.
    pub provider: Option<String>,
    pub from: &'static str,
    /// Unix seconds.
    pub fetched_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tier {
    pub name: &'static str,
    pub picks: Vec<Recommendation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    pub id: String,
    pub why: String,
    pub pricing: Option<ProviderPricing>,
    pub context: Option<u64>,
    pub free: bool,
    pub caveat: Option<&'static str>,
    /// At the last 30 days' pace.
    pub monthly_usd: Option<f64>,
    pub connected: Option<String>,
}

/// The curated list, each entry re-checked against `live` (OpenRouter's).
pub fn recommend(
    live: Option<&Listing>,
    provider: Option<String>,
    cat: &Catalog,
    usage: Usage,
) -> Recommended {
    let curated: Curated = toml::from_str(RECOMMENDED).expect("recommended.toml parses");
    let mut missing = Vec::new();
    let mut tiers = Vec::new();
    for (name, picks) in [
        ("value", curated.value),
        ("strongest", curated.strongest),
        ("free", curated.free),
    ] {
        let mut out = Vec::new();
        for p in picks {
            let Some(m) = live.and_then(|l| l.find(&p.id)) else {
                missing.push(format!("{} isn't in OpenRouter's list anymore", p.id));
                continue;
            };
            if m.tools == Some(false) {
                missing.push(format!("{} can't call tools anymore", p.id));
                continue;
            }
            out.push(Recommendation {
                monthly_usd: m.pricing.as_ref().and_then(|pr| usage.monthly(pr)),
                connected: connected(cat, provider.as_deref(), &p.id),
                caveat: m.free.then_some(FREE_CAVEAT),
                free: m.free,
                pricing: m.pricing,
                context: m.context,
                id: p.id,
                why: p.why,
            });
        }
        tiers.push(Tier { name, picks: out });
    }
    if live.is_none_or(|l| l.models.is_empty()) {
        missing = vec!["OpenRouter's list couldn't be read, and there's no cached copy".into()];
    }
    Recommended {
        checked: curated.checked,
        tiers,
        missing,
        usage,
        provider,
        from: live.map_or("none", |l| l.from),
        fetched_at: live.and_then(|l| l.fetched_at),
    }
}

/// The prices `e` would get from `listings`: its own provider's list,
/// else the OpenRouter reference by the same id.
pub fn price_for<'a>(e: &Entry, listings: &'a [Listing]) -> Option<(&'a Listing, ProviderPricing)> {
    let own = listings
        .iter()
        .filter(|l| l.provider.as_deref() == Some(e.provider.as_str()))
        .chain(listings.iter().filter(|l| l.provider.is_none()));
    for l in own {
        if let Some(p) = l.find(&e.model).and_then(|m| m.pricing) {
            return Some((l, p));
        }
    }
    None
}

fn source_note(l: &Listing) -> String {
    let at = l
        .fetched_at
        .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
        .unwrap_or_else(Utc::now);
    let day = at.format("%Y-%m-%d");
    format!("{} catalog {day}", l.source)
}

/// What [`Models::fill_prices`] did.
#[derive(Debug, Clone, Serialize)]
pub struct Filled {
    pub filled: Vec<String>,
    /// Connected models still without prices, and why.
    pub unknown: Vec<String>,
    pub said: String,
}

impl Models {
    /// Prices from `listings` for every connected model that has none. A
    /// price set by hand is never touched; one ferrule wrote before is
    /// refreshed.
    pub fn fill_prices(&self, listings: &[Listing], by: &str) -> anyhow::Result<Filled> {
        let (filled, unknown) = self.edit_config(|t, cat| {
            let mut filled = Vec::new();
            let mut unknown = Vec::new();
            for e in &cat.entries {
                if e.pricing.is_some() && e.price_source.is_none() {
                    continue;
                }
                let Some((l, p)) = price_for(e, listings) else {
                    if e.pricing.is_none() {
                        unknown.push(format!("{}: no list has its price", e.reference()));
                    }
                    continue;
                };
                if e.pricing == Some(p) {
                    continue;
                }
                write_prices(t, e, &p, &source_note(l))?;
                filled.push(e.reference());
            }
            Ok((filled, unknown))
        })?;
        if !filled.is_empty() {
            self.audit(
                "model.prices",
                serde_json::json!({ "filled": filled, "by": by }),
            );
        }
        let said = match (filled.is_empty(), unknown.is_empty()) {
            (true, true) => "Every connected model has prices.".to_string(),
            (false, _) => format!("Prices filled for {}.", filled.join(", ")),
            (true, false) => "No prices could be filled.".to_string(),
        };
        let said = if unknown.is_empty() {
            said
        } else {
            format!("{said} Still unpriced: {}.", unknown.join("; "))
        };
        Ok(Filled {
            filled,
            unknown,
            said,
        })
    }

    /// One tap from the catalog: connect `provider`'s `id` and make it the
    /// default (after one real call answers), a fallback, or just a
    /// connected model. The list's prices go in with it, unless it has
    /// hand-set ones.
    pub async fn add_from_catalog(
        &self,
        provider: &str,
        id: &str,
        as_what: &str,
        listing: Option<&Listing>,
        by: &str,
    ) -> anyhow::Result<Done> {
        let cat = self.catalog();
        let reference = format!("{provider}/{id}");
        let existing = cat
            .entries
            .iter()
            .find(|e| e.reference() == reference)
            .cloned();
        let Some(base) = cat
            .entries
            .iter()
            .find(|e| e.provider == provider && e.primary)
            .cloned()
        else {
            anyhow::bail!("there's no provider `{provider}`; connect it with `ferrule setup`");
        };
        if as_what == "default" {
            let mut probe = existing.clone().unwrap_or_else(|| Entry {
                model: id.to_string(),
                primary: false,
                aliases: Vec::new(),
                price_source: None,
                ..base.clone()
            });
            probe.model = id.to_string();
            let t = super::test_entry(&probe).await;
            if !t.ok {
                anyhow::bail!(
                    "{reference} didn't answer a test call, so the default is unchanged: {}",
                    t.said
                );
            }
        }
        let mut said = Vec::new();
        if existing.is_none() {
            said.push(self.add_model(provider, id, None, by)?.said);
        }
        if let Some(p) = listing.and_then(|l| l.find(id)).and_then(|m| m.pricing) {
            let note = source_note(listing.unwrap());
            let wrote = self.edit_config(|t, cat| {
                let e = cat
                    .entries
                    .iter()
                    .find(|e| e.reference() == reference)
                    .ok_or_else(|| anyhow::anyhow!("{reference} isn't connected"))?;
                if e.pricing.is_some() && e.price_source.is_none() {
                    return Ok(false);
                }
                write_prices(t, e, &p, &note)?;
                Ok(true)
            })?;
            if wrote {
                said.push(format!(
                    "Its prices (${}/${} per 1M in/out) are from {note}.",
                    p.input, p.output
                ));
            }
        }
        let done = match as_what {
            "default" => self.set_default(&reference, by)?,
            "fallback" => {
                let mut list: Vec<String> = cat.fallback.clone();
                if !list.iter().any(|f| {
                    cat.resolve(f).map(Entry::reference).ok().as_deref() == Some(&reference)
                }) {
                    list.push(reference.clone());
                }
                self.set_fallback(&list, by)?
            }
            _ => Done {
                said: String::new(),
                view: self.view(),
            },
        };
        if !done.said.is_empty() {
            said.push(done.said);
        }
        Ok(Done {
            said: said.join(" "),
            view: done.view,
        })
    }
}

/// `[providers.P.models."M"]`'s three prices and where they came from.
fn write_prices(
    t: &mut crate::setup::Target,
    e: &Entry,
    p: &ProviderPricing,
    note: &str,
) -> anyhow::Result<()> {
    let tbl = table(t.root(), &["providers", &e.provider, "models", &e.model])?;
    put(tbl, "price_input_per_mtok", p.input);
    put(tbl, "price_cached_input_per_mtok", p.cached_input);
    put(tbl, "price_output_per_mtok", p.output);
    put(tbl, "price_source", note);
    Ok(())
}

/// Connected models the ledger can't cost, for `ferrule doctor` and the
/// page: no prices at all, or all three zero on a model that isn't free.
pub fn unpriced(cat: &Catalog) -> Vec<String> {
    cat.entries
        .iter()
        .filter_map(|e| match e.pricing {
            None => Some(format!(
                "{} has no prices, so its cost shows as unknown and the spending caps can't count it",
                e.reference()
            )),
            Some(p)
                if p.input == 0.0 && p.output == 0.0 && !e.model.ends_with(":free") =>
            {
                Some(format!(
                    "{} is priced at $0, so the spending caps never fire for it",
                    e.reference()
                ))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../tests/fixtures/openrouter-models.json")).unwrap()
    }

    fn listing() -> Listing {
        Listing {
            source: "openrouter".into(),
            provider: None,
            from: "live",
            fetched_at: Some(Utc::now().timestamp()),
            error: None,
            models: parse(&fixture()).unwrap(),
        }
    }

    #[test]
    fn parses_prices_per_million_tools_and_free() {
        let l = listing();
        let flash = l.find("deepseek/deepseek-v4.1-flash").unwrap();
        let p = flash.pricing.unwrap();
        assert_eq!((p.input, p.output), (0.15, 0.6));
        assert_eq!(flash.tools, Some(true));
        assert!(!flash.free);
        assert!(l.find("qwen/qwen3.8-27b:free").unwrap().free);
        assert_eq!(
            l.find("google/gemini-3.1-flash-image").unwrap().tools,
            Some(false)
        );
        // A plain OpenAI list: ids only.
        let plain =
            parse(&serde_json::json!({"data": [{"id": "gpt-x", "object": "model"}]})).unwrap();
        assert_eq!(plain[0].pricing, None);
        assert_eq!(plain[0].tools, None);
    }

    #[test]
    fn the_tool_filter_hides_and_counts() {
        let l = vec![listing()];
        let cat = Catalog::default();
        let f = filter(&l, &cat, &Query::default());
        assert!(f.rows.iter().all(|r| r.model.tools != Some(false)));
        assert_eq!(f.hidden_no_tools, 2);
        let all = filter(
            &l,
            &cat,
            &Query {
                all: true,
                ..Query::default()
            },
        );
        assert_eq!(all.rows.len(), f.rows.len() + 2);
        let free = filter(
            &l,
            &cat,
            &Query {
                search: Some("FREE qwen".into()),
                ..Query::default()
            },
        );
        assert_eq!(free.rows.len(), 1);
        assert_eq!(free.rows[0].caveat, Some(FREE_CAVEAT));
        // Cheapest input first; frees lead.
        assert_eq!(f.rows[0].model.pricing.unwrap().input, 0.0);
    }

    #[test]
    fn a_missing_recommendation_is_hidden_and_reported() {
        let l = listing();
        let r = recommend(Some(&l), None, &Catalog::default(), Usage::default());
        let ids: Vec<&str> = r
            .tiers
            .iter()
            .flat_map(|t| t.picks.iter().map(|p| p.id.as_str()))
            .collect();
        assert!(!ids.contains(&"minimax/minimax-m3"));
        assert!(r.missing.iter().any(|m| m.contains("minimax/minimax-m3")));
        assert!(ids.contains(&"deepseek/deepseek-v4.1-flash"));
        assert!(r.tiers[2]
            .picks
            .iter()
            .all(|p| p.free && p.caveat.is_some()));
    }

    #[test]
    fn the_monthly_estimate_scales_the_ledger() {
        let now = Utc::now();
        let rec = |days_ago: i64, input: u64, cached: u64, output: u64| LedgerRecord {
            timestamp: (now - chrono::Duration::days(days_ago)).to_rfc3339(),
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            ..serde_json::from_value(serde_json::json!({
                "timestamp": "", "session_id": "s", "task_shape": "chat",
                "provider": "p", "model": "m", "iteration": 0,
                "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
                "tool_calls": 0, "latency_ms": 1, "outcome": "ok"
            }))
            .unwrap()
        };
        // 10 days: 2M uncached, 1M cached, 1M out; and one too old.
        let records = vec![
            rec(9, 2_000_000, 0, 500_000),
            rec(1, 1_000_000, 1_000_000, 500_000),
            rec(40, 9_000_000, 0, 0),
        ];
        let u = Usage::from_records(&records, now);
        assert_eq!(u.days, 10);
        assert_eq!(
            (u.uncached_input, u.cached_input, u.output),
            (2_000_000, 1_000_000, 1_000_000)
        );
        let p = ProviderPricing {
            input: 1.0,
            cached_input: 0.1,
            output: 2.0,
        };
        // (2 + 0.1 + 2) USD over 10 days → 12.30 for 30.
        assert_eq!(u.monthly(&p), Some(12.3));
    }

    #[tokio::test]
    async fn the_cache_answers_offline() {
        let dir = tempfile::tempdir().unwrap();
        let body = fixture().to_string();
        let server = crate::dashboard::testing::serve_once(body).await;
        let src = Source {
            name: "openrouter".into(),
            provider: None,
            url: format!("http://{server}/v1/models"),
            key_env: None,
        };
        let live = load(&src, dir.path(), true).await;
        assert_eq!(live.from, "live");
        assert_eq!(live.models.len(), 15);
        // The server is gone now; a forced refresh falls back to the cache.
        let offline = load(&src, dir.path(), true).await;
        assert_eq!(offline.from, "cache");
        assert!(offline.error.is_some());
        assert_eq!(offline.models, live.models);
        // Fresh enough: no fetch at all.
        assert_eq!(load(&src, dir.path(), false).await.error, None);
    }

    #[test]
    fn unpriced_models_are_named() {
        let mut cat = Catalog::default();
        let e = |model: &str, pricing| Entry {
            provider: "or".into(),
            model: model.into(),
            primary: false,
            base_url: String::new(),
            key_env: "K".into(),
            profile: "generic".into(),
            context_window: None,
            pricing,
            price_source: None,
            aliases: vec![],
        };
        let zero = Some(ProviderPricing {
            input: 0.0,
            cached_input: 0.0,
            output: 0.0,
        });
        cat.entries = vec![e("a", None), e("b", zero), e("c:free", zero)];
        let u = unpriced(&cat);
        assert_eq!(u.len(), 2);
        assert!(u[0].contains("or/a") && u[1].contains("or/b"));
    }
}
