//! `TrustSink`: stamps each ledger row with its run tree, prices it, and
//! charges it to the hub on the way to the real sink.

use crate::hub::Hub;
use ferrule_core::{LedgerRecord, LedgerSink, TraceEvent, TraceLevel};
use std::sync::Arc;

/// A row's price in dollars, when its provider has prices.
pub type Pricer = Arc<dyn Fn(&LedgerRecord) -> Option<f64> + Send + Sync>;

pub struct TrustSink {
    inner: Arc<dyn LedgerSink>,
    hub: Arc<Hub>,
    tree: String,
    price: Option<Pricer>,
}

impl TrustSink {
    pub fn new(
        inner: Arc<dyn LedgerSink>,
        hub: Arc<Hub>,
        tree: impl Into<String>,
        price: Option<Pricer>,
    ) -> Self {
        Self {
            inner,
            hub,
            tree: tree.into(),
            price,
        }
    }
}

impl LedgerSink for TrustSink {
    fn record(&self, mut record: LedgerRecord) {
        if record.tree.is_none() {
            record.tree = Some(self.tree.clone());
        }
        if record.cost_usd.is_none() && !record.is_error() {
            if let Some(p) = &self.price {
                record.cost_usd = p(&record);
            }
        }
        self.hub.charge(&self.tree, &record);
        self.inner.record(record);
    }

    // Trace events pass straight through to the exporter behind (M33).
    fn trace_level(&self) -> TraceLevel {
        self.inner.trace_level()
    }

    fn trace(&self, event: TraceEvent) {
        self.inner.trace(event);
    }
}
