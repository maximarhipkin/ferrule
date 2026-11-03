//! M66: routing that remembers (docs/routing.md, "Remembering"). Per scope
//! (a chat, or a scheduled task) the last few turns say whether each had
//! to move up a tier. A scope where most of them did starts its turns one
//! tier above its floor; after a run of turns there that moved no further,
//! it tries its floor again. The owner can reset a scope. Kept small in
//! `<data>/models/routing-memory.json`.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

/// The turns a scope's count looks at.
pub const WINDOW: usize = 5;
/// Of those, how many moved up before the scope starts one tier higher.
pub const RAISE_AT: usize = 3;
/// Turns in a row on the higher start that moved no further before it
/// tries its floor again (and its count starts over).
pub const EASE_AFTER: u32 = 5;
/// The most scopes kept: the least recently used go first.
pub const MAX_SCOPES: usize = 200;
/// The escalations kept for "why it moved up", newest last.
pub const RECENT: usize = 20;

/// Now, in one width (microseconds, `Z`), so stamps sort as they happened.
fn stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// One turn of a scope: whether it moved up for a need (a signal, the
/// watchdog; not the owner's `/model strong`), and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub escalated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeMemory {
    /// The last [`WINDOW`] turns, oldest first.
    pub turns: VecDeque<Turn>,
    /// The tier its turns start on above its floor, while raised.
    pub raised: Option<String>,
    /// When it was raised (RFC 3339, UTC).
    pub since: Option<String>,
    /// Why it was raised, in a sentence.
    pub why: Option<String>,
    /// Turns in a row on the raised start that moved no further.
    pub quiet: u32,
    /// Its floor's tier, as last seen.
    pub floor: String,
    /// RFC 3339, UTC, one width: sorts as it happened.
    pub updated: Option<String>,
}

/// One escalation, for the page's "why it moved up".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    /// RFC 3339, UTC.
    pub at: String,
    pub scope: String,
    pub from: String,
    pub to: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Data {
    scopes: BTreeMap<String, ScopeMemory>,
    recent: VecDeque<Recent>,
}

/// The memory, as read from its file (none: in memory only).
#[derive(Debug, Default)]
pub struct Memory {
    path: Option<PathBuf>,
    data: Data,
}

impl Memory {
    /// The memory kept at `path`; an unreadable file starts it empty.
    pub fn at(path: Option<PathBuf>) -> Self {
        let data = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, data }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&self.data)?)?;
            std::fs::rename(tmp, path)
        };
        if let Err(e) = write() {
            tracing::warn!("routing memory {}: {e}", path.display());
        }
    }

    /// A turn of `scope` begins on `tiers`, its floor `floor`: the tier it
    /// starts on. Before this turn is counted, the last one decides: a
    /// raised scope eases back after [`EASE_AFTER`] quiet turns, and one at
    /// its floor is raised when [`RAISE_AT`] of its last [`WINDOW`] moved up.
    pub fn begin(&mut self, scope: &str, tiers: &[String], floor: usize) -> usize {
        let Some(floor_name) = tiers.get(floor) else {
            return floor;
        };
        let now = stamp();
        let m = self.data.scopes.entry(scope.to_string()).or_default();
        if m.floor != *floor_name {
            // Another floor (a new pin, new tiers): its past doesn't apply.
            *m = ScopeMemory {
                floor: floor_name.clone(),
                ..ScopeMemory::default()
            };
        }
        let raised_to = tiers.get(floor + 1).cloned();
        match (&m.raised, m.turns.back()) {
            (Some(_), Some(last)) if !last.escalated => m.quiet += 1,
            (Some(_), _) => m.quiet = 0,
            _ => {}
        }
        if m.raised.is_some() && (m.quiet >= EASE_AFTER || raised_to != m.raised) {
            m.raised = None;
            m.since = None;
            m.why = None;
            m.quiet = 0;
            m.turns.clear();
        } else if m.raised.is_none() && raised_to.is_some() {
            let up: Vec<&Turn> = m.turns.iter().filter(|t| t.escalated).collect();
            if up.len() >= RAISE_AT {
                let mut reasons: Vec<&str> = up
                    .iter()
                    .flat_map(|t| t.why.iter().map(String::as_str))
                    .collect();
                reasons.sort_unstable();
                reasons.dedup();
                m.why = Some(format!(
                    "{} of its last {} turns moved up ({})",
                    up.len(),
                    m.turns.len(),
                    reasons.join(", ")
                ));
                m.raised = raised_to;
                m.since = Some(now.clone());
                m.quiet = 0;
            }
        }
        m.turns.push_back(Turn::default());
        while m.turns.len() > WINDOW {
            m.turns.pop_front();
        }
        m.updated = Some(now);
        let start = if m.raised.is_some() { floor + 1 } else { floor };
        self.bound();
        self.save();
        start
    }

    /// `scope`'s turn moved up `from` → `to` for `why`. `counts`: a need
    /// (not the owner's own `/model strong`), so the turn counts as moved.
    pub fn escalated(&mut self, scope: &str, from: &str, to: &str, why: &str, counts: bool) {
        if counts {
            if let Some(turn) = self
                .data
                .scopes
                .get_mut(scope)
                .and_then(|m| m.turns.back_mut())
            {
                turn.escalated = true;
                if !turn.why.iter().any(|w| w == why) {
                    turn.why.push(why.to_string());
                }
            }
        }
        self.data.recent.push_back(Recent {
            at: stamp(),
            scope: scope.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            why: why.to_string(),
        });
        while self.data.recent.len() > RECENT {
            self.data.recent.pop_front();
        }
        self.save();
    }

    /// Forgets `scope`: its next turn starts on its floor, counting anew.
    pub fn reset(&mut self, scope: &str) -> bool {
        let had = self.data.scopes.remove(scope).is_some();
        if had {
            self.save();
        }
        had
    }

    /// At most [`MAX_SCOPES`]: the least recently used go.
    fn bound(&mut self) {
        while self.data.scopes.len() > MAX_SCOPES {
            let oldest = self
                .data
                .scopes
                .iter()
                .min_by_key(|(_, m)| m.updated.clone())
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.data.scopes.remove(&k),
                None => break,
            };
        }
    }

    pub fn scopes(&self) -> &BTreeMap<String, ScopeMemory> {
        &self.data.scopes
    }

    /// Newest first.
    pub fn recent(&self) -> impl Iterator<Item = &Recent> {
        self.data.recent.iter().rev()
    }

    #[cfg(test)]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
}

impl ScopeMemory {
    /// The tier its turns start on now.
    pub fn start_tier(&self) -> &str {
        self.raised.as_deref().unwrap_or(&self.floor)
    }

    /// Of its last turns, how many moved up.
    pub fn escalated_last(&self) -> usize {
        self.turns.iter().filter(|t| t.escalated).count()
    }

    /// In a sentence: why it starts where it does.
    pub fn why_now(&self) -> String {
        match (&self.raised, &self.why) {
            (Some(tier), Some(why)) => format!(
                "starts on {tier}: {why}; it tries {} again after {EASE_AFTER} turns there that move no further ({} so far)",
                self.floor, self.quiet
            ),
            _ => format!(
                "starts on its floor, {}: {} of its last {} turns moved up ({RAISE_AT} of {WINDOW} start it a tier higher)",
                self.floor,
                self.escalated_last(),
                self.turns.len()
            ),
        }
    }
}

/// The scope a session's turns are remembered under: its chat
/// (`channel:chat`) or its scheduled task (`task:<id>`), only for the
/// session of that chat or task itself (a sub-agent's are its own, and a
/// terminal run's aren't remembered).
pub fn scope_of(scope: &super::Scope) -> Option<String> {
    if let Some(task) = &scope.task {
        let own =
            ferrule_gateway::session::session_id(ferrule_gateway::SCHEDULER_PSEUDO_CHANNEL, task);
        return (own == scope.session).then(|| format!("task:{task}"));
    }
    let (channel, chat) = scope.chat.as_ref()?;
    let own = ferrule_gateway::session::session_id(channel, chat);
    (own == scope.session).then(|| super::pin_key(channel, chat))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers() -> Vec<String> {
        vec!["cheap".into(), "strong".into()]
    }

    /// Runs `n` turns of `scope`, each moving up when `up(i)`: the start
    /// tier of each.
    fn turns(m: &mut Memory, scope: &str, n: usize, up: impl Fn(usize) -> bool) -> Vec<usize> {
        (0..n)
            .map(|i| {
                let start = m.begin(scope, &tiers(), 0);
                if up(i) {
                    m.escalated(scope, "cheap", "strong", "check_failed", true);
                }
                start
            })
            .collect()
    }

    #[test]
    fn three_of_five_moved_up_start_the_next_a_tier_higher_and_quiet_turns_ease_it_back() {
        let mut m = Memory::default();
        // Two of five: still on the floor.
        assert_eq!(turns(&mut m, "telegram:42", 5, |i| i >= 3), [0; 5]);
        // A third: the next turn starts higher.
        assert_eq!(turns(&mut m, "telegram:42", 1, |_| true), [0]);
        let s = &m.scopes()["telegram:42"];
        assert_eq!((s.escalated_last(), s.turns.len()), (3, 5));
        assert_eq!(m.begin("telegram:42", &tiers(), 0), 1);
        let s = &m.scopes()["telegram:42"];
        assert_eq!(s.start_tier(), "strong");
        assert!(
            s.why_now()
                .contains("3 of its last 5 turns moved up (check_failed)"),
            "{}",
            s.why_now()
        );
        assert!(s.since.is_some());
        // Five turns there (that one and four more) that moved no further,
        // then the floor again, counting anew.
        assert_eq!(turns(&mut m, "telegram:42", 4, |_| false), [1, 1, 1, 1]);
        assert_eq!(m.begin("telegram:42", &tiers(), 0), 0);
        let s = &m.scopes()["telegram:42"];
        assert_eq!((s.raised.clone(), s.turns.len()), (None, 1));
        // The owner's own `/model strong` isn't a need.
        let mut o = Memory::default();
        let starts = (0..6)
            .map(|_| {
                let start = o.begin("t", &tiers(), 0);
                o.escalated("t", "cheap", "strong", "owner", false);
                start
            })
            .collect::<Vec<_>>();
        assert_eq!(starts, [0; 6]);
        assert_eq!(o.recent().count(), 6, "but it's listed");
    }

    #[test]
    fn a_turn_that_moves_up_on_the_raised_start_keeps_it_raised_and_the_top_cant_rise() {
        let mut m = Memory::default();
        turns(&mut m, "c", 3, |_| true);
        assert_eq!(m.begin("c", &tiers(), 0), 1);
        let three = vec!["a".to_string(), "b".into(), "c".into()];
        // A floor already on the top tier has nowhere higher to start.
        let mut top = Memory::default();
        for _ in 0..6 {
            assert_eq!(top.begin("x", &three, 2), 2);
            top.escalated("x", "c", "c", "check_failed", true);
        }
        // Another floor: its past doesn't count.
        assert_eq!(m.begin("c", &three, 1), 1);
        assert_eq!(m.scopes()["c"].turns.len(), 1);
    }

    #[test]
    fn a_reset_forgets_a_scope_and_everything_is_bounded_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models/routing-memory.json");
        let mut m = Memory::at(Some(path.clone()));
        turns(&mut m, "telegram:42", 4, |_| true);
        assert_eq!(m.begin("telegram:42", &tiers(), 0), 1);
        // Kept on disk: a new process remembers.
        let mut again = Memory::at(Some(path.clone()));
        assert_eq!(again.scopes()["telegram:42"].start_tier(), "strong");
        assert!(again.reset("telegram:42"));
        assert!(!again.reset("telegram:42"));
        assert_eq!(again.begin("telegram:42", &tiers(), 0), 0);
        assert!(Memory::at(Some(path.clone())).scopes()["telegram:42"]
            .raised
            .is_none());
        // Bounded: the window, the scopes, the recent list.
        let mut big = Memory::default();
        for i in 0..(MAX_SCOPES + 30) {
            big.begin(&format!("s{i}"), &tiers(), 0);
            big.escalated(&format!("s{i}"), "cheap", "strong", "tool_errors", true);
        }
        assert_eq!(big.scopes().len(), MAX_SCOPES);
        assert!(!big.scopes().contains_key("s0"), "the oldest went");
        assert_eq!(big.recent().count(), RECENT);
        assert_eq!(
            big.recent().next().unwrap().scope,
            format!("s{}", MAX_SCOPES + 29)
        );
        turns(&mut big, "w", 12, |_| false);
        assert_eq!(big.scopes()["w"].turns.len(), WINDOW);
        assert!(again.path().is_some());
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < 16 * 1024, "{size}");
    }
}
