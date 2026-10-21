//! M53.3: skills the learning pass writes (`docs/m53-contrastive-probing.md`,
//! "M53.3"). A `skill` delta is a named procedure: a SKILL.md in the
//! agentskills format ferrule-skills loads, optionally with scripts. It is
//! kept under `<data>/learned-skills/<name>/` only after the draft passes
//! the validators here and the same contrast gate a lesson passes.
//!
//! The checks reuse M13's `skill_keep` path (draft, check, injection scan,
//! lock): ferrule-extensions' `inspect` reads the frontmatter, checks the
//! name and description and scans SKILL.md and its text files; the same
//! scanner reads the scripts; the parse checks run in the sandbox with the
//! draft as their workspace. The lock is `learn/skills.json`, which no
//! agent can read or write: a learned skill loads only while its SKILL.md
//! is the one the pass kept.

use crate::files::{write_atomic, LearnDir};
use ferrule_core::{ToolContext, Verifier};
use ferrule_extensions::scan;
use ferrule_sandbox::Sandbox;
use ferrule_skills::{Scope, SkillRoot};
use ferrule_tools::CommandVerifier;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// `<data>/learned-skills`: a discovery root, after every other.
pub const DIR: &str = "learned-skills";
pub const NAME_MAX: usize = 40;
pub const DESCRIPTION_MIN: usize = 10;
pub const DESCRIPTION_MAX: usize = 300;
pub const BODY_MAX: usize = 8_000;
pub const SCRIPTS_MAX: usize = 5;
pub const SCRIPT_MAX: usize = 8_000;
/// How long one parse check may take.
const PARSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A skill the reflector proposed.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDraft {
    pub name: String,
    /// When to use it: the catalog's one line.
    pub description: String,
    /// The steps, Markdown.
    pub body: String,
    /// `scripts/<file>` → its text.
    pub scripts: BTreeMap<String, String>,
}

/// The agentskills name rules, kept to what both ferrule-skills and M13's
/// installer accept: lowercase letters and digits in hyphen-joined words.
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name.split('-').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// A script's path: `scripts/<file>`, one plain file name.
fn script_path_ok(rel: &str) -> bool {
    rel.strip_prefix("scripts/").is_some_and(|f| {
        !f.is_empty()
            && !f.starts_with('.')
            && f.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    })
}

impl SkillDraft {
    /// For people and the judge: "a skill `name`: description".
    pub fn summary(&self) -> String {
        format!("a skill `{}`: {}", self.name, self.description)
    }

    /// The checks that need nothing on disk: name, sizes, script paths,
    /// and no reach into ferrule's own data.
    pub fn shape(&self) -> Result<(), String> {
        if !name_ok(&self.name) {
            return Err(format!(
                "invalid skill name `{}`: 1-{NAME_MAX} lowercase letters and digits in hyphen-joined words",
                self.name
            ));
        }
        let d = self.description.trim();
        if d.contains(['\n', '\r']) {
            return Err("a skill's description must be one line".into());
        }
        let n = d.chars().count();
        if !(DESCRIPTION_MIN..=DESCRIPTION_MAX).contains(&n) {
            return Err(format!(
                "a skill's description must say when to use it in {DESCRIPTION_MIN}-{DESCRIPTION_MAX} characters (it has {n})"
            ));
        }
        if let Some(what) = crate::screen::denied(d) {
            return Err(format!("the description looks like it contains {what}"));
        }
        let body = self.body.trim();
        if body.is_empty() {
            return Err("a skill needs a body: the steps to follow".into());
        }
        if body.chars().count() > BODY_MAX {
            return Err(format!("the skill's body is over {BODY_MAX} characters"));
        }
        if self.scripts.len() > SCRIPTS_MAX {
            return Err(format!("at most {SCRIPTS_MAX} scripts in a skill"));
        }
        for (rel, text) in &self.scripts {
            if !script_path_ok(rel) {
                return Err(format!(
                    "script path `{rel}`: scripts live at scripts/<file>, one plain file name"
                ));
            }
            if text.trim().is_empty() {
                return Err(format!("{rel} is empty"));
            }
            if text.chars().count() > SCRIPT_MAX {
                return Err(format!("{rel} is over {SCRIPT_MAX} characters"));
            }
        }
        for (what, text) in std::iter::once(("SKILL.md", body))
            .chain(self.scripts.iter().map(|(r, t)| (r.as_str(), t.as_str())))
        {
            if let Some(hit) = crate::screen::ferrule_data(text) {
                return Err(format!("{what} reaches into {hit}"));
            }
        }
        Ok(())
    }

    /// SKILL.md: the frontmatter ferrule-skills reads, then the body.
    pub fn skill_md(&self) -> String {
        let description = self
            .description
            .trim()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!(
            "---\nname: {}\ndescription: \"{description}\"\n---\n\n{}\n",
            self.name,
            self.body.trim()
        )
    }

    /// Writes the draft to `<parent>/<name>/`, replacing what was there.
    pub fn write(&self, parent: &Path) -> Result<PathBuf, String> {
        let dir = parent.join(&self.name);
        let err = |e: std::io::Error| format!("couldn't write the skill draft: {e}");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(err)?;
        fs::write(dir.join("SKILL.md"), self.skill_md()).map_err(err)?;
        for (rel, text) in &self.scripts {
            let path = dir.join(rel);
            fs::create_dir_all(dir.join("scripts")).map_err(err)?;
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            fs::write(&path, text).map_err(err)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(err)?;
            }
        }
        Ok(dir)
    }
}

/// `scripts/…` paths the body names: each must ship with the skill.
pub fn referenced(body: &str) -> BTreeSet<String> {
    body.split(|c: char| c.is_whitespace() || "`'\"()[]<>,;|&".contains(c))
        .map(|t| t.trim_end_matches(['.', ':', '!', '?']))
        .map(|t| t.strip_prefix("./").unwrap_or(t))
        .filter(|t| t.len() > "scripts/".len() && t.starts_with("scripts/"))
        .map(str::to_string)
        .collect()
}

/// The validators, in order (Harness²'s, through M13's path): the
/// frontmatter reads and names a description; nothing the scan blocks, in
/// SKILL.md, its text files or its scripts; the catalog lists it; every
/// script it names ships; every script parses. `Ok` carries notes for the
/// journal (a parse check that couldn't run here).
pub async fn validate(
    dir: &Path,
    draft: &SkillDraft,
    sandbox: Option<&Arc<Sandbox>>,
) -> Result<Vec<String>, String> {
    let c = ferrule_extensions::skill::inspect(dir).map_err(|e| e.to_string())?;
    if c.name != draft.name {
        return Err(format!(
            "SKILL.md names itself `{}`, not `{}`",
            c.name, draft.name
        ));
    }
    let mut findings = c.findings;
    for (rel, text) in &draft.scripts {
        findings.extend(scan::scan_skill_file(&draft.name, rel, text));
    }
    let blocks: Vec<_> = scan::blocks(&findings).cloned().collect();
    if !blocks.is_empty() {
        return Err(format!(
            "the injection scan flagged {}",
            scan::summary_for_model(&blocks)
        ));
    }
    reachable(dir, draft)?;
    for rel in referenced(&draft.body) {
        if !dir.join(&rel).is_file() {
            return Err(format!(
                "SKILL.md names {rel}, which the skill doesn't ship"
            ));
        }
    }
    parse_scripts(dir, draft, sandbox).await
}

/// Reachable the way an agent would reach it: discovery finds it, the
/// model may activate it, its description reads back as written, and the
/// catalog lists it by name.
fn reachable(dir: &Path, draft: &SkillDraft) -> Result<(), String> {
    let root = dir.parent().ok_or("the draft has no parent directory")?;
    let set = ferrule_skills::discover(
        &[SkillRoot {
            dir: root.to_path_buf(),
            scope: Scope::Learned,
        }],
        &[],
    );
    let Some(skill) = set.get(&draft.name).filter(|s| s.model_invocable) else {
        let why: Vec<_> = set.diagnostics.iter().map(|d| d.message.clone()).collect();
        return Err(format!(
            "the skills catalog doesn't list `{}`{}",
            draft.name,
            if why.is_empty() {
                String::new()
            } else {
                format!(": {}", why.join("; "))
            }
        ));
    };
    if skill.description != draft.description.trim() {
        return Err(format!(
            "the catalog reads `{}`'s description as {:?}",
            draft.name, skill.description
        ));
    }
    let listed = set
        .catalog()
        .is_some_and(|c| c.contains(&format!("<name>{}</name>", draft.name)));
    if !listed {
        return Err(format!("the skills catalog doesn't list `{}`", draft.name));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Sh,
    Bash,
    Python,
    Other,
}

fn lang(rel: &str, text: &str) -> Lang {
    let shebang = text.lines().next().unwrap_or("");
    let by_bang = || {
        if !shebang.starts_with("#!") {
            Lang::Other
        } else if shebang.contains("python") {
            Lang::Python
        } else if shebang.contains("bash") {
            Lang::Bash
        } else if shebang.ends_with("/sh") || shebang.contains(" sh") {
            Lang::Sh
        } else {
            Lang::Other
        }
    };
    match rel.rsplit_once('.').map(|(_, e)| e) {
        Some("py") => Lang::Python,
        Some("bash") => Lang::Bash,
        Some("sh") if by_bang() == Lang::Bash => Lang::Bash,
        Some("sh") => Lang::Sh,
        _ => by_bang(),
    }
}

/// Runs `command` in the sandbox with the draft as its workspace.
async fn sandboxed(command: &str, dir: &Path, sandbox: &Arc<Sandbox>) -> Result<(), String> {
    let ctx = ToolContext {
        workspace: dir.to_path_buf(),
        max_output_chars: 1_500,
    };
    CommandVerifier::new(command, sandbox.clone(), PARSE_TIMEOUT)
        .verify(&ctx)
        .await
}

/// `sh -n`, `bash -n` and `python3 -m py_compile`, in the sandbox. A tool
/// that isn't there skips its scripts with a note; nothing runs outside
/// the sandbox.
async fn parse_scripts(
    dir: &Path,
    draft: &SkillDraft,
    sandbox: Option<&Arc<Sandbox>>,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if draft.scripts.is_empty() {
        return Ok(notes);
    }
    let Some(sandbox) = sandbox else {
        notes.push("scripts not parsed: no sandbox to run the parse checks in".into());
        return Ok(notes);
    };
    if !cfg!(unix) {
        notes.push("scripts not parsed: the parse checks run on Linux and macOS".into());
        return Ok(notes);
    }
    let mut have: BTreeMap<&str, bool> = BTreeMap::new();
    for (rel, text) in &draft.scripts {
        let (tool, command) = match lang(rel, text) {
            Lang::Sh => ("sh", format!("sh -n '{rel}'")),
            Lang::Bash => ("bash", format!("bash -n '{rel}'")),
            Lang::Python => ("python3", format!("python3 -I -m py_compile '{rel}'")),
            Lang::Other => {
                notes.push(format!(
                    "{rel} not parsed: only shell and Python scripts are checked"
                ));
                continue;
            }
        };
        let present = match have.get(tool) {
            Some(p) => *p,
            None => {
                let p = sandboxed(&format!("command -v {tool}"), dir, sandbox)
                    .await
                    .is_ok();
                have.insert(tool, p);
                p
            }
        };
        if !present {
            notes.push(format!("{rel} not parsed: {tool} isn't available here"));
            continue;
        }
        if let Err(out) = sandboxed(&command, dir, sandbox).await {
            let out = out.trim().replace('\n', " | ");
            return Err(format!("{rel} doesn't parse: {out}"));
        }
    }
    let _ = fs::remove_dir_all(dir.join("scripts").join("__pycache__"));
    Ok(notes)
}

/// One kept skill in `learn/skills.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// SHA-256 of the SKILL.md the pass kept; a different one doesn't load.
    pub digest: String,
    /// The pass that kept it.
    pub pass: String,
    pub at: String,
    /// M53.4: the one scope it is offered in; `None` is everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Entry {
    /// Whether an agent in `scope` is offered it (M53.4).
    pub fn applies(&self, scope: Option<&str>) -> bool {
        self.scope.is_none() || self.scope.as_deref() == scope
    }

    /// Whether it was kept for `scope` itself, not globally: what a
    /// session that isn't the owner's is offered.
    pub fn kept_for(&self, scope: &str) -> bool {
        self.scope.as_deref() == Some(scope)
    }
}

/// `learn/skills.json`: the learned skills the pass kept, by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub skills: BTreeMap<String, Entry>,
}

impl LearnDir {
    /// `<data>/learned-skills`, beside `<data>/learn`.
    pub fn skills_root(&self) -> PathBuf {
        self.root()
            .parent()
            .map(|p| p.join(DIR))
            .unwrap_or_else(|| PathBuf::from(DIR))
    }

    pub fn skills_index(&self) -> Index {
        fs::read_to_string(self.root().join("skills.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_skills_index(&self, index: &Index) -> anyhow::Result<()> {
        write_atomic(
            &self.root().join("skills.json"),
            serde_json::to_string_pretty(index)?.as_bytes(),
        )
    }

    /// The learned skills as the reflector sees them in `scope`: name and
    /// description, those the index holds there (M53.4: global or that
    /// scope's).
    pub fn learned_skills(&self, scope: Option<&str>) -> Vec<(String, String)> {
        let index = self.skills_index();
        let set = ferrule_skills::discover(
            &[SkillRoot {
                dir: self.skills_root(),
                scope: Scope::Learned,
            }],
            &[],
        );
        set.skills
            .into_iter()
            .filter(|s| index.skills.get(&s.name).is_some_and(|e| e.applies(scope)))
            .map(|s| (s.name, s.description))
            .collect()
    }
}

/// What keeping a skill did: enough to undo it.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub digest: String,
    /// Where it applies now.
    pub scope: Option<String>,
    /// M53.4: it was kept in another scope before, so it is global now.
    pub promoted: bool,
    /// The learned skill it replaced, under the pass dir.
    pub backup: Option<String>,
    pub previous: Option<Entry>,
}

/// SHA-256 of `<dir>/SKILL.md`.
pub fn digest(dir: &Path) -> Option<String> {
    fs::read(dir.join("SKILL.md"))
        .ok()
        .map(|b| scan::bytes_digest(&b))
}

/// Copies the validated draft into `learned-skills/<name>/` and records
/// it for `scope`. A learned skill of that name is moved into the pass dir
/// first (`skills/<n>-<name>`), so an undo can put it back. M53.4: one
/// already kept in another scope makes this one global.
pub fn keep(
    dir: &LearnDir,
    pass: &str,
    n: usize,
    draft: &Path,
    name: &str,
    scope: Option<&str>,
) -> Result<Kept, String> {
    let dest = dir.skills_root().join(name);
    let mut backup = None;
    if dest.exists() {
        let rel = format!("{n}-{name}");
        let to = dir.pass_dir(pass).join("skills").join(&rel);
        let _ = fs::remove_dir_all(&to);
        fs::create_dir_all(to.parent().unwrap_or(&to))
            .and_then(|()| fs::rename(&dest, &to))
            .map_err(|e| format!("couldn't set the old `{name}` aside: {e}"))?;
        backup = Some(rel);
    }
    if let Err(e) = ferrule_extensions::skill::copy_skill(draft, &dest) {
        // Put back what was there.
        if let Some(rel) = &backup {
            let _ = fs::remove_dir_all(&dest);
            let _ = fs::rename(dir.pass_dir(pass).join("skills").join(rel), &dest);
        }
        return Err(format!("couldn't keep the skill: {e}"));
    }
    let digest = digest(&dest).ok_or("the kept SKILL.md can't be read")?;
    let mut index = dir.skills_index();
    let (new_scope, promoted) = match index.skills.get(name).map(|e| e.scope.as_deref()) {
        None => (scope.map(str::to_string), false),
        // Global stays global.
        Some(None) => (None, false),
        Some(Some(was)) if Some(was) == scope => (scope.map(str::to_string), false),
        // Kept in a second scope: everywhere from now on.
        Some(Some(_)) => (None, true),
    };
    let previous = index.skills.insert(
        name.to_string(),
        Entry {
            digest: digest.clone(),
            pass: pass.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            scope: new_scope.clone(),
        },
    );
    dir.save_skills_index(&index)
        .map_err(|e| format!("couldn't record the skill: {e:#}"))?;
    Ok(Kept {
        digest,
        scope: new_scope,
        promoted,
        backup,
        previous,
    })
}

/// Undoes one kept skill, if it is still as the pass left it: removes it,
/// and puts back the one it replaced.
pub fn undo(
    dir: &LearnDir,
    pass: &str,
    name: &str,
    kept: &str,
    backup: Option<&str>,
    previous: Option<&Entry>,
) -> Result<String, String> {
    let dest = dir.skills_root().join(name);
    match digest(&dest) {
        None => return Err(format!("skill `{name}` is already gone")),
        Some(d) if d != kept => {
            return Err(format!("skill `{name}` was changed since; left as it is"))
        }
        Some(_) => {}
    }
    fs::remove_dir_all(&dest).map_err(|e| format!("skill `{name}`: {e}"))?;
    let mut index = dir.skills_index();
    let restored = match backup {
        Some(rel) => {
            let from = dir.pass_dir(pass).join("skills").join(rel);
            fs::rename(&from, &dest).map_err(|e| {
                format!("skill `{name}` removed, but the one it replaced couldn't be put back: {e}")
            })?;
            true
        }
        None => false,
    };
    match previous {
        Some(p) => {
            index.skills.insert(name.to_string(), p.clone());
        }
        None => {
            index.skills.remove(name);
        }
    }
    dir.save_skills_index(&index)
        .map_err(|e| format!("skill `{name}`: {e:#}"))?;
    Ok(if restored {
        format!("skill `{name}` restored to what it was before")
    } else {
        format!("skill `{name}` removed")
    })
}
