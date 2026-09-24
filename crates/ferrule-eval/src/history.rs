//! Earlier runs and the diff against them (see `docs/m14-eval.md`,
//! "Reports"). Each run is saved as `<root>/<run id>/run.json`; run ids
//! start with the UTC time, so they sort oldest first.

use crate::report::{self, VariantSummary};
use crate::runner::{Outcome, SuiteRun};
use crate::variant::Variant;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// Every saved run under `root`, oldest first. Unreadable ones are
/// skipped: one bad file mustn't hide the rest.
pub fn load_runs(root: &Path) -> Vec<SuiteRun> {
    let mut runs: Vec<SuiteRun> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("run.json")).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    runs
}

/// The latest run before `run` that is comparable for `variant`: the
/// same suite, kind and model, and it ran that variant.
pub fn previous<'a>(
    runs: &'a [SuiteRun],
    run: &SuiteRun,
    variant: Variant,
) -> Option<&'a SuiteRun> {
    runs.iter().rev().find(|p| {
        p.run_id < run.run_id
            && p.suite == run.suite
            && p.kind == run.kind
            && p.model == run.model
            && p.results.iter().any(|r| r.variant == variant)
    })
}

/// How one variant moved between two runs.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantDiff {
    pub variant: Variant,
    pub previous_run: String,
    pub before: VariantSummary,
    pub now: VariantSummary,
    pub newly_passing: Vec<String>,
    pub newly_failing: Vec<String>,
    /// Tasks whose definition or fixture changed in between.
    pub changed: Vec<String>,
    pub new_tasks: Vec<String>,
    pub gone: Vec<String>,
}

/// A task's verdict across its repeats: `Some(true)` if every graded
/// repeat passed, `Some(false)` if any failed, `None` if none was graded.
fn verdicts(run: &SuiteRun, variant: Variant) -> BTreeMap<String, (Option<bool>, String)> {
    let mut out: BTreeMap<String, (Option<bool>, String)> = BTreeMap::new();
    for r in run.results.iter().filter(|r| r.variant == variant) {
        let e = out
            .entry(r.task.clone())
            .or_insert((None, r.fingerprint.clone()));
        let passed = match r.outcome {
            Outcome::Pass => true,
            Outcome::Fail | Outcome::Error => false,
            Outcome::Stopped => continue,
        };
        e.0 = Some(e.0.unwrap_or(true) && passed);
    }
    out
}

pub fn diff(prev: &SuiteRun, run: &SuiteRun, variant: Variant) -> VariantDiff {
    let before = verdicts(prev, variant);
    let now = verdicts(run, variant);
    let mut d = VariantDiff {
        variant,
        previous_run: prev.run_id.clone(),
        before: report::summarize(&prev.results)
            .remove(&variant)
            .unwrap_or_default(),
        now: report::summarize(&run.results)
            .remove(&variant)
            .unwrap_or_default(),
        newly_passing: Vec::new(),
        newly_failing: Vec::new(),
        changed: Vec::new(),
        new_tasks: Vec::new(),
        gone: Vec::new(),
    };
    let names: BTreeSet<&String> = before.keys().chain(now.keys()).collect();
    for task in names {
        match (before.get(task), now.get(task)) {
            (Some((was, fp_was)), Some((is, fp_is))) => {
                if fp_was != fp_is {
                    d.changed.push(task.clone());
                }
                match (was, is) {
                    (Some(false), Some(true)) => d.newly_passing.push(task.clone()),
                    (Some(true), Some(false)) => d.newly_failing.push(task.clone()),
                    _ => {}
                }
            }
            // Only a task with a verdict is new or gone.
            (None, Some((Some(_), _))) => d.new_tasks.push(task.clone()),
            (Some((Some(_), _)), None) => d.gone.push(task.clone()),
            _ => {}
        }
    }
    d
}

/// The diff of every variant in `run` against its own previous run, from
/// the saved `runs` (which may include `run` itself).
pub fn diffs(runs: &[SuiteRun], run: &SuiteRun) -> Vec<Result<VariantDiff, Variant>> {
    let mut variants: Vec<Variant> = run.results.iter().map(|r| r.variant).collect();
    variants.sort();
    variants.dedup();
    variants
        .into_iter()
        .map(|v| previous(runs, run, v).map(|p| diff(p, run, v)).ok_or(v))
        .collect()
}

fn rate(s: &VariantSummary) -> String {
    match s.pass_rate() {
        Some(r) => format!("{}/{} ({:.0}%)", s.pass, s.graded(), r * 100.0),
        None => "n/a".into(),
    }
}

/// The "since the last run" section of the report.
pub fn render(diffs: &[Result<VariantDiff, Variant>]) -> String {
    let mut out = String::from("\nsince the last run (same suite, kind and model):\n");
    for d in diffs {
        let d = match d {
            Ok(d) => d,
            Err(v) => {
                let _ = writeln!(out, "  {v}: no earlier run to compare with");
                continue;
            }
        };
        let points = match (d.before.pass_rate(), d.now.pass_rate()) {
            (Some(a), Some(b)) => format!(", {:+.0} pts", (b - a) * 100.0),
            _ => String::new(),
        };
        let tok = |s: &VariantSummary| (s.totals.input_tokens + s.totals.output_tokens) as i128;
        let cost = match (d.before.totals.cost_usd, d.now.totals.cost_usd) {
            (Some(a), Some(b)) => format!("; cost {}", report::signed_usd(b - a)),
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "  {} vs run {}: pass rate {} → {}{points}; tokens {}{cost}",
            d.variant,
            d.previous_run,
            rate(&d.before),
            rate(&d.now),
            report::signed_tokens(tok(&d.now) - tok(&d.before)),
        );
        let mark = |t: &String| {
            if d.changed.contains(t) {
                format!("{t} (task changed)")
            } else {
                t.clone()
            }
        };
        let list = |v: &[String]| v.iter().map(mark).collect::<Vec<_>>().join(", ");
        if !d.newly_failing.is_empty() {
            let _ = writeln!(out, "    NEWLY FAILING: {}", list(&d.newly_failing));
        }
        if !d.newly_passing.is_empty() {
            let _ = writeln!(out, "    newly passing: {}", list(&d.newly_passing));
        }
        let quiet: Vec<&String> = d
            .changed
            .iter()
            .filter(|t| !d.newly_failing.contains(t) && !d.newly_passing.contains(t))
            .collect();
        if !quiet.is_empty() {
            let names: Vec<&str> = quiet.iter().map(|s| s.as_str()).collect();
            let _ = writeln!(out, "    task changed since: {}", names.join(", "));
        }
        if !d.new_tasks.is_empty() {
            let _ = writeln!(out, "    new: {}", d.new_tasks.join(", "));
        }
        if !d.gone.is_empty() {
            let _ = writeln!(out, "    not run this time: {}", d.gone.join(", "));
        }
        if d.newly_failing.is_empty()
            && d.newly_passing.is_empty()
            && d.changed.is_empty()
            && d.new_tasks.is_empty()
            && d.gone.is_empty()
        {
            out.push_str("    same verdict on every task\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TaskResult;
    use crate::sink::Totals;

    fn result(task: &str, variant: Variant, outcome: Outcome, fp: &str) -> TaskResult {
        TaskResult {
            task: task.into(),
            variant,
            repeat: 0,
            outcome,
            graders: vec![],
            totals: Totals {
                input_tokens: 1000,
                output_tokens: 100,
                cost_usd: Some(0.01),
                ..Default::default()
            },
            iterations: 1,
            wall_ms: 1,
            truncations: 0,
            compactions: 0,
            verify_failures: 0,
            stopped_early: None,
            fingerprint: fp.into(),
            context_window: 32000,
            ferrule_version: "0".into(),
        }
    }

    fn run(id: &str, model: &str, results: Vec<TaskResult>) -> SuiteRun {
        SuiteRun {
            run_id: id.into(),
            suite: "s".into(),
            kind: "capability".into(),
            provider: "p".into(),
            model: model.into(),
            context_window: 32000,
            results,
            totals: Totals::default(),
            budget_stop: None,
            not_run: 0,
        }
    }

    use Outcome::*;
    use Variant::*;

    #[test]
    fn the_diff_names_what_moved_and_what_changed() {
        let first = run(
            "20260101T000000-a",
            "m",
            vec![
                result("a", Engineered, Pass, "1"),
                result("b", Engineered, Fail, "1"),
                result("c", Engineered, Pass, "1"),
                result("gone", Engineered, Pass, "1"),
            ],
        );
        let second = run(
            "20260102T000000-b",
            "m",
            vec![
                result("a", Engineered, Fail, "2"),
                result("b", Engineered, Pass, "1"),
                result("c", Engineered, Pass, "2"),
                result("new", Engineered, Pass, "1"),
            ],
        );
        let runs = vec![first.clone(), second.clone()];
        let d = diffs(&runs, &second);
        let d = d[0].as_ref().unwrap();
        assert_eq!(d.previous_run, first.run_id);
        assert_eq!(d.newly_failing, vec!["a"]);
        assert_eq!(d.newly_passing, vec!["b"]);
        assert_eq!(d.changed, vec!["a", "c"]);
        assert_eq!(d.new_tasks, vec!["new"]);
        assert_eq!(d.gone, vec!["gone"]);
        let text = render(&[Ok(d.clone())]);
        assert!(
            text.contains("pass rate 3/4 (75%) → 3/4 (75%), +0 pts"),
            "{text}"
        );
        assert!(text.contains("NEWLY FAILING: a (task changed)"), "{text}");
        assert!(text.contains("newly passing: b\n"), "{text}");
        assert!(text.contains("task changed since: c\n"), "{text}");
    }

    #[test]
    fn only_the_same_suite_kind_model_and_variant_are_compared() {
        let other_model = run(
            "20260101T000000-a",
            "other",
            vec![result("a", Engineered, Pass, "1")],
        );
        let naive_only = run(
            "20260102T000000-b",
            "m",
            vec![result("a", Naive, Fail, "1")],
        );
        let mut regression = run(
            "20260103T000000-c",
            "m",
            vec![result("a", Engineered, Pass, "1")],
        );
        regression.kind = "regression".into();
        let later = run(
            "20260105T000000-e",
            "m",
            vec![result("a", Engineered, Pass, "1")],
        );
        let now = run(
            "20260104T000000-d",
            "m",
            vec![
                result("a", Engineered, Pass, "1"),
                result("a", Naive, Pass, "1"),
            ],
        );
        let runs = vec![other_model, naive_only, regression, now.clone(), later];
        let d = diffs(&runs, &now);
        // Engineered: nothing comparable before it (the later run doesn't count).
        assert_eq!(d[0], Err(Engineered));
        let naive = d[1].as_ref().unwrap();
        assert_eq!(naive.previous_run, "20260102T000000-b");
        assert_eq!(naive.newly_passing, vec!["a"]);
        assert!(render(&d).contains("engineered: no earlier run to compare with"));
    }

    #[test]
    fn a_task_fails_if_any_repeat_fails_and_stopped_runs_do_not_count() {
        let mut first = run(
            "20260101T000000-a",
            "m",
            vec![result("a", Engineered, Pass, "1")],
        );
        let mut again = result("a", Engineered, Fail, "1");
        again.repeat = 1;
        first.results.push(again);
        let second = run(
            "20260102T000000-b",
            "m",
            vec![
                result("a", Engineered, Pass, "1"),
                result("b", Engineered, Stopped, "1"),
            ],
        );
        let d = diff(&first, &second, Engineered);
        assert_eq!(d.newly_passing, vec!["a"]);
        assert!(
            d.new_tasks.is_empty(),
            "a stopped task has no verdict: {:?}",
            d.new_tasks
        );
    }

    #[test]
    fn saved_runs_load_oldest_first_and_bad_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["20260102T000000-b", "20260101T000000-a"] {
            std::fs::create_dir_all(dir.path().join(id)).unwrap();
            let r = run(id, "m", vec![]);
            std::fs::write(
                dir.path().join(id).join("run.json"),
                serde_json::to_string(&r).unwrap(),
            )
            .unwrap();
        }
        std::fs::create_dir_all(dir.path().join("broken")).unwrap();
        std::fs::write(dir.path().join("broken/run.json"), "{").unwrap();
        let ids: Vec<String> = load_runs(dir.path())
            .into_iter()
            .map(|r| r.run_id)
            .collect();
        assert_eq!(ids, vec!["20260101T000000-a", "20260102T000000-b"]);
        assert!(load_runs(&dir.path().join("missing")).is_empty());
    }
}
