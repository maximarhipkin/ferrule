//! M25's owner surfaces over routing (docs/m25-routing.md §7): what
//! `/model`, `ferrule model route`, doctor and the dashboard show, the
//! set/unset edits (audited like every M21 edit), the escalations and spend
//! per tier the ledger holds, and a cheap/strong pair to suggest.

use super::admin::{stored, Done};
use super::catalog::{self, Listing, Recommended};
use super::routing::{is_tier_ref, spent, Routing};
use super::*;
use crate::setup::{put, table};
use ferrule_core::LedgerRecord;

/// `[routing]` as it is now.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingView {
    pub enabled: bool,
    /// Enabled, with two tiers that resolve: the default is routed.
    pub on: bool,
    /// The tiers that resolve, cheap first.
    pub tiers: Vec<TierRow>,
    pub de_escalate: bool,
    /// Which signals move a turn up.
    pub triggers: serde_json::Value,
    pub strong_daily_usd: Option<f64>,
    /// Today's (UTC) spend above tier 0.
    pub strong_spent_today: f64,
    /// A tier that doesn't resolve, too few tiers.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TierRow {
    /// As written in `tiers`.
    pub name: String,
    pub reference: String,
    pub pricing: Option<ProviderPricing>,
    pub key_present: bool,
    pub key_env: String,
    pub context_window: usize,
}

pub(super) fn view_of(st: &mut State, r: &Routing) -> RoutingView {
    let p = &r.policy;
    RoutingView {
        enabled: r.enabled,
        on: r.on(),
        tiers: r
            .tiers
            .iter()
            .map(|t| TierRow {
                name: t.name.clone(),
                reference: t.entry.reference(),
                pricing: t.entry.pricing,
                key_present: t.entry.key().is_some_and(|k| !k.is_empty()),
                key_env: t.entry.key_env.clone(),
                context_window: t.entry.harness().context_window,
            })
            .collect(),
        de_escalate: p.de_escalate,
        triggers: serde_json::json!({
            "call_failed": p.call_failed,
            "tool_errors": p.tool_errors,
            "checks": p.checks,
            "stop_hooks": p.stop_hooks,
            "no_progress": p.no_progress,
            "watchdog": p.watchdog,
        }),
        strong_daily_usd: r.strong_daily_usd,
        strong_spent_today: if r.usable() { spent(st, r) } else { 0.0 },
        problems: r.problems.clone(),
    }
}

impl Models {
    /// `[routing]`: turn it on over `words` (cheap first, two or more),
    /// and optionally set `de_escalate` and the daily cap (`Some(None)`
    /// removes it).
    pub fn set_routing(
        &self,
        words: &[String],
        de_escalate: Option<bool>,
        cap: Option<Option<f64>>,
        by: &str,
    ) -> anyhow::Result<Done> {
        let (from, to) = self.edit_config(|t, cat| {
            if words.len() < 2 {
                anyhow::bail!(
                    "routing needs two tiers or more, cheap first: `ferrule model route set <cheap> <strong>`"
                );
            }
            let mut refs: Vec<String> = Vec::new();
            let mut written: Vec<String> = Vec::new();
            for w in words {
                let w = w.trim();
                if is_tier_ref(w) {
                    anyhow::bail!("`{w}` is a tier; a tier names a model");
                }
                let e = cat.resolve(w).map_err(anyhow::Error::msg)?;
                if refs.contains(&e.reference()) {
                    anyhow::bail!("{} is named twice", e.reference());
                }
                refs.push(e.reference());
                written.push(stored(cat, w, e));
            }
            if let Some(Some(c)) = cap {
                if !(c.is_finite() && c > 0.0) {
                    anyhow::bail!("the cap is dollars a day above the cheap tier, like 2.0");
                }
            }
            let from = serde_json::json!({
                "enabled": cat.routing.enabled,
                "tiers": cat.routing.names(),
                "de_escalate": cat.routing.policy.de_escalate,
                "strong_daily_usd": cat.routing.strong_daily_usd,
            });
            let r = table(t.root(), &["routing"])?;
            put(r, "enabled", true);
            put(
                r,
                "tiers",
                toml_edit::Array::from_iter(written.iter().map(String::as_str)),
            );
            if let Some(d) = de_escalate {
                put(r, "de_escalate", d);
            }
            match cap {
                Some(Some(c)) => put(r, "strong_daily_usd", c),
                Some(None) => {
                    r.remove("strong_daily_usd");
                }
                None => {}
            }
            Ok((from, written))
        })?;
        let r = self.catalog().routing.clone();
        self.audit(
            "routing.set",
            serde_json::json!({
                "from": from,
                "to": {
                    "tiers": to,
                    "de_escalate": r.policy.de_escalate,
                    "strong_daily_usd": r.strong_daily_usd,
                },
                "by": by,
            }),
        );
        Ok(self.done(on_text(&r)))
    }

    /// `[routing] enabled = false`. The tiers stay, so a `tier:` ref still
    /// names its model.
    pub fn unset_routing(&self, by: &str) -> anyhow::Result<Done> {
        let was = self.edit_config(|t, cat| {
            if !cat.routing.enabled {
                return Ok(false);
            }
            put(table(t.root(), &["routing"])?, "enabled", false);
            Ok(true)
        })?;
        if was {
            self.audit("routing.unset", serde_json::json!({ "by": by }));
        }
        let default = self
            .catalog()
            .default_entry()
            .map(|(e, _)| e.reference())
            .unwrap_or_else(|e| e);
        Ok(self.done(if was {
            format!("Routing is off: turns run on the default ({default}).")
        } else {
            format!("Routing was off already; turns run on the default ({default}).")
        }))
    }
}

/// What turning routing on did, for the owner.
fn on_text(r: &Routing) -> String {
    let Some((cheap, rest)) = r.tiers.split_first() else {
        return "Routing is on, but has no tiers that resolve.".into();
    };
    let tier = |t: &super::routing::RouteTier| {
        format!(
            "{} ({}{})",
            t.name,
            t.entry.reference(),
            price_note(t.entry.pricing)
        )
    };
    let up: Vec<String> = rest.iter().map(tier).collect();
    let back = if r.policy.de_escalate {
        " The next turn starts cheap again."
    } else {
        " A turn that moved up stays up (de_escalate = false)."
    };
    let cap = r
        .strong_daily_usd
        .map(|c| format!(" Spend above the cheap tier stops at ${c:.2} a day."))
        .unwrap_or_default();
    format!(
        "Routing is on: turns start on {} and move up to {} when a call fails for good, a check or Stop hook sends the answer back, tool calls keep failing or the turn stops making progress.{back}{cap}",
        tier(cheap),
        up.join(", then ")
    )
}

fn price_note(p: Option<ProviderPricing>) -> String {
    p.map(|p| format!(", ${}/${} per M in/out", p.input, p.output))
        .unwrap_or_default()
}

/// The routing lines of `/model` and `ferrule model list`.
pub fn render(r: &RoutingView) -> String {
    if r.tiers.is_empty() && r.problems.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\nRouting: {}\n",
        match (r.enabled, r.on) {
            (true, true) => "on (the default starts on the first tier)",
            (true, false) => "on, but not routing (see the problems)",
            (false, _) => "off (tier refs still name their models)",
        }
    );
    for (i, t) in r.tiers.iter().enumerate() {
        let key = if t.key_present {
            String::new()
        } else {
            format!("; key missing (${})", t.key_env)
        };
        out.push_str(&format!(
            "{i}. {} → {}{}{key}\n",
            t.name,
            t.reference,
            price_note(t.pricing)
        ));
    }
    if r.on {
        if let Some(c) = r.strong_daily_usd {
            out.push_str(&format!(
                "Above the cheap tier today: ${:.2} of ${c:.2}\n",
                r.strong_spent_today
            ));
        }
    }
    out
}

/// Escalations and spend per tier, from ledger rows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RoutingStats {
    pub escalations: u64,
    /// Oldest first.
    pub days: Vec<DayEscalations>,
    pub tiers: Vec<TierSpend>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayEscalations {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub escalations: u64,
    pub reasons: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TierSpend {
    pub tier: String,
    pub calls: u64,
    pub usd: f64,
}

/// Rows with a route tag (eval rows aside): the escalations a day, why,
/// and what each tier cost.
pub fn stats(records: &[LedgerRecord]) -> RoutingStats {
    let mut days: BTreeMap<String, DayEscalations> = BTreeMap::new();
    let mut tiers: Vec<TierSpend> = Vec::new();
    let mut escalations = 0;
    for r in records.iter().filter(|r| r.eval.is_none()) {
        let Some(route) = &r.route else { continue };
        match tiers.iter_mut().find(|t| t.tier == route.tier) {
            Some(t) => {
                t.calls += 1;
                t.usd += r.cost_usd.unwrap_or(0.0);
            }
            None => tiers.push(TierSpend {
                tier: route.tier.clone(),
                calls: 1,
                usd: r.cost_usd.unwrap_or(0.0),
            }),
        }
        if let Some(why) = &route.escalated {
            escalations += 1;
            let day = r.timestamp.get(..10).unwrap_or("?").to_string();
            let d = days.entry(day.clone()).or_insert_with(|| DayEscalations {
                day,
                escalations: 0,
                reasons: BTreeMap::new(),
            });
            d.escalations += 1;
            *d.reasons.entry(why.clone()).or_default() += 1;
        }
    }
    for t in &mut tiers {
        t.usd = (t.usd * 1e6).round() / 1e6;
    }
    RoutingStats {
        escalations,
        days: days.into_values().collect(),
        tiers,
    }
}

/// A cheap/strong pair to route over.
#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub cheap: Option<Pick>,
    pub strong: Option<Pick>,
    pub said: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pick {
    /// The connected `provider/model`, or the catalog id to add.
    pub reference: String,
    pub connected: bool,
    pub pricing: Option<ProviderPricing>,
}

/// Blended per-M price, 3 input tokens to 1 output: an agent turn reads
/// far more than it writes.
fn blended(p: &ProviderPricing) -> f64 {
    (3.0 * p.input + p.output) / 4.0
}

/// Two connected models with a key and different prices: the cheapest and
/// the dearest. Otherwise the curated list's first value and strongest
/// picks (M22), connected or to add.
pub fn suggest(cat: &Catalog, listings: &[Listing], rec: Option<&Recommended>) -> Suggestion {
    let mut priced: Vec<Pick> = cat
        .entries
        .iter()
        .filter(|e| e.key().is_some_and(|k| !k.is_empty()))
        .filter_map(|e| {
            let p = e
                .pricing
                .or_else(|| catalog::price_for(e, listings).map(|(_, p)| p))?;
            Some(Pick {
                reference: e.reference(),
                connected: true,
                pricing: Some(p),
            })
        })
        .collect();
    priced.sort_by(|a, b| {
        let key = |p: &Pick| p.pricing.as_ref().map(blended).unwrap_or_default();
        key(a).total_cmp(&key(b))
    });
    let price = |p: &Pick| p.pricing.as_ref().map(blended).unwrap_or_default();
    let (cheap, strong) = match (priced.first(), priced.last()) {
        (Some(c), Some(s)) if price(s) > price(c) => (Some(c.clone()), Some(s.clone())),
        _ => {
            let first = |tier: &str| {
                rec.and_then(|r| r.tiers.iter().find(|t| t.name == tier))
                    .and_then(|t| t.picks.first())
                    .map(|p| Pick {
                        reference: p.connected.clone().unwrap_or_else(|| p.id.clone()),
                        connected: p.connected.is_some(),
                        pricing: p.pricing,
                    })
            };
            (first("value"), first("strongest"))
        }
    };
    let said = match (&cheap, &strong) {
        (Some(c), Some(s)) => {
            let one = |p: &Pick| {
                format!(
                    "{}{}{}",
                    p.reference,
                    price_note(p.pricing),
                    if p.connected { "" } else { ", not connected" }
                )
            };
            let mut said = format!("Suggested: cheap {}; strong {}.", one(c), one(s));
            if c.connected && s.connected {
                said.push_str(&format!(
                    " `ferrule model route set {} {}` turns it on.",
                    c.reference, s.reference
                ));
            } else {
                let provider = rec
                    .and_then(|r| r.provider.clone())
                    .unwrap_or_else(|| "openrouter".into());
                for p in [c, s].into_iter().filter(|p| !p.connected) {
                    said.push_str(&format!(
                        " Connect {0} with `ferrule model add {provider}/{0}`.",
                        p.reference
                    ));
                }
            }
            said
        }
        _ => "No pair to suggest: connect two models with prices (`ferrule model recommend` lists some), or name any two with `ferrule model route set`.".into(),
    };
    Suggestion {
        cheap,
        strong,
        said,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(day: &str, tier: &str, escalated: Option<&str>, cost: f64, eval: bool) -> LedgerRecord {
        let mut v = serde_json::json!({
            "timestamp": format!("{day}T10:00:00+00:00"), "session_id": "x",
            "task_shape": "chat", "provider": "p", "model": "m",
            "iteration": 0, "input_tokens": 1, "cached_input_tokens": 0,
            "output_tokens": 1, "tool_calls": 0, "latency_ms": 1,
            "outcome": "ok", "cost_usd": cost,
            "route": {"tier": tier, "escalated": escalated},
        });
        if eval {
            v["eval"] = serde_json::json!({"run_id": "r", "suite": "s", "kind": "capability", "task": "t", "variant": "engineered"});
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn stats_count_escalations_per_day_and_reason_and_spend_per_tier() {
        let mut rows = vec![
            row("2026-09-01", "cheap", None, 0.01, false),
            row("2026-09-01", "strong", Some("tool_errors"), 0.2, false),
            row("2026-09-01", "strong", None, 0.1, false),
            row("2026-09-02", "strong", Some("tool_errors"), 0.3, false),
            row("2026-09-02", "strong", Some("owner"), 0.3, false),
            row("2026-09-02", "cheap", None, 0.02, false),
        ];
        // Eval rows and rows from before routing don't count.
        rows.push(row("2026-09-02", "strong", Some("check_failed"), 9.0, true));
        let mut plain = row("2026-09-02", "cheap", None, 5.0, false);
        plain.route = None;
        rows.push(plain);
        let s = stats(&rows);
        assert_eq!(s.escalations, 3);
        let days: Vec<(&str, u64)> = s
            .days
            .iter()
            .map(|d| (d.day.as_str(), d.escalations))
            .collect();
        assert_eq!(days, [("2026-09-01", 1), ("2026-09-02", 2)]);
        assert_eq!(s.days[1].reasons["tool_errors"], 1);
        assert_eq!(s.days[1].reasons["owner"], 1);
        let tiers: Vec<(&str, u64, f64)> = s
            .tiers
            .iter()
            .map(|t| (t.tier.as_str(), t.calls, t.usd))
            .collect();
        assert_eq!(tiers, [("cheap", 2, 0.03), ("strong", 4, 0.9)]);
    }

    const TWO_PRICED: &str = r#"
default_provider = "c"

[providers.c]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "c-small"
price_input_per_mtok = 0.2
price_cached_input_per_mtok = 0.2
price_output_per_mtok = 0.8

[providers.s]
base_url = "http://127.0.0.1:2/v1"
api_key_env = "PATH"
model = "s-big"
price_input_per_mtok = 3.0
price_cached_input_per_mtok = 3.0
price_output_per_mtok = 15.0

[providers.m]
base_url = "http://127.0.0.1:3/v1"
api_key_env = "PATH"
model = "m-mid"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 1.0
price_output_per_mtok = 4.0

[providers.n]
base_url = "http://127.0.0.1:4/v1"
api_key_env = "FERRULE_M25_NEVER_SET"
model = "n-dear"
price_input_per_mtok = 90.0
price_cached_input_per_mtok = 90.0
price_output_per_mtok = 90.0
"#;

    #[test]
    fn suggest_picks_the_cheapest_and_dearest_connected_models_with_a_key() {
        let cfg: crate::config::Config = toml::from_str(TWO_PRICED).unwrap();
        let s = suggest(&Catalog::from_config(&cfg), &[], None);
        assert_eq!(s.cheap.as_ref().unwrap().reference, "c/c-small");
        // n is dearer, but has no key.
        assert_eq!(s.strong.as_ref().unwrap().reference, "s/s-big");
        assert!(
            s.said
                .contains("`ferrule model route set c/c-small s/s-big`"),
            "{}",
            s.said
        );
    }

    #[test]
    fn with_one_priced_model_suggest_says_what_to_do() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
[providers.c]
base_url = "http://127.0.0.1:1/v1"
api_key_env = "PATH"
model = "c-small"
price_input_per_mtok = 0.2
price_cached_input_per_mtok = 0.2
price_output_per_mtok = 0.8
"#,
        )
        .unwrap();
        let s = suggest(&Catalog::from_config(&cfg), &[], None);
        assert!(s.cheap.is_none() && s.strong.is_none());
        assert!(s.said.starts_with("No pair to suggest"), "{}", s.said);
    }
}
