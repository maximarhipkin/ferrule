//! The pass's spending caps. Every model call the pass makes goes through a
//! [`LearnSink`], which tags the ledger row (`task_shape` and `call_kind`
//! `"learn"`), prices it, and charges the [`Meter`]; the pass and every gate
//! agent (through the core [`Budget`] hook) ask the meter before each call.

use ferrule_core::{Budget, LedgerRecord, LedgerSink, Usage};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// The ledger's `call_kind` and `task_shape` for everything the pass spends.
pub const CALL_KIND: &str = "learn";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    pub usd_per_pass: f64,
    pub usd_per_day: f64,
    pub tokens_per_pass: u64,
    pub tokens_per_day: u64,
}

/// Money and tokens spent. `unpriced` counts calls that got no price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Spent {
    pub usd: f64,
    pub tokens: u64,
    pub calls: u64,
    #[serde(default)]
    pub unpriced: u64,
}

impl Spent {
    pub fn add(&mut self, r: &LedgerRecord) {
        self.tokens += r.input_tokens + r.output_tokens;
        self.calls += 1;
        match r.cost_usd {
            Some(c) => self.usd += c,
            None if !r.is_error() => self.unpriced += 1,
            None => {}
        }
    }

    /// What the rows at or after `since` spent, counting only the pass's.
    pub fn from_rows<'a>(rows: impl IntoIterator<Item = &'a LedgerRecord>) -> Self {
        let mut s = Spent::default();
        for r in rows {
            if r.call_kind == CALL_KIND {
                s.add(r);
            }
        }
        s
    }
}

pub struct Meter {
    caps: Caps,
    day_before: Spent,
    pass: Mutex<Spent>,
}

impl Meter {
    /// `day_before`: what earlier passes spent in the last 24 hours.
    pub fn new(caps: Caps, day_before: Spent) -> Arc<Self> {
        Arc::new(Self {
            caps,
            day_before,
            pass: Mutex::new(Spent::default()),
        })
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    pub fn day_before(&self) -> Spent {
        self.day_before
    }

    pub fn spent(&self) -> Spent {
        *self.pass.lock().unwrap()
    }

    pub fn charge(&self, r: &LedgerRecord) {
        self.pass.lock().unwrap().add(r);
    }

    /// `Some(why)` once a cap is reached: nothing more may be spent.
    pub fn exceeded(&self) -> Option<String> {
        let p = self.spent();
        let c = self.caps;
        let day_tokens = self.day_before.tokens + p.tokens;
        let day_usd = self.day_before.usd + p.usd;
        if p.tokens >= c.tokens_per_pass {
            Some(format!(
                "the per-pass cap of {} tokens is reached ({} spent)",
                c.tokens_per_pass, p.tokens
            ))
        } else if p.usd >= c.usd_per_pass {
            Some(format!(
                "the per-pass cap of ${:.2} is reached (${:.4} spent)",
                c.usd_per_pass, p.usd
            ))
        } else if day_tokens >= c.tokens_per_day {
            Some(format!(
                "the daily cap of {} tokens is reached ({} spent in 24h)",
                c.tokens_per_day, day_tokens
            ))
        } else if day_usd >= c.usd_per_day {
            Some(format!(
                "the daily cap of ${:.2} is reached (${:.4} spent in 24h)",
                c.usd_per_day, day_usd
            ))
        } else {
            None
        }
    }
}

/// The core hook: gate agents stop with a status once the meter is spent.
/// Charging happens in [`LearnSink`], which sees the priced row.
impl Budget for Meter {
    fn charge(&self, _usage: &Usage) {}

    fn exhausted(&self) -> Option<String> {
        self.exceeded()
    }
}

/// Prices one ledger row; `None` when the provider has no prices.
pub type PriceFn = Arc<dyn Fn(&LedgerRecord) -> Option<f64> + Send + Sync>;

/// Tags, prices and charges every row the pass writes, then hands it to the
/// real ledger (if there is one).
pub struct LearnSink {
    pub inner: Option<Arc<dyn LedgerSink>>,
    pub meter: Arc<Meter>,
    pub price: PriceFn,
    pub origin: String,
}

impl LearnSink {
    pub fn new(
        inner: Option<Arc<dyn LedgerSink>>,
        meter: Arc<Meter>,
        price: PriceFn,
        origin: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            meter,
            price,
            origin: origin.into(),
        })
    }
}

impl LedgerSink for LearnSink {
    fn record(&self, mut r: LedgerRecord) {
        r.task_shape = CALL_KIND.into();
        r.call_kind = CALL_KIND.into();
        r.origin = Some(self.origin.clone());
        if r.cost_usd.is_none() && !r.is_error() {
            r.cost_usd = (self.price)(&r);
        }
        self.meter.charge(&r);
        if let Some(inner) = &self.inner {
            inner.record(r);
        }
    }
}

/// A ledger row for a call the pass makes itself (not through an agent).
#[allow(clippy::too_many_arguments)]
pub fn row(
    session_id: &str,
    provider: &str,
    model: &str,
    iteration: usize,
    usage: &Usage,
    latency_ms: u64,
    error: Option<&str>,
) -> LedgerRecord {
    LedgerRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.into(),
        task_shape: CALL_KIND.into(),
        origin: None,
        provider: provider.into(),
        model: model.into(),
        iteration,
        call_kind: CALL_KIND.into(),
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        tool_calls: 0,
        latency_ms,
        outcome: if error.is_some() { "error" } else { "ok" }.into(),
        error_kind: error.map(|_| "provider".into()),
        error_message: error.map(str::to_string),
        cost_usd: None,
        eval: None,
        tree: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Caps {
        Caps {
            usd_per_pass: 0.5,
            usd_per_day: 1.0,
            tokens_per_pass: 1000,
            tokens_per_day: 5000,
        }
    }

    fn usage(i: u64, o: u64) -> Usage {
        Usage {
            input_tokens: i,
            output_tokens: o,
            cached_input_tokens: 0,
        }
    }

    #[test]
    fn the_sink_tags_prices_and_charges() {
        let meter = Meter::new(caps(), Spent::default());
        let seen: Arc<Mutex<Vec<LedgerRecord>>> = Arc::default();
        struct Keep(Arc<Mutex<Vec<LedgerRecord>>>);
        impl LedgerSink for Keep {
            fn record(&self, r: LedgerRecord) {
                self.0.lock().unwrap().push(r);
            }
        }
        let sink = LearnSink::new(
            Some(Arc::new(Keep(seen.clone()))),
            meter.clone(),
            Arc::new(|r: &LedgerRecord| Some(r.input_tokens as f64 * 0.001)),
            "gate:1",
        );
        let mut r = row("s", "mock", "m", 0, &usage(100, 10), 5, None);
        r.call_kind = "turn".into();
        r.task_shape = "run".into();
        sink.record(r);
        let rows = seen.lock().unwrap();
        assert_eq!(rows[0].call_kind, "learn");
        assert_eq!(rows[0].task_shape, "learn");
        assert_eq!(rows[0].origin.as_deref(), Some("gate:1"));
        assert_eq!(rows[0].cost_usd, Some(0.1));
        assert_eq!(meter.spent().tokens, 110);
        assert!(meter.exceeded().is_none());
    }

    #[test]
    fn caps_trip_per_pass_and_per_day() {
        let meter = Meter::new(caps(), Spent::default());
        let mut r = row("s", "mock", "m", 0, &usage(990, 10), 5, None);
        meter.charge(&r);
        assert!(meter
            .exceeded()
            .unwrap()
            .contains("per-pass cap of 1000 tokens"));

        let meter = Meter::new(
            caps(),
            Spent {
                usd: 0.95,
                ..Default::default()
            },
        );
        r.cost_usd = Some(0.06);
        r.input_tokens = 1;
        meter.charge(&r);
        assert!(meter.exceeded().unwrap().contains("daily cap of $1.00"));
        assert!(meter.exhausted().is_some());
    }

    #[test]
    fn day_spend_counts_only_learn_rows() {
        let mut a = row("s", "p", "m", 0, &usage(10, 0), 1, None);
        a.cost_usd = Some(0.2);
        let mut b = a.clone();
        b.call_kind = "turn".into();
        let s = Spent::from_rows([&a, &b]);
        assert_eq!(s.calls, 1);
        assert_eq!(s.usd, 0.2);
    }
}
