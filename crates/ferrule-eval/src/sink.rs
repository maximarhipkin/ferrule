//! The ledger sink every eval agent writes through: it tags each row with
//! its task run, prices it, keeps the totals, and stops the suite when the
//! budget is spent (see `docs/m14-eval.md`, "Cost guards").

use ferrule_core::{EvalTag, LedgerRecord, LedgerSink, StopFlag};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// USD per million tokens. `input` is charged on the part neither read
/// from nor written to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    /// M23: a cache write. `None`: the input price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl Pricing {
    pub fn cost(&self, input: u64, cached: u64, written: u64, output: u64) -> f64 {
        let rest = input.saturating_sub(cached).saturating_sub(written);
        (rest as f64 * self.input
            + cached as f64 * self.cached_input
            + written as f64 * self.cache_write.unwrap_or(self.input)
            + output as f64 * self.output)
            / 1_000_000.0
    }
}

/// A suite run's spending limits. `None` is no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Caps {
    pub max_usd: Option<f64>,
    /// Input + output tokens.
    pub max_tokens: Option<u64>,
}

/// Token and cost totals over some set of calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub calls: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// `None` when the provider has no configured prices.
    pub cost_usd: Option<f64>,
    /// Main-loop calls (`call_kind = "turn"`).
    pub turns: u64,
}

impl Totals {
    fn add(&mut self, r: &LedgerRecord) {
        self.calls += 1;
        self.input_tokens += r.input_tokens;
        self.cached_input_tokens += r.cached_input_tokens;
        self.output_tokens += r.output_tokens;
        if let Some(c) = r.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += c;
        }
        if r.call_kind == "turn" {
            self.turns += 1;
        }
    }

    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn merge(&mut self, o: &Totals) {
        self.calls += o.calls;
        self.input_tokens += o.input_tokens;
        self.cached_input_tokens += o.cached_input_tokens;
        self.output_tokens += o.output_tokens;
        self.turns += o.turns;
        if let Some(c) = o.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += c;
        }
    }
}

struct State {
    tag: Option<EvalTag>,
    stop: Option<StopFlag>,
    run: Totals,
    suite: Totals,
    exceeded: Option<String>,
}

pub struct EvalSink {
    inner: Option<Arc<dyn LedgerSink>>,
    pricing: Option<Pricing>,
    caps: Caps,
    state: Mutex<State>,
}

impl EvalSink {
    pub fn new(inner: Option<Arc<dyn LedgerSink>>, pricing: Option<Pricing>, caps: Caps) -> Self {
        Self {
            inner,
            pricing,
            caps,
            state: Mutex::new(State {
                tag: None,
                stop: None,
                run: Totals::default(),
                suite: Totals::default(),
                exceeded: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Rows from now on belong to `tag`; `stop` is set once the budget is
    /// spent, which ends that agent before its next call.
    pub fn begin(&self, tag: EvalTag, stop: StopFlag) {
        let mut s = self.lock();
        s.tag = Some(tag);
        s.run = Totals::default();
        if s.exceeded.is_some() {
            stop.stop();
        }
        s.stop = Some(stop);
    }

    /// The finished run's totals.
    pub fn end(&self) -> Totals {
        let mut s = self.lock();
        s.tag = None;
        s.stop = None;
        std::mem::take(&mut s.run)
    }

    /// Why the budget stopped the suite, once it has.
    pub fn exceeded(&self) -> Option<String> {
        self.lock().exceeded.clone()
    }

    pub fn suite_totals(&self) -> Totals {
        self.lock().suite.clone()
    }

    pub fn pricing(&self) -> Option<Pricing> {
        self.pricing
    }

    /// Writes a row that isn't a provider call (a verdict) straight to the
    /// ledger, uncounted.
    pub fn write_uncounted(&self, record: LedgerRecord) {
        if let Some(inner) = &self.inner {
            inner.record(record);
        }
    }
}

impl EvalSink {
    /// Like [`LedgerSink::record`], priced with `pricing` instead of the
    /// run's (a judge on another provider has its own prices).
    pub fn record_priced(&self, mut record: LedgerRecord, pricing: Option<Pricing>) {
        if record.cost_usd.is_none() {
            if let Some(p) = pricing {
                record.cost_usd = Some(p.cost(
                    record.input_tokens,
                    record.cached_input_tokens,
                    record.cache_write_input_tokens,
                    record.output_tokens,
                ));
            }
        }
        {
            let mut s = self.lock();
            record.task_shape = "eval".into();
            record.origin = s.tag.as_ref().map(|t| format!("{}/{}", t.suite, t.task));
            record.eval = s.tag.clone();
            s.run.add(&record);
            s.suite.add(&record);
            if s.exceeded.is_none() {
                let tokens = s.suite.tokens();
                let why = match (self.caps.max_tokens, self.caps.max_usd, s.suite.cost_usd) {
                    (Some(max), _, _) if tokens >= max => Some(format!(
                        "the token budget is spent ({tokens} of {max} tokens)"
                    )),
                    (_, Some(max), Some(spent)) if spent >= max => Some(format!(
                        "the cost budget is spent (${spent:.2} of ${max:.2})"
                    )),
                    _ => None,
                };
                if let Some(why) = why {
                    s.exceeded = Some(why);
                    if let Some(stop) = &s.stop {
                        stop.stop();
                    }
                }
            }
        }
        if let Some(inner) = &self.inner {
            inner.record(record);
        }
    }
}

impl LedgerSink for EvalSink {
    fn record(&self, record: LedgerRecord) {
        self.record_priced(record, self.pricing);
    }
}
