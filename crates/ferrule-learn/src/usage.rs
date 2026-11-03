//! What helps (`docs/m16-learning-loop.md`, "Measuring what helps"): each
//! agent's runs and the skills it activated, one line each in
//! `learn/usage.jsonl`, and the numbers the learning page and the pass
//! read off them. A *session* here is one agent from its first run to
//! its last (a chat until `/new` or the lane is retired, one scheduled
//! run): its prompt, and so its lessons, don't change in between.
//!
//! The numbers are signals, not proof: a lesson in a session that went
//! badly may have had nothing to do with it.

use crate::files::LearnDir;
use crate::thumbs::Thumb;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

/// A run that ended with its work done.
pub const DONE: &str = "done";
/// A run that ended before it: incomplete, stopped, failed.
pub const NOT_DONE: &str = "not_done";
/// A lesson needs this many sessions before its record can count against
/// it when the pass must cut one.
pub const MIN_SESSIONS: usize = 2;

/// One line of `learn/usage.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Use {
    /// A run ended.
    Run {
        /// Unix seconds.
        at: i64,
        /// The chat's or task's session (`<channel>__<chat>`).
        session: String,
        /// This agent, apart from the session's earlier and later ones.
        instance: String,
        /// [`DONE`] or [`NOT_DONE`].
        outcome: String,
        /// The `[pb-N]` lessons its prompt held.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        lessons: Vec<u32>,
        /// The prompt's playbook section's hash (FNV-64, hex), as the
        /// harness stamp has it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playbook: Option<String>,
    },
    /// `activate_skill` loaded a skill.
    Skill {
        at: i64,
        session: String,
        instance: String,
        skill: String,
    },
}

/// A lesson's record, in sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LessonUses {
    /// Sessions whose prompt held it.
    pub sessions: usize,
    /// Of those, the ones with a run that ended not done.
    pub failed: usize,
    /// Of those, the ones the owner gave a 👎.
    pub thumbs_down: usize,
}

impl LessonUses {
    /// How badly it went where it was used, when there is enough of a
    /// record to say: (bad per thousand sessions, bad), where bad is its
    /// sessions that failed plus those with a 👎 (at most all of them).
    pub fn badness(&self) -> Option<(usize, usize)> {
        let bad = (self.failed + self.thumbs_down).min(self.sessions);
        (self.sessions >= MIN_SESSIONS && bad > 0).then(|| (bad * 1000 / self.sessions, bad))
    }
}

/// A skill's record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkillUses {
    pub activations: usize,
    /// Sessions that activated it.
    pub sessions: usize,
    /// Of those, the ones where the run that activated it ended not done.
    pub failed: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub lessons: BTreeMap<u32, LessonUses>,
    pub skills: BTreeMap<String, SkillUses>,
}

impl LearnDir {
    pub fn usage_path(&self) -> PathBuf {
        self.root().join("usage.jsonl")
    }

    /// Appends `u` to `learn/usage.jsonl`, bounded as
    /// [`crate::files::append_log`] says.
    pub fn record_use(&self, u: &Use) -> Result<(), String> {
        let line = serde_json::to_string(u).map_err(|e| e.to_string())?;
        crate::files::append_log(&self.usage_path(), &line)
    }

    /// Every use kept, oldest first; unreadable lines skipped.
    pub fn uses(&self) -> Vec<Use> {
        fs::read_to_string(self.usage_path())
            .map(|t| {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The records of every lesson and skill used, with the owner's
    /// thumbs as they are now.
    pub fn usage_stats(&self) -> Stats {
        stats(&self.uses(), &self.thumbs())
    }
}

#[derive(Default)]
struct Instance {
    session: String,
    lessons: BTreeSet<u32>,
    /// (at, done) of each run, in order.
    runs: Vec<(i64, bool)>,
    thumbed_down: bool,
}

/// The records `uses` and `thumbs` make.
pub fn stats(uses: &[Use], thumbs: &[Thumb]) -> Stats {
    let mut inst: HashMap<&str, Instance> = HashMap::new();
    for u in uses {
        if let Use::Run {
            at,
            session,
            instance,
            outcome,
            lessons,
            ..
        } = u
        {
            let i = inst.entry(instance).or_default();
            i.session.clone_from(session);
            i.lessons.extend(lessons);
            i.runs.push((*at, outcome == DONE));
        }
    }
    for i in inst.values_mut() {
        i.runs.sort_unstable();
    }
    // A 👎 is on the last reply before it: the run of that session's
    // agent that ended last before it.
    for t in thumbs.iter().filter(|t| !t.up) {
        let latest = inst
            .iter()
            .filter(|(_, i)| i.session == t.session)
            .filter_map(|(k, i)| {
                let last = i.runs.iter().map(|r| r.0).filter(|a| *a <= t.at).max()?;
                Some((last, *k))
            })
            .max();
        if let Some((_, k)) = latest {
            if let Some(i) = inst.get_mut(k) {
                i.thumbed_down = true;
            }
        }
    }
    let mut out = Stats::default();
    for i in inst.values() {
        let failed = i.runs.iter().any(|r| !r.1);
        for l in &i.lessons {
            let e = out.lessons.entry(*l).or_default();
            e.sessions += 1;
            e.failed += usize::from(failed);
            e.thumbs_down += usize::from(i.thumbed_down);
        }
    }
    // Skills: the run an activation happened in is the instance's first
    // that ended at or after it.
    let mut seen: BTreeMap<&str, (BTreeSet<&str>, BTreeSet<&str>)> = BTreeMap::new();
    for u in uses {
        if let Use::Skill {
            at,
            instance,
            skill,
            ..
        } = u
        {
            let e = out.skills.entry(skill.clone()).or_default();
            e.activations += 1;
            let (all, bad) = seen.entry(skill).or_default();
            all.insert(instance);
            let done = inst
                .get(instance.as_str())
                .and_then(|i| i.runs.iter().find(|r| r.0 >= *at))
                .map(|r| r.1);
            if done == Some(false) {
                bad.insert(instance);
            }
        }
    }
    for (skill, (all, bad)) in seen {
        if let Some(e) = out.skills.get_mut(skill) {
            e.sessions = all.len();
            e.failed = bad.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(at: i64, session: &str, instance: &str, done: bool, lessons: &[u32]) -> Use {
        Use::Run {
            at,
            session: session.into(),
            instance: instance.into(),
            outcome: if done { DONE } else { NOT_DONE }.into(),
            lessons: lessons.to_vec(),
            playbook: None,
        }
    }

    fn down(at: i64, session: &str) -> Thumb {
        Thumb {
            at,
            session: session.into(),
            message: "9".into(),
            up: false,
            transcript: None,
            why: None,
        }
    }

    #[test]
    fn sessions_failures_thumbs_and_activations_are_counted_per_agent() {
        let uses = vec![
            run(10, "telegram__1", "a", true, &[1, 2]),
            run(20, "telegram__1", "a", false, &[1, 2]),
            run(30, "telegram__1", "b", true, &[2]),
            run(40, "dashboard__owner", "c", true, &[1]),
            Use::Skill {
                at: 35,
                session: "dashboard__owner".into(),
                instance: "c".into(),
                skill: "deploy".into(),
            },
            Use::Skill {
                at: 15,
                session: "telegram__1".into(),
                instance: "a".into(),
                skill: "deploy".into(),
            },
            Use::Skill {
                at: 16,
                session: "telegram__1".into(),
                instance: "a".into(),
                skill: "deploy".into(),
            },
        ];
        // On instance b's reply (the latest before it in that chat).
        let s = stats(&uses, &[down(31, "telegram__1")]);
        assert_eq!(
            s.lessons[&1],
            LessonUses {
                sessions: 2,
                failed: 1,
                thumbs_down: 0
            }
        );
        assert_eq!(
            s.lessons[&2],
            LessonUses {
                sessions: 2,
                failed: 1,
                thumbs_down: 1
            }
        );
        assert_eq!(
            s.skills["deploy"],
            SkillUses {
                activations: 3,
                sessions: 2,
                failed: 1
            }
        );
        assert_eq!(s.lessons[&2].badness(), Some((1000, 2)));
        assert_eq!(s.lessons[&1].badness(), Some((500, 1)));
        assert_eq!(LessonUses::default().badness(), None);
    }

    #[test]
    fn the_log_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(d.path());
        dir.record_use(&run(1, "s", "i", true, &[3])).unwrap();
        assert_eq!(dir.uses().len(), 1);
        assert_eq!(dir.usage_stats().lessons[&3].sessions, 1);
    }
}
