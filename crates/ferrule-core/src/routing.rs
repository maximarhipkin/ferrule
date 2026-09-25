//! M25 routing, Phase 1: start every turn on the cheap tier and move up a
//! tier only when the loop sees a failure (see `docs/m25-routing.md`).
//!
//! The loop reports [`Signal`]s to its provider ([`crate::Provider::escalate`]);
//! a provider that routes keeps a [`Ladder`] and decides. [`Tiered`] is the
//! plain version over a list of providers, for eval and tests; the CLI's
//! routed provider keeps its own ladder over M21's models.

use crate::error::{CoreError, FailureClass};
use crate::provider::{CompletionRequest, CompletionResponse, FailOver, Provider, Served};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// A failure the loop saw, reported to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// A call failed with an error retrying the same model won't fix.
    CallFailed(FailureClass),
    /// Invalid tool calls in a row (an unknown tool, or arguments that
    /// don't fit its schema).
    ToolErrors(u32),
    /// A verify check failed when the model said it was done.
    CheckFailed,
    /// A Stop hook sent the answer back.
    StopHook,
    /// The same tool with the same arguments this many times in a row.
    Repeated(u32),
    /// The stuck detector's nudge.
    Stuck,
    /// The gateway's watchdog saw the session stall.
    Watchdog,
    /// The owner asked for the strong tier (`/model strong`).
    Owner,
}

impl Signal {
    /// The reason recorded in the ledger and the audit.
    pub fn reason(&self) -> String {
        match self {
            Signal::CallFailed(c) => format!("call_failed:{}", class_name(*c)),
            Signal::ToolErrors(_) => "tool_errors".into(),
            Signal::CheckFailed => "check_failed".into(),
            Signal::StopHook => "stop_hook".into(),
            Signal::Repeated(_) | Signal::Stuck => "no_progress".into(),
            Signal::Watchdog => "watchdog".into(),
            Signal::Owner => "owner".into(),
        }
    }
}

/// `FailureClass` as the ledger writes it.
pub fn class_name(c: FailureClass) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

/// Whether a failed call is one a stronger model may not repeat: the
/// answer was malformed, too long for the window, refused, rejected as a
/// bad request, or the model can't call tools. Outages and auth aren't.
pub fn escalates_on(error: &CoreError) -> Option<FailureClass> {
    let class = error.class();
    match class {
        FailureClass::Malformed
        | FailureClass::ContextTooLong
        | FailureClass::Refused
        | FailureClass::BadRequest => Some(class),
        FailureClass::ModelNotFound
            if error
                .to_string()
                .to_ascii_lowercase()
                .contains("support tool use") =>
        {
            Some(class)
        }
        _ => None,
    }
}

/// A move up the ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    /// The tier's name before and after.
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// What a ledger row records about routing: the tier that answered, and
/// on the first call after an escalation, why it moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTag {
    pub tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated: Option<String>,
}

/// Which signals escalate, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Back to the floor at the start of the next turn.
    pub de_escalate: bool,
    pub call_failed: bool,
    /// Invalid tool calls in a row that escalate; 0 = never.
    pub tool_errors: u32,
    pub checks: bool,
    pub stop_hooks: bool,
    /// Identical tool calls in a row that escalate; 0 = never (the stuck
    /// detector's nudge still counts while this is on).
    pub no_progress: u32,
    pub watchdog: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            de_escalate: true,
            call_failed: true,
            tool_errors: 2,
            checks: true,
            stop_hooks: true,
            no_progress: 3,
            watchdog: true,
        }
    }
}

impl Policy {
    fn fires(&self, s: &Signal) -> bool {
        match s {
            Signal::CallFailed(_) => self.call_failed,
            Signal::ToolErrors(n) => self.tool_errors > 0 && *n >= self.tool_errors,
            Signal::CheckFailed => self.checks,
            Signal::StopHook => self.stop_hooks,
            Signal::Repeated(n) => self.no_progress > 0 && *n >= self.no_progress,
            Signal::Stuck => self.no_progress > 0,
            Signal::Watchdog => self.watchdog,
            Signal::Owner => true,
        }
    }
}

/// One agent's place on the tiers: the floor its turns start on, where it
/// is now, and why it last moved. Sticky within a turn: only
/// [`Ladder::begin_turn`] brings it down.
#[derive(Debug, Clone)]
pub struct Ladder {
    tiers: Vec<String>,
    floor: usize,
    level: usize,
    policy: Policy,
    /// Set by an escalation, taken by the next call's row.
    pending: Option<String>,
}

impl Ladder {
    pub fn new(tiers: Vec<String>, floor: usize, policy: Policy) -> Self {
        let floor = floor.min(tiers.len().saturating_sub(1));
        Self {
            tiers,
            floor,
            level: floor,
            policy,
            pending: None,
        }
    }

    pub fn tiers(&self) -> &[String] {
        &self.tiers
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn floor(&self) -> usize {
        self.floor
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// A new turn: back to the floor (unless `de_escalate` is off), then
    /// straight to the top if the owner asked for it.
    pub fn begin_turn(&mut self, force_top: bool) -> Option<Escalation> {
        if self.policy.de_escalate {
            self.level = self.floor;
            self.pending = None;
        }
        let top = self.tiers.len().saturating_sub(1);
        if force_top && self.level < top {
            let from = self.tiers[self.level].clone();
            self.level = top;
            let reason = Signal::Owner.reason();
            self.pending = Some(reason.clone());
            return Some(Escalation {
                from,
                to: self.tiers[top].clone(),
                reason,
            });
        }
        None
    }

    /// One tier up for `signal`, if the policy says it counts, there is a
    /// tier above, and `allow` lets it (the spend cap; it gets the tier
    /// index it would move to).
    pub fn escalate(
        &mut self,
        signal: &Signal,
        allow: impl FnOnce(usize) -> bool,
    ) -> Option<Escalation> {
        if !self.policy.fires(signal) || self.level + 1 >= self.tiers.len() {
            return None;
        }
        let to = self.level + 1;
        if !allow(to) {
            return None;
        }
        let from = self.tiers[self.level].clone();
        self.level = to;
        let reason = signal.reason();
        self.pending = Some(reason.clone());
        Some(Escalation {
            from,
            to: self.tiers[to].clone(),
            reason,
        })
    }

    /// Back down to `level` (the spend cap closed a tier the floor is on).
    pub fn clamp(&mut self, level: usize) {
        self.level = self.level.min(level);
    }

    /// The tier this call goes to, and the row's tag: the reason rides on
    /// the first call after a move.
    pub fn serve(&mut self) -> (usize, RouteTag) {
        (
            self.level,
            RouteTag {
                tier: self.tiers[self.level].clone(),
                escalated: self.pending.take(),
            },
        )
    }
}

/// One tier of a [`Tiered`] provider.
pub struct Tier {
    pub name: String,
    pub provider: Arc<dyn Provider>,
    /// The model it runs, for ledger rows.
    pub served: Served,
}

/// A provider over tiers, cheap first: routing with nothing else attached
/// (no pins, no fallback list, no cap). `ferrule eval --variant routing`
/// uses it, and the tests.
pub struct Tiered {
    name: String,
    tiers: Vec<Tier>,
    state: Mutex<(Ladder, Option<RouteTag>)>,
}

impl Tiered {
    pub fn new(name: impl Into<String>, tiers: Vec<Tier>, policy: Policy) -> Self {
        let names = tiers.iter().map(|t| t.name.clone()).collect();
        Self {
            name: name.into(),
            tiers,
            state: Mutex::new((Ladder::new(names, 0, policy), None)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Ladder, Option<RouteTag>)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The tier the next call goes to.
    pub fn level(&self) -> usize {
        self.lock().0.level()
    }
}

#[async_trait::async_trait]
impl Provider for Tiered {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        self.complete_routed(req).await.1
    }

    async fn complete_routed(
        &self,
        req: CompletionRequest,
    ) -> (Option<Served>, Result<CompletionResponse, CoreError>) {
        let level = {
            let mut st = self.lock();
            let (level, tag) = st.0.serve();
            st.1 = Some(tag);
            level
        };
        let tier = &self.tiers[level];
        let (served, result) = tier.provider.complete_routed(req).await;
        (Some(served.unwrap_or_else(|| tier.served.clone())), result)
    }

    fn fail_over(&self, served: Option<&Served>, error: &CoreError) -> Option<FailOver> {
        let level = self.level();
        self.tiers[level].provider.fail_over(served, error)
    }

    fn routes(&self) -> bool {
        true
    }

    fn begin_turn(&self) {
        self.lock().0.begin_turn(false);
    }

    fn escalate(&self, signal: &Signal) -> Option<Escalation> {
        self.lock().0.escalate(signal, |_| true)
    }

    fn route_tag(&self) -> Option<RouteTag> {
        self.lock().1.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(n: usize, floor: usize) -> Ladder {
        let names = (0..n).map(|i| format!("t{i}")).collect();
        Ladder::new(names, floor, Policy::default())
    }

    #[test]
    fn one_tier_up_per_signal_and_none_past_the_top() {
        let mut l = ladder(3, 0);
        let e = l.escalate(&Signal::CheckFailed, |_| true).unwrap();
        assert_eq!((e.from.as_str(), e.to.as_str()), ("t0", "t1"));
        assert_eq!(e.reason, "check_failed");
        assert_eq!(l.serve().1.escalated.as_deref(), Some("check_failed"));
        assert_eq!(l.serve().1.escalated, None, "the reason rides once");
        l.escalate(&Signal::StopHook, |_| true).unwrap();
        assert!(l.escalate(&Signal::StopHook, |_| true).is_none());
        assert_eq!(l.level(), 2);
    }

    #[test]
    fn thresholds_and_switches_decide_what_counts() {
        let mut l = ladder(2, 0);
        assert!(l.escalate(&Signal::ToolErrors(1), |_| true).is_none());
        assert!(l.escalate(&Signal::Repeated(2), |_| true).is_none());
        assert!(l.escalate(&Signal::Repeated(3), |_| true).is_some());
        let mut off = Ladder::new(
            vec!["a".into(), "b".into()],
            0,
            Policy {
                checks: false,
                ..Policy::default()
            },
        );
        assert!(off.escalate(&Signal::CheckFailed, |_| true).is_none());
        assert!(off.escalate(&Signal::ToolErrors(2), |_| false).is_none());
        assert_eq!(off.level(), 0, "the cap said no");
    }

    #[test]
    fn a_turn_starts_at_the_floor_unless_de_escalation_is_off() {
        let mut l = ladder(3, 1);
        assert_eq!(l.level(), 1);
        l.escalate(&Signal::Watchdog, |_| true).unwrap();
        l.begin_turn(false);
        assert_eq!(l.level(), 1);
        let e = l.begin_turn(true).unwrap();
        assert_eq!((e.to.as_str(), e.reason.as_str()), ("t2", "owner"));
        let mut sticky = Ladder::new(
            vec!["a".into(), "b".into()],
            0,
            Policy {
                de_escalate: false,
                ..Policy::default()
            },
        );
        sticky.escalate(&Signal::CheckFailed, |_| true).unwrap();
        sticky.begin_turn(false);
        assert_eq!(sticky.level(), 1);
    }

    #[test]
    fn only_failures_a_stronger_model_may_fix_escalate() {
        let yes = [
            CoreError::MalformedResponse("no choices".into()),
            CoreError::Provider("HTTP 400: prompt is too long".into()),
            CoreError::Provider("refused: the model declined".into()),
            CoreError::Provider("HTTP 404: No endpoints found that support tool use".into()),
            CoreError::Provider("HTTP 422: bad tool schema".into()),
        ];
        for e in yes {
            assert!(escalates_on(&e).is_some(), "{e}");
        }
        let no = [
            CoreError::Provider("HTTP 404: model not found".into()),
            CoreError::Provider("HTTP 401: invalid key".into()),
            CoreError::Transient {
                message: "HTTP 503".into(),
                retry_after: None,
            },
            CoreError::ToolFailed {
                tool: "shell".into(),
                message: "exit 1".into(),
            },
        ];
        for e in no {
            assert!(escalates_on(&e).is_none(), "{e}");
        }
        assert_eq!(
            Signal::CallFailed(FailureClass::ContextTooLong).reason(),
            "call_failed:context_too_long"
        );
    }
}
