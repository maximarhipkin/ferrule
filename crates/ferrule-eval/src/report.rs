//! What a suite run prints: a line per task, then per variant the pass
//! rate, tokens and cost, and with two variants the difference (see
//! `docs/m14-eval.md`, "Reports").

use crate::runner::{Outcome, SuiteRun, TaskResult};
use crate::sink::Totals;
use crate::variant::Variant;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Pass/fail/error counts and totals for one variant of a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VariantSummary {
    pub pass: usize,
    pub fail: usize,
    pub error: usize,
    pub stopped: usize,
    pub totals: Totals,
    pub truncations: u32,
    pub compactions: u32,
    pub verify_failures: u32,
}

impl VariantSummary {
    /// Runs with a verdict: stopped ones don't count.
    pub fn graded(&self) -> usize {
        self.pass + self.fail + self.error
    }

    pub fn pass_rate(&self) -> Option<f64> {
        (self.graded() > 0).then(|| self.pass as f64 / self.graded() as f64)
    }
}

pub fn summarize(results: &[TaskResult]) -> BTreeMap<Variant, VariantSummary> {
    let mut out: BTreeMap<Variant, VariantSummary> = BTreeMap::new();
    for r in results {
        let s = out.entry(r.variant).or_default();
        match r.outcome {
            Outcome::Pass => s.pass += 1,
            Outcome::Fail => s.fail += 1,
            Outcome::Error => s.error += 1,
            Outcome::Stopped => s.stopped += 1,
        }
        s.totals.merge(&r.totals);
        s.truncations += r.truncations;
        s.compactions += r.compactions;
        s.verify_failures += r.verify_failures;
    }
    out
}

pub fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

pub fn usd(c: Option<f64>) -> String {
    match c {
        Some(c) if c < 0.01 && c > 0.0 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
        None => "n/a".into(),
    }
}

fn signed_tokens(d: i128) -> String {
    let sign = if d < 0 { "-" } else { "+" };
    format!("{sign}{}", tokens(d.unsigned_abs() as u64))
}

fn signed_usd(d: f64) -> String {
    if d < 0.0 {
        format!("-${:.4}", -d)
    } else {
        format!("+${d:.4}")
    }
}

fn cell(r: &TaskResult) -> String {
    let mut s = match r.outcome {
        Outcome::Pass => "pass".to_string(),
        Outcome::Fail => "FAIL".to_string(),
        Outcome::Error => "error".to_string(),
        Outcome::Stopped => "stopped".to_string(),
    };
    let mut notes = Vec::new();
    if r.truncations > 0 {
        notes.push(format!("{}×trunc", r.truncations));
    }
    if r.compactions > 0 {
        notes.push(format!("{}×compact", r.compactions));
    }
    if r.verify_failures > 0 {
        notes.push(format!("{}×check", r.verify_failures));
    }
    if !notes.is_empty() {
        let _ = write!(s, " ({})", notes.join(", "));
    }
    s
}

fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{s:<w$}", w = widths[i]))
            .collect();
        out.push_str("  ");
        out.push_str(line.join("   ").trim_end());
        out.push('\n');
    }
    out
}

/// The whole report for one run.
pub fn render(run: &SuiteRun) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ferrule eval — suite {} ({}), {} via {}, context window {}, run {}",
        run.suite,
        run.kind,
        run.model,
        run.provider,
        tokens(run.context_window as u64),
        run.run_id
    );
    let variants: Vec<Variant> = {
        let mut v: Vec<Variant> = run.results.iter().map(|r| r.variant).collect();
        v.sort();
        v.dedup();
        v
    };

    // One line per task (and repeat).
    let mut header = vec!["task".to_string()];
    header.extend(variants.iter().map(|v| v.to_string()));
    let mut rows = vec![header];
    let mut keys: Vec<(String, u32)> = Vec::new();
    for r in &run.results {
        let k = (r.task.clone(), r.repeat);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let repeated = keys.iter().any(|(_, r)| *r > 0);
    for (task, rep) in &keys {
        let mut row = vec![if repeated {
            format!("{task} #{}", rep + 1)
        } else {
            task.clone()
        }];
        for v in &variants {
            row.push(
                run.results
                    .iter()
                    .find(|r| &r.task == task && r.repeat == *rep && r.variant == *v)
                    .map(cell)
                    .unwrap_or_else(|| "—".into()),
            );
        }
        rows.push(row);
    }
    out.push('\n');
    out.push_str(&table(&rows));

    // Per variant, and the difference.
    let sums = summarize(&run.results);
    let mut header = vec![String::new()];
    header.extend(variants.iter().map(|v| v.to_string()));
    let ab = variants.len() == 2;
    if ab {
        header.push("engineered − naive".into());
    }
    let get = |v: &Variant| sums.get(v).cloned().unwrap_or_default();
    let rate = |s: &VariantSummary| match s.pass_rate() {
        Some(p) => format!("{}/{} ({:.0}%)", s.pass, s.graded(), p * 100.0),
        None => "—".into(),
    };
    let mut rows = vec![header];
    let mut line = |name: &str, f: &dyn Fn(&VariantSummary) -> String, diff: Option<String>| {
        let mut row = vec![name.to_string()];
        row.extend(variants.iter().map(|v| f(&get(v))));
        if ab {
            row.push(diff.unwrap_or_default());
        }
        rows.push(row);
    };
    let (e, n) = (get(&Variant::Engineered), get(&Variant::Naive));
    line(
        "pass rate",
        &rate,
        match (e.pass_rate(), n.pass_rate()) {
            (Some(a), Some(b)) => Some(format!("{:+.0} pts", (a - b) * 100.0)),
            _ => None,
        },
    );
    line("errors", &|s| s.error.to_string(), None);
    line(
        "input tokens",
        &|s| tokens(s.totals.input_tokens),
        Some(signed_tokens(
            e.totals.input_tokens as i128 - n.totals.input_tokens as i128,
        )),
    );
    line(
        "output tokens",
        &|s| tokens(s.totals.output_tokens),
        Some(signed_tokens(
            e.totals.output_tokens as i128 - n.totals.output_tokens as i128,
        )),
    );
    line(
        "model calls",
        &|s| s.totals.calls.to_string(),
        Some(format!(
            "{:+}",
            e.totals.calls as i64 - n.totals.calls as i64
        )),
    );
    line(
        "cost",
        &|s| usd(s.totals.cost_usd),
        match (e.totals.cost_usd, n.totals.cost_usd) {
            (Some(a), Some(b)) => Some(signed_usd(a - b)),
            _ => None,
        },
    );
    line(
        "tokens per pass",
        &|s| {
            if s.pass == 0 {
                "—".into()
            } else {
                tokens(s.totals.tokens() / s.pass as u64)
            }
        },
        None,
    );
    line(
        "context events",
        &|s| {
            let mut parts = Vec::new();
            if s.compactions > 0 {
                parts.push(format!("{} compactions", s.compactions));
            }
            if s.truncations > 0 {
                parts.push(format!("{} truncations", s.truncations));
            }
            if parts.is_empty() {
                "none".into()
            } else {
                parts.join(", ")
            }
        },
        None,
    );
    line(
        "failed checks fixed",
        &|s| s.verify_failures.to_string(),
        None,
    );
    out.push('\n');
    out.push_str(&table(&rows));

    let _ = write!(
        out,
        "\ntotal: {} calls, {} input + {} output tokens, cost {}\n",
        run.totals.calls,
        tokens(run.totals.input_tokens),
        tokens(run.totals.output_tokens),
        usd(run.totals.cost_usd)
    );
    if let Some(why) = &run.budget_stop {
        let _ = writeln!(
            out,
            "STOPPED by the budget: {why}. {} task run(s) not started.",
            run.not_run
        );
    }
    let failed: Vec<&TaskResult> = run
        .results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Fail | Outcome::Error))
        .collect();
    if !failed.is_empty() {
        out.push_str("\nwhy they failed:\n");
        for r in failed {
            let why = r
                .graders
                .iter()
                .find(|g| !g.passed)
                .map(|g| first_line(&g.detail))
                .or_else(|| r.stopped_early.clone())
                .unwrap_or_default();
            let _ = writeln!(out, "  {} ({}): {}", r.task, r.variant, why);
        }
    }
    out
}

/// The last non-empty line of a grader's output: usually the assertion;
/// failing that, the exit code.
fn first_line(detail: &str) -> String {
    let mut lines = detail.lines().rev().map(str::trim);
    let line = lines
        .clone()
        .find(|l| !l.is_empty() && !l.starts_with("[exit code") && !l.starts_with("[stderr]"))
        .or_else(|| lines.find(|l| !l.is_empty()))
        .unwrap_or("");
    let mut s: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        s.push('…');
    }
    s
}
