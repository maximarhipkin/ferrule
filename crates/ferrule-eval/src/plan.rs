//! `--dry-run`: what a suite run would do and the most it could spend,
//! without a single model call.

use crate::report::{tokens, usd};
use crate::sink::{Caps, Pricing};
use crate::suite::Suite;
use crate::variant::Variant;
use anyhow::Result;
use ferrule_core::HarnessProfile;
use std::fmt::Write as _;

pub struct PlanInput<'a> {
    pub suite: &'a Suite,
    pub tags: &'a [String],
    pub tasks: &'a [String],
    pub variants: &'a [Variant],
    pub repeat: u32,
    /// Already windowed (see [`crate::variant::windowed`]).
    pub profile: &'a HarnessProfile,
    pub provider: &'a str,
    pub model: &'a str,
    pub pricing: Option<Pricing>,
    pub caps: Caps,
}

/// The most calls one run can make: every step, a compaction summary per
/// step for the engineered variant, and the closing status answer. A
/// rubric adds one judge call per run on top.
pub fn max_calls(v: Variant, max_iterations: usize) -> u64 {
    let steps = max_iterations as u64;
    match v {
        Variant::Engineered => 2 * steps + 1,
        Variant::Naive => steps + 1,
    }
}

pub fn render(p: &PlanInput<'_>) -> Result<String> {
    let tasks = p.suite.select(p.tags, p.tasks)?;
    let repeat = p.repeat.max(1);
    let window = p.profile.context_window as u64;
    let reserve = p.profile.output_reserve as u64;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "dry run — suite {} ({}), {} via {}, context window {}; nothing is sent to the model\n",
        p.suite.name,
        p.suite.kind.as_str(),
        p.model,
        p.provider,
        tokens(window)
    );
    let (mut all_calls, mut all_in, mut all_out) = (0u64, 0u64, 0u64);
    for t in &tasks {
        let graders: Vec<&str> = [
            t.grade.command.as_ref().map(|_| "command"),
            t.grade.rubric.as_ref().map(|_| "rubric"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let first = t
            .prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let first: String = first.chars().take(70).collect();
        let _ = writeln!(
            out,
            "  {} [{}] — {}\n      graders: {}; steps ≤ {}{}",
            t.id,
            t.tags.join(","),
            first.trim(),
            graders.join(" + "),
            t.max_iterations(p.suite),
            if t.check.is_some() {
                "; engineered checks with its verify command"
            } else {
                ""
            }
        );
        for v in p.variants {
            let calls = max_calls(*v, t.max_iterations(p.suite)) * repeat as u64;
            all_calls += calls;
            all_in += calls * window;
            all_out += calls * reserve;
            if t.grade.rubric.is_some() {
                let judged = repeat as u64;
                all_calls += judged;
                all_in += judged * crate::rubric::JUDGE_MAX_INPUT;
                all_out += judged * crate::rubric::JUDGE_MAX_OUTPUT as u64;
            }
        }
    }
    let runs = tasks.len() * p.variants.len() * repeat as usize;
    let _ = writeln!(
        out,
        "\n{runs} task run(s): {} task(s) × {} × {repeat} repeat(s)",
        tasks.len(),
        p.variants
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
    );
    let _ = writeln!(
        out,
        "worst case (every step used, every call a full window): {all_calls} calls, {} input + {} output tokens",
        tokens(all_in),
        tokens(all_out)
    );
    let _ = writeln!(
        out,
        "  worst-case cost: {}",
        match p.pricing {
            Some(pr) => usd(Some(pr.cost(all_in, 0, 0, all_out))),
            None => format!(
                "n/a — set price_input_per_mtok, price_cached_input_per_mtok and \
                 price_output_per_mtok under [providers.{}] to price it",
                p.provider
            ),
        }
    );
    let _ = writeln!(
        out,
        "  typical runs use a small fraction of this: most tasks finish in 5-20 steps"
    );
    let _ = writeln!(
        out,
        "budget cap: {}, {} (the suite stops cleanly at the first call past either)",
        p.caps
            .max_usd
            .map(|m| format!("${m:.2}"))
            .unwrap_or_else(|| "no cost cap".into()),
        p.caps
            .max_tokens
            .map(|m| format!("{} tokens", tokens(m)))
            .unwrap_or_else(|| "no token cap".into()),
    );
    if p.caps.max_usd.is_some() && p.pricing.is_none() {
        let _ = writeln!(
            out,
            "  note: the provider has no prices configured, so only the token cap applies"
        );
    }
    Ok(out)
}
