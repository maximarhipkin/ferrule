//! M28: skills that load when a person's message names them
//! (docs/skills.md). A skill lists words and phrases under `triggers:`;
//! matching is whole words, case- and accent-insensitive, with Hebrew
//! proclitic prefixes allowed. No regex: a pattern that fires on every
//! message is exactly what this mustn't allow.
//!
//! [`SkillTriggers`] is the agent's [`PromptTriggers`]: it matches, vets,
//! renders and applies the turn's limits. The agent only ever asks it about
//! a message a person typed.

use crate::discover::{Scope, Skill};
use crate::tool::{render_activation, SkillsHandle};
use ferrule_core::{PromptTriggers, TriggerLoad, Triggered};
use icu_normalizer::DecomposingNormalizerBorrowed;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// At most this many triggers a skill; the rest are dropped with a warning.
pub const TRIGGERS_MAX: usize = 20;
/// A trigger shorter than this, normalized, is dropped with a warning.
pub const TRIGGER_MIN_CHARS: usize = 2;

/// Letters that attach to the front of a Hebrew word: and, the, in, to,
/// from, that, as/when.
const HEBREW_PREFIXES: &str = "והבלמשכ";
/// `וכשה` ("and when the") is four.
const HEBREW_PREFIX_MAX: usize = 4;

/// The words of `text`: NFD, lowercased, combining marks (accents, niqqud,
/// cantillation) dropped, split on anything that isn't a letter or digit.
/// An apostrophe or geresh between two letters stays in the word, as `'`;
/// gershayim (or `"`) between two letters, as `"`.
pub fn words(text: &str) -> Vec<String> {
    let nfd = DecomposingNormalizerBorrowed::new_nfd().normalize(text);
    let chars: Vec<char> = nfd
        .chars()
        .filter(|c| !is_mark(*c))
        .flat_map(char::to_lowercase)
        .collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_alphanumeric() {
            word.push(c);
            continue;
        }
        let inner = word.chars().last().is_some_and(char::is_alphabetic)
            && chars.get(i + 1).is_some_and(|n| n.is_alphabetic());
        match c {
            '\'' | '\u{2019}' | '\u{05F3}' if inner => word.push('\''),
            '"' | '\u{05F4}' if inner => word.push('"'),
            _ if !word.is_empty() => out.push(std::mem::take(&mut word)),
            _ => {}
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

fn is_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
        | '\u{0591}'..='\u{05BD}'
        | '\u{05BF}'
        | '\u{05C1}'..='\u{05C2}'
        | '\u{05C4}'..='\u{05C5}'
        | '\u{05C7}')
}

fn is_hebrew_letter(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c)
}

/// A message word matches a trigger word when they're equal, or, for a
/// Hebrew trigger word, when the message word is it behind 1-4 prefix
/// letters (`השחרור`, `וכשהשחרור` for `שחרור`).
fn word_matches(message: &str, trigger: &str) -> bool {
    if message == trigger {
        return true;
    }
    if !trigger.chars().next().is_some_and(is_hebrew_letter) {
        return false;
    }
    let Some(prefix) = message.strip_suffix(trigger) else {
        return false;
    };
    let n = prefix.chars().count();
    (1..=HEBREW_PREFIX_MAX).contains(&n) && prefix.chars().all(|c| HEBREW_PREFIXES.contains(c))
}

/// Where in `message` (a word index) the trigger's words first appear in a
/// row, if they do.
pub fn find(message: &[String], trigger: &[String]) -> Option<usize> {
    if trigger.is_empty() || trigger.len() > message.len() {
        return None;
    }
    (0..=message.len() - trigger.len()).find(|&at| {
        trigger
            .iter()
            .zip(&message[at..])
            .all(|(t, m)| word_matches(m, t))
    })
}

/// A skill's `triggers:` entries, checked: the kept ones, and why the rest
/// were dropped.
pub fn validate(raw: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut problems = Vec::new();
    for t in raw {
        let len: usize = words(&t).iter().map(|w| w.chars().count()).sum();
        if len < TRIGGER_MIN_CHARS {
            problems.push(format!(
                "trigger `{t}` dropped: under {TRIGGER_MIN_CHARS} letters or digits"
            ));
        } else if kept.len() == TRIGGERS_MAX {
            problems.push(format!(
                "trigger `{t}` dropped: at most {TRIGGERS_MAX} triggers a skill"
            ));
        } else if !kept.contains(&t) {
            kept.push(t);
        }
    }
    (kept, problems)
}

/// Checks a skill is fit to load on its own: the extension scan, the lock.
/// `Err` says why not.
pub type Vet = Arc<dyn Fn(&Skill) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct TriggerOptions {
    /// Most skills a message loads (or notes as too large).
    pub max_triggered: usize,
    /// Their combined rendered size, at 4 characters a token.
    pub budget_tokens: usize,
    /// Whether project-scope skills (from the workspace) may trigger.
    pub project: bool,
    pub vet: Vet,
}

/// The agent's [`PromptTriggers`] over a live skill set.
pub struct SkillTriggers {
    skills: SkillsHandle,
    /// Shared with `activate_skill`, which then says "already active".
    active: Arc<Mutex<HashSet<String>>>,
    opts: TriggerOptions,
}

impl SkillTriggers {
    pub(crate) fn new(
        skills: SkillsHandle,
        active: Arc<Mutex<HashSet<String>>>,
        opts: TriggerOptions,
    ) -> Self {
        Self {
            skills,
            active,
            opts,
        }
    }

    /// Skills `prompt` names and that may trigger, earliest match first,
    /// then by name, with the trigger that matched.
    fn matches(&self, prompt: &str, loaded: &[String]) -> Vec<(Skill, String)> {
        let message = words(prompt);
        let set = self.skills.get();
        let mut found: Vec<(usize, Skill, String)> = set
            .invocable()
            .filter(|s| s.scope == Scope::User || self.opts.project)
            .filter(|s| !loaded.contains(&s.name))
            .filter_map(|s| {
                s.triggers
                    .iter()
                    .filter_map(|t| find(&message, &words(t)).map(|at| (at, t)))
                    .min_by_key(|(at, _)| *at)
                    .map(|(at, t)| (at, s.clone(), t.clone()))
            })
            .collect();
        found.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));
        found.into_iter().map(|(_, s, t)| (s, t)).collect()
    }

    fn load(&self, skill: &Skill) -> Result<String, String> {
        (self.opts.vet)(skill)?;
        let text = std::fs::read_to_string(&skill.location)
            .map_err(|e| format!("{}: {e}", skill.location.display()))?;
        let body = crate::frontmatter::parse(&text)?.body;
        Ok(render_activation(skill, &body))
    }
}

impl PromptTriggers for SkillTriggers {
    fn triggered(&self, prompt: &str, loaded: &[String]) -> Vec<Triggered> {
        let mut out = Vec::new();
        let mut budget = self.opts.budget_tokens.saturating_mul(4);
        let mut counted = 0;
        for (skill, matched) in self.matches(prompt, loaded) {
            if counted == self.opts.max_triggered {
                tracing::info!(
                    skill = %skill.name,
                    "skill trigger matched but {} already loaded this turn",
                    self.opts.max_triggered
                );
                continue;
            }
            let load = match self.load(&skill) {
                Err(why) => TriggerLoad::Refused(why),
                Ok(block) if block.chars().count() > budget => {
                    counted += 1;
                    TriggerLoad::TooLarge
                }
                Ok(block) => {
                    counted += 1;
                    budget -= block.chars().count();
                    self.active.lock().unwrap().insert(skill.name.clone());
                    TriggerLoad::Loaded(block)
                }
            };
            out.push(Triggered {
                name: skill.name,
                matched,
                load,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(message: &str, trigger: &str) -> Option<usize> {
        find(&words(message), &words(trigger))
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(at("Ship it!", "ship"), Some(0));
        assert_eq!(at("we SHIP today", "ship"), Some(1));
        assert_eq!(at("shipping today", "ship"), None);
        assert_eq!(at("reship it", "ship"), None);
        assert_eq!(at("ships", "ship"), None);
        assert_eq!(at("the ship-date", "ship"), Some(1));
    }

    #[test]
    fn phrases_match_consecutive_words() {
        assert_eq!(at("ok, ship it now", "ship it"), Some(1));
        assert_eq!(at("SHIP, it", "ship it"), Some(0));
        assert_eq!(at("ship this", "ship it"), None);
        assert_eq!(at("it ship", "ship it"), None);
        assert_eq!(at("ship", "ship it"), None);
    }

    #[test]
    fn case_accents_and_apostrophes() {
        assert_eq!(at("Café opens", "cafe"), Some(0));
        // Precomposed and decomposed forms are the same word.
        assert_eq!(at("cafe\u{0301}", "caf\u{00E9}"), Some(0));
        assert_eq!(at("ÜBER alles", "über"), Some(0));
        assert_eq!(at("don’t deploy", "don't"), Some(0));
        assert_eq!(at("don t", "don't"), None);
        assert_eq!(at("'quoted'", "quoted"), Some(0));
        assert_eq!(words("צה״ל"), words("צה\"ל"));
        assert_eq!(words("צ׳יפס"), words("צ'יפס"));
        assert_eq!(words("עוד 5 דקות"), ["עוד", "5", "דקות"]);
    }

    #[test]
    fn hebrew_prefixes_but_not_suffixes() {
        for m in [
            "שחרור",
            "השחרור",
            "ושחרור",
            "בשחרור",
            "לשחרור",
            "מהשחרור",
            "כשהשחרור",
            "וכשהשחרור מחר",
        ] {
            assert_eq!(at(m, "שחרור"), Some(0), "{m}");
        }
        for m in ["שחרורים", "אשחרור", "תשחרור", "ולכשהשחרור"] {
            assert_eq!(at(m, "שחרור"), None, "{m}");
        }
        // Niqqud doesn't matter.
        assert_eq!(at("הַשִּׁחְרוּר", "שחרור"), Some(0));
        // Each word of a phrase takes its own article.
        assert_eq!(at("תריץ את הבדיקה המהירה", "בדיקה מהירה"), Some(2));
        // A prefixed trigger doesn't match the bare word.
        assert_eq!(at("שחרור", "השחרור"), None);
        // Latin triggers take no prefixes.
        assert_eq!(at("וship", "ship"), None);
    }

    use crate::discover::SkillRoot;
    use crate::LiveSkillTools;
    use ferrule_core::tool::{ToolContext, ToolSource};

    fn skill(root: &std::path::Path, name: &str, triggers: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\ntriggers: {triggers}\n---\n{body}\n"),
        )
        .unwrap();
    }

    struct Setup {
        _user: tempfile::TempDir,
        _project: tempfile::TempDir,
        live: LiveSkillTools,
    }

    fn setup() -> Setup {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        skill(
            user.path(),
            "release",
            "[ship it, release]",
            "RELEASE-STEPS",
        );
        skill(user.path(), "deploy", "[deploy]", "DEPLOY-STEPS");
        skill(user.path(), "big", "[huge]", &"x".repeat(20_000));
        skill(user.path(), "bad", "[bad]", "BAD-STEPS");
        skill(project.path(), "repo", "[the repo]", "REPO-STEPS");
        let dir = user.path().join("hidden");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: hidden\ndescription: d\ntriggers: [hidden]\ndisable-model-invocation: true\n---\nH\n",
        )
        .unwrap();
        let handle = SkillsHandle::discovering(
            vec![
                SkillRoot {
                    dir: project.path().to_path_buf(),
                    scope: Scope::Project,
                },
                SkillRoot {
                    dir: user.path().to_path_buf(),
                    scope: Scope::User,
                },
            ],
            vec![],
        );
        Setup {
            _user: user,
            _project: project,
            live: LiveSkillTools::new(handle),
        }
    }

    fn opts(max: usize, project: bool) -> TriggerOptions {
        TriggerOptions {
            max_triggered: max,
            budget_tokens: 4000,
            project,
            vet: Arc::new(|s: &Skill| match s.name.as_str() {
                "bad" => Err("scan: blocked".into()),
                _ => Ok(()),
            }),
        }
    }

    fn summary(t: &[Triggered]) -> Vec<(String, String, &'static str)> {
        t.iter()
            .map(|t| {
                let load = match &t.load {
                    TriggerLoad::Loaded(_) => "loaded",
                    TriggerLoad::TooLarge => "too large",
                    TriggerLoad::Refused(_) => "refused",
                };
                (t.name.clone(), t.matched.clone(), load)
            })
            .collect()
    }

    #[test]
    fn earliest_match_first_up_to_the_limit_and_the_budget() {
        let s = setup();
        let triggers = s.live.triggers(opts(2, false));
        let got = triggers.triggered("Deploy, then SHIP IT, then release", &[]);
        assert_eq!(
            summary(&got),
            [
                ("deploy".into(), "deploy".into(), "loaded"),
                ("release".into(), "ship it".into(), "loaded"),
            ]
        );
        let TriggerLoad::Loaded(block) = &got[0].load else {
            unreachable!()
        };
        assert!(block.starts_with("<skill_content name=\"deploy\">"));
        assert!(block.contains("DEPLOY-STEPS"));

        // The limit: the third match doesn't load. Over the budget: noted.
        let got = triggers.triggered("huge release now, deploy", &[]);
        assert_eq!(
            summary(&got),
            [
                ("big".into(), "huge".into(), "too large"),
                ("release".into(), "release".into(), "loaded"),
            ]
        );
        // Already in context: not again.
        let got = triggers.triggered("release", &["release".to_string()]);
        assert!(got.is_empty());
    }

    #[test]
    fn unvetted_hidden_and_project_skills_dont_load() {
        let s = setup();
        let got = s
            .live
            .triggers(opts(5, false))
            .triggered("bad hidden, in the repo", &[]);
        assert_eq!(summary(&got), [("bad".into(), "bad".into(), "refused")]);
        let got = s.live.triggers(opts(5, true)).triggered("in the repo", &[]);
        assert_eq!(
            summary(&got),
            [("repo".into(), "the repo".into(), "loaded")]
        );
    }

    #[tokio::test]
    async fn activate_skill_knows_what_a_trigger_loaded() {
        let s = setup();
        s.live.triggers(opts(2, false)).triggered("deploy", &[]);
        let activate = s
            .live
            .tools()
            .into_iter()
            .find(|t| t.definition().name == crate::ACTIVATE_TOOL)
            .unwrap();
        let out = activate
            .call(
                serde_json::json!({"name": "deploy"}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("already active"), "{}", out.content);
    }

    #[test]
    fn validation_drops_short_and_extra_triggers() {
        let mut raw: Vec<String> = ["a", "!!", "ok", "ב", "ok"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        raw.extend((0..25).map(|i| format!("word{i}")));
        let (kept, problems) = validate(raw);
        assert_eq!(kept.len(), TRIGGERS_MAX);
        assert_eq!(kept[0], "ok");
        assert_eq!(problems.len(), 3 + 6, "{problems:?}");
        assert!(problems[0].contains("`a`"));
    }
}
