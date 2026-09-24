//! File-backed sink and read side for the Phase 0 per-call ledger
//! (`ferrule-core::ledger`). Rows are appended as JSONL to
//! `<data_dir>/ledger.jsonl`; `ferrule ledger` aggregates them.

use crate::config::{Config, ProviderConfig};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use ferrule_core::{LedgerRecord, LedgerSink};
use ferrule_gateway::SCHEDULER_PSEUDO_CHANNEL;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// USD per million tokens. Only built when all three prices are configured —
/// a partial set would silently undercount.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderPricing {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

impl ProviderPricing {
    pub fn from_config(p: &ProviderConfig) -> Option<Self> {
        Some(Self {
            input: p.price_input_per_mtok?,
            cached_input: p.price_cached_input_per_mtok?,
            output: p.price_output_per_mtok?,
        })
    }

    /// `input_tokens` includes the cached ones (OpenAI `prompt_tokens`
    /// convention), so only the uncached remainder is billed at full price.
    pub fn cost_usd(&self, r: &LedgerRecord) -> f64 {
        let uncached = r.input_tokens.saturating_sub(r.cached_input_tokens);
        (uncached as f64 * self.input
            + r.cached_input_tokens as f64 * self.cached_input
            + r.output_tokens as f64 * self.output)
            / 1_000_000.0
    }
}

/// Appends one JSON line per record. Shared by every session in the
/// process; the mutex keeps concurrent sessions' lines from interleaving.
/// A write failure is logged and the row dropped — it never fails a turn.
pub struct FileLedgerSink {
    path: PathBuf,
    pricing: HashMap<String, ProviderPricing>,
    lock: Mutex<()>,
}

impl FileLedgerSink {
    pub fn new(path: PathBuf, pricing: HashMap<String, ProviderPricing>) -> Self {
        Self {
            path,
            pricing,
            lock: Mutex::new(()),
        }
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())
    }
}

impl LedgerSink for FileLedgerSink {
    fn record(&self, mut record: LedgerRecord) {
        if record.cost_usd.is_none() && !record.is_error() {
            if let Some(p) = self.pricing.get(&record.provider) {
                record.cost_usd = Some(p.cost_usd(&record));
            }
        }
        let line = match serde_json::to_string(&record) {
            Ok(mut l) => {
                l.push('\n');
                l
            }
            Err(e) => {
                tracing::warn!("ledger: could not serialize row ({e}); dropped");
                return;
            }
        };
        if let Err(e) = self.append(&line) {
            tracing::warn!(
                "ledger: write to {} failed ({e}); row dropped",
                self.path.display()
            );
        }
    }
}

pub fn ledger_path() -> Result<PathBuf> {
    Ok(crate::config::data_dir()?.join("ledger.jsonl"))
}

/// One sink per process, priced from `[providers.*]`. `None` (with a
/// warning) if the data dir is unavailable — the agent runs without a ledger
/// rather than not at all.
pub fn build_sink(cfg: &Config) -> Option<Arc<dyn LedgerSink>> {
    let path = match ledger_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("ledger disabled: {e}");
            return None;
        }
    };
    let pricing = cfg
        .providers
        .iter()
        .filter_map(|(name, p)| ProviderPricing::from_config(p).map(|pr| (name.clone(), pr)))
        .collect();
    Some(Arc::new(FileLedgerSink::new(path, pricing)))
}

/// Which entry point a ledger row came from. The sink is shared; the tag is
/// per agent.
#[derive(Clone)]
pub struct LedgerTag {
    pub sink: Arc<dyn LedgerSink>,
    pub task_shape: String,
    pub origin: Option<String>,
}

impl LedgerTag {
    pub fn new(
        sink: &Option<Arc<dyn LedgerSink>>,
        task_shape: &str,
        origin: Option<String>,
    ) -> Option<Self> {
        sink.as_ref().map(|s| Self {
            sink: s.clone(),
            task_shape: task_shape.into(),
            origin,
        })
    }
}

/// Gateway session ids are `<channel>__<chat>` (`ferrule-gateway::session`);
/// scheduler sessions use the reserved pseudo-channel with the task id as
/// the chat. Returns `(task_shape, origin)`.
pub fn classify_session(session_id: &str) -> (&'static str, Option<String>) {
    match session_id.split_once("__") {
        Some((ch, task)) if ch == SCHEDULER_PSEUDO_CHANNEL => ("scheduler", Some(task.to_string())),
        Some((ch, _)) => ("gateway", Some(ch.to_string())),
        None => ("gateway", None),
    }
}

/// `7d`, `12h`, `30m`, or an RFC 3339 instant.
pub fn parse_since(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let bad = || anyhow!("--since: expected e.g. 7d, 12h, 30m or an RFC 3339 time, got `{s}`");
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let d = match unit {
        'd' => Duration::days(n),
        'h' => Duration::hours(n),
        'm' => Duration::minutes(n),
        _ => return Err(anyhow!("--since: unknown unit in `{s}` (use d, h or m)")),
    };
    Ok(now - d)
}

/// Reads every parseable row at or after `since`. Returns the rows and the
/// number of malformed lines skipped. A missing file is an empty ledger.
pub fn read_records(
    path: &Path,
    since: Option<DateTime<Utc>>,
) -> Result<(Vec<LedgerRecord>, usize)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let mut records = Vec::new();
    let mut malformed = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(r) = serde_json::from_str::<LedgerRecord>(line) else {
            malformed += 1;
            continue;
        };
        if let Some(since) = since {
            match DateTime::parse_from_rfc3339(&r.timestamp) {
                Ok(t) if t.with_timezone(&Utc) < since => continue,
                Ok(_) => {}
                Err(_) => {
                    malformed += 1;
                    continue;
                }
            }
        }
        records.push(r);
    }
    Ok((records, malformed))
}

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryRow {
    pub task_shape: String,
    pub provider: String,
    pub model: String,
    pub calls: usize,
    pub errors: usize,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    /// Sum over priced rows only; `None` if no row in the group was priced.
    pub cost_usd: Option<f64>,
    pub priced_calls: usize,
}

impl SummaryRow {
    pub fn cache_hit_pct(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            self.cached_input_tokens as f64 * 100.0 / self.input_tokens as f64
        }
    }
}

/// Nearest-rank percentile over an already-sorted slice.
pub fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn aggregate(records: &[LedgerRecord]) -> Vec<SummaryRow> {
    let mut groups: BTreeMap<(String, String, String), Vec<&LedgerRecord>> = BTreeMap::new();
    // `ferrule eval`'s one-per-task verdict rows aren't provider calls.
    for r in records.iter().filter(|r| r.call_kind != "eval_result") {
        groups
            .entry((r.task_shape.clone(), r.provider.clone(), r.model.clone()))
            .or_default()
            .push(r);
    }
    groups
        .into_iter()
        .map(|((task_shape, provider, model), rows)| {
            let mut latencies: Vec<u64> = rows.iter().map(|r| r.latency_ms).collect();
            latencies.sort_unstable();
            let priced: Vec<f64> = rows.iter().filter_map(|r| r.cost_usd).collect();
            SummaryRow {
                task_shape,
                provider,
                model,
                calls: rows.len(),
                errors: rows.iter().filter(|r| r.is_error()).count(),
                input_tokens: rows.iter().map(|r| r.input_tokens).sum(),
                cached_input_tokens: rows.iter().map(|r| r.cached_input_tokens).sum(),
                output_tokens: rows.iter().map(|r| r.output_tokens).sum(),
                p50_latency_ms: percentile(&latencies, 50.0),
                p95_latency_ms: percentile(&latencies, 95.0),
                cost_usd: if priced.is_empty() {
                    None
                } else {
                    Some(priced.iter().sum())
                },
                priced_calls: priced.len(),
            }
        })
        .collect()
}

pub fn render_table(rows: &[SummaryRow]) -> String {
    let header = [
        "shape", "provider", "model", "calls", "errors", "in", "cached", "out", "cache%", "p50ms",
        "p95ms", "cost$",
    ];
    let mut table: Vec<Vec<String>> = vec![header.iter().map(|s| s.to_string()).collect()];
    for r in rows {
        let cost = match r.cost_usd {
            None => "-".to_string(),
            // `*` = only some of the group's calls had prices configured.
            Some(c) if r.priced_calls < r.calls => format!("{c:.4}*"),
            Some(c) => format!("{c:.4}"),
        };
        table.push(vec![
            r.task_shape.clone(),
            r.provider.clone(),
            r.model.clone(),
            r.calls.to_string(),
            r.errors.to_string(),
            r.input_tokens.to_string(),
            r.cached_input_tokens.to_string(),
            r.output_tokens.to_string(),
            format!("{:.1}", r.cache_hit_pct()),
            r.p50_latency_ms.to_string(),
            r.p95_latency_ms.to_string(),
            cost,
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            table
                .iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    table
        .iter()
        .map(|row| {
            row.iter()
                .zip(&widths)
                .map(|(c, w)| format!("{c:<w$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        shape: &str,
        provider: &str,
        latency_ms: u64,
        input: u64,
        cached: u64,
        output: u64,
        outcome: &str,
    ) -> LedgerRecord {
        LedgerRecord {
            timestamp: "2026-09-24T10:00:00+00:00".into(),
            session_id: "s".into(),
            task_shape: shape.into(),
            origin: None,
            provider: provider.into(),
            model: "m".into(),
            iteration: 0,
            call_kind: "turn".into(),
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            tool_calls: 0,
            latency_ms,
            outcome: outcome.into(),
            error_kind: None,
            error_message: None,
            cost_usd: None,
            eval: None,
            tree: None,
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let v: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&v, 50.0), 10);
        assert_eq!(percentile(&v, 95.0), 19);
        assert_eq!(percentile(&[7], 95.0), 7);
        assert_eq!(percentile(&[], 50.0), 0);
    }

    #[test]
    fn cost_bills_cached_tokens_at_the_cached_rate_only() {
        let p = ProviderPricing {
            input: 1.0,
            cached_input: 0.1,
            output: 4.0,
        };
        // 1M input of which 400k cached, 250k output:
        // 600k*1.0 + 400k*0.1 + 250k*4.0 = 0.6 + 0.04 + 1.0 per 1M-token unit.
        let r = rec("run", "kimi", 1, 1_000_000, 400_000, 250_000, "ok");
        assert!((p.cost_usd(&r) - 1.64).abs() < 1e-9);
    }

    #[test]
    fn pricing_requires_all_three_prices() {
        let mut p: ProviderConfig =
            toml::from_str("base_url = \"x\"\napi_key_env = \"K\"\nmodel = \"m\"").unwrap();
        assert!(ProviderPricing::from_config(&p).is_none());
        p.price_input_per_mtok = Some(1.0);
        p.price_output_per_mtok = Some(2.0);
        assert!(
            ProviderPricing::from_config(&p).is_none(),
            "partial prices must not produce a cost"
        );
        p.price_cached_input_per_mtok = Some(0.5);
        assert_eq!(
            ProviderPricing::from_config(&p),
            Some(ProviderPricing {
                input: 1.0,
                cached_input: 0.5,
                output: 2.0
            })
        );
    }

    #[test]
    fn aggregate_groups_and_sums() {
        let mut priced = rec("scheduler", "kimi", 100, 1000, 800, 50, "ok");
        priced.cost_usd = Some(0.25);
        let rows = aggregate(&[
            priced,
            rec("scheduler", "kimi", 300, 1000, 200, 50, "ok"),
            rec("scheduler", "kimi", 200, 0, 0, 0, "error"),
            rec("chat", "openai", 50, 10, 0, 5, "ok"),
        ]);
        assert_eq!(rows.len(), 2);
        let s = rows.iter().find(|r| r.task_shape == "scheduler").unwrap();
        assert_eq!((s.calls, s.errors), (3, 1));
        assert_eq!(
            (s.input_tokens, s.cached_input_tokens, s.output_tokens),
            (2000, 1000, 100)
        );
        assert!((s.cache_hit_pct() - 50.0).abs() < 1e-9);
        assert_eq!((s.p50_latency_ms, s.p95_latency_ms), (200, 300));
        assert_eq!((s.cost_usd, s.priced_calls), (Some(0.25), 1));
        let c = rows.iter().find(|r| r.task_shape == "chat").unwrap();
        assert_eq!(c.cost_usd, None);
        assert!(
            render_table(&rows).contains("0.2500*"),
            "partially priced group is marked"
        );
    }

    #[test]
    fn sessions_are_classified_by_channel_prefix() {
        assert_eq!(
            classify_session("scheduler__daily-report"),
            ("scheduler", Some("daily-report".into()))
        );
        assert_eq!(
            classify_session("telegram__555999"),
            ("gateway", Some("telegram".into()))
        );
        assert_eq!(classify_session("odd"), ("gateway", None));
    }

    #[test]
    fn since_accepts_relative_and_absolute() {
        let now = DateTime::parse_from_rfc3339("2026-09-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parse_since("7d", now).unwrap(), now - Duration::days(7));
        assert_eq!(parse_since("12h", now).unwrap(), now - Duration::hours(12));
        assert_eq!(
            parse_since("2026-09-01T00:00:00+03:00", now)
                .unwrap()
                .to_rfc3339(),
            "2026-08-31T21:00:00+00:00"
        );
        assert!(parse_since("7w", now).is_err());
        assert!(parse_since("soon", now).is_err());
        assert!(
            parse_since("7ד", now).is_err(),
            "multi-byte unit must error, not panic"
        );
        assert!(parse_since("", now).is_err());
    }

    #[test]
    fn sink_round_trip_prices_ok_rows_and_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let pricing = HashMap::from([(
            "kimi".to_string(),
            ProviderPricing {
                input: 1.0,
                cached_input: 0.0,
                output: 0.0,
            },
        )]);
        let sink = FileLedgerSink::new(path.clone(), pricing);
        sink.record(rec("run", "kimi", 10, 2_000_000, 0, 0, "ok"));
        sink.record(rec("run", "kimi", 10, 0, 0, 0, "error"));
        sink.record(rec("run", "unpriced", 10, 5, 0, 0, "ok"));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();

        let (records, malformed) = read_records(&path, None).unwrap();
        assert_eq!((records.len(), malformed), (3, 1));
        assert_eq!(records[0].cost_usd, Some(2.0));
        assert_eq!(records[1].cost_usd, None, "failed calls are not priced");
        assert_eq!(
            records[2].cost_usd, None,
            "no prices configured, never guessed"
        );

        let later = DateTime::parse_from_rfc3339("2026-09-25T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(read_records(&path, Some(later)).unwrap().0.len(), 0);
        let (missing, malformed) = read_records(&dir.path().join("missing.jsonl"), None).unwrap();
        assert!(
            missing.is_empty() && malformed == 0,
            "a missing ledger is an empty one"
        );
    }
}
