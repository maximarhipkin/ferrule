//! Finding skills on disk: which directories are scanned, in what order,
//! and what happens to malformed or colliding skills.
//!
//! Follows the agentskills.io client guide: project scope beats user scope,
//! first-found wins inside a scope, validation is lenient (warn and load)
//! except for an unreadable frontmatter block or a missing description,
//! which skip the skill.

use crate::frontmatter;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Spec limits; exceeding them warns, it doesn't skip.
pub const NAME_MAX: usize = 64;
pub const DESCRIPTION_MAX: usize = 1024;

/// Bounds on the directory walk so a skills root pointed at something huge
/// can't stall agent startup.
const MAX_DEPTH: usize = 4;
const MAX_DIRS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::User => "user",
        }
    }
}

/// One directory to scan, e.g. `~/.agents/skills`.
#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub dir: PathBuf,
    pub scope: Scope,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute-or-as-found path to the SKILL.md.
    pub location: PathBuf,
    pub scope: Scope,
    /// False for `disable-model-invocation: true`: hidden from the catalog
    /// and the activation tool, listed only by `ferrule skills`.
    pub model_invocable: bool,
}

impl Skill {
    /// The skill's base directory, against which its relative paths resolve.
    pub fn dir(&self) -> &Path {
        self.location.parent().unwrap_or(Path::new("."))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Loaded anyway.
    Warning,
    /// Not loaded.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub severity: Severity,
    pub message: String,
}

/// Everything discovery found, sorted by name, plus what it had to say
/// about the files it rejected or doubted.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<Diagnostic>,
}

impl SkillSet {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// The skills the model may activate itself.
    pub fn invocable(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter().filter(|s| s.model_invocable)
    }
}

/// The standard scan order. Project roots come first so they shadow user
/// skills of the same name. Within a scope Ferrule's own directory wins,
/// then the cross-client `.agents/skills`, then `.claude/skills` (where most
/// existing skills are installed). `extra` directories from config are
/// user scope and searched before the defaults, since naming them is an
/// explicit choice.
pub fn default_roots(workspace: &Path, include_project: bool, extra: &[PathBuf]) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    if include_project {
        for sub in [".ferrule", ".agents", ".claude"] {
            roots.push(SkillRoot {
                dir: workspace.join(sub).join("skills"),
                scope: Scope::Project,
            });
        }
    }
    for dir in extra {
        roots.push(SkillRoot {
            dir: dir.clone(),
            scope: Scope::User,
        });
    }
    if let Some(config) = dirs::config_dir() {
        roots.push(SkillRoot {
            dir: config.join("ferrule").join("skills"),
            scope: Scope::User,
        });
    }
    if let Some(home) = dirs::home_dir() {
        for sub in [".agents", ".claude"] {
            roots.push(SkillRoot {
                dir: home.join(sub).join("skills"),
                scope: Scope::User,
            });
        }
    }
    roots
}

/// Scan `roots` in order. The first skill to claim a name keeps it; later
/// ones are reported as shadowed. Names in `disabled` are dropped entirely.
pub fn discover(roots: &[SkillRoot], disabled: &[String]) -> SkillSet {
    let mut set = SkillSet::default();
    let mut by_name: HashMap<String, PathBuf> = HashMap::new();
    let mut seen_roots: Vec<PathBuf> = Vec::new();

    for root in roots {
        // The same directory reached twice (e.g. workspace == $HOME) would
        // otherwise report every skill as shadowing itself.
        let canon = root.dir.canonicalize().unwrap_or_else(|_| root.dir.clone());
        if seen_roots.contains(&canon) {
            continue;
        }
        seen_roots.push(canon);

        let mut found = Vec::new();
        let mut budget = MAX_DIRS;
        if root.dir.join("SKILL.md").is_file() {
            found.push(root.dir.join("SKILL.md"));
        } else if root.dir.is_dir() {
            walk(&root.dir, 0, &mut budget, &mut found);
            if budget == 0 {
                set.diagnostics.push(Diagnostic {
                    path: root.dir.clone(),
                    severity: Severity::Warning,
                    message: format!("stopped scanning after {MAX_DIRS} directories"),
                });
            }
        }

        for location in found {
            let Some(skill) = load(&location, root.scope, &mut set.diagnostics) else {
                continue;
            };
            if disabled.iter().any(|d| d == &skill.name) {
                continue;
            }
            if let Some(winner) = by_name.get(&skill.name) {
                set.diagnostics.push(Diagnostic {
                    path: location.clone(),
                    severity: Severity::Skipped,
                    message: format!("`{}` is shadowed by {}", skill.name, winner.display()),
                });
                continue;
            }
            by_name.insert(skill.name.clone(), location);
            set.skills.push(skill);
        }
    }
    set.skills.sort_by(|a, b| a.name.cmp(&b.name));
    set
}

/// Collect `*/SKILL.md` under `dir`. A directory holding a SKILL.md is a
/// skill, so its own subdirectories (scripts/, references/) aren't searched.
/// `is_dir`/`is_file` follow symlinks on purpose: skill installers commonly
/// symlink skill directories into place.
fn walk(dir: &Path, depth: usize, budget: &mut usize, found: &mut Vec<PathBuf>) {
    if depth >= MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('.') && name != "node_modules"
        })
        .collect();
    subdirs.sort();
    for sub in subdirs {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let skill_md = sub.join("SKILL.md");
        if skill_md.is_file() {
            found.push(skill_md);
        } else {
            walk(&sub, depth + 1, budget, found);
        }
    }
}

fn load(location: &Path, scope: Scope, diags: &mut Vec<Diagnostic>) -> Option<Skill> {
    let mut diag = |severity, message: String| {
        diags.push(Diagnostic {
            path: location.to_path_buf(),
            severity,
            message,
        });
    };
    let text = match std::fs::read_to_string(location) {
        Ok(t) => t,
        Err(e) => {
            diag(Severity::Skipped, format!("unreadable: {e}"));
            return None;
        }
    };
    let fm = match frontmatter::parse(&text) {
        Ok(fm) => fm,
        Err(e) => {
            diag(Severity::Skipped, e);
            return None;
        }
    };
    let dir_name = location
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let name = match fm.get("name").map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => {
            diag(
                Severity::Warning,
                format!("no `name`; using the directory name `{dir_name}`"),
            );
            dir_name.clone()
        }
    };
    if name.is_empty() {
        diag(
            Severity::Skipped,
            "no `name` and no usable directory name".into(),
        );
        return None;
    }
    if name != dir_name {
        diag(
            Severity::Warning,
            format!("name `{name}` doesn't match its directory `{dir_name}`"),
        );
    }
    if let Some(problem) = name_problem(&name) {
        diag(Severity::Warning, format!("name `{name}`: {problem}"));
    }

    let description = fm.get("description").map(str::trim).unwrap_or("");
    if description.is_empty() {
        diag(
            Severity::Skipped,
            "no `description` — the model couldn't know when to use it".into(),
        );
        return None;
    }
    let mut description = description.to_string();
    if description.chars().count() > DESCRIPTION_MAX {
        diag(Severity::Warning, format!("description is over {DESCRIPTION_MAX} characters; the catalog shows the first {DESCRIPTION_MAX}"));
        description = description.chars().take(DESCRIPTION_MAX).collect();
    }

    Some(Skill {
        name,
        description,
        location: location.to_path_buf(),
        scope,
        model_invocable: !fm.flag("disable-model-invocation"),
    })
}

/// The spec's name rules: 1-64 chars of `a-z0-9-`, no leading, trailing or
/// doubled hyphen.
fn name_problem(name: &str) -> Option<&'static str> {
    if name.chars().count() > NAME_MAX {
        return Some("longer than 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some("only lowercase letters, digits and hyphens are allowed");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Some("no leading, trailing or consecutive hyphens");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str) -> PathBuf {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("SKILL.md");
        std::fs::write(&p, format!("---\n{frontmatter}\n---\nbody of {dir}\n")).unwrap();
        p
    }

    fn root(dir: &Path, scope: Scope) -> SkillRoot {
        SkillRoot {
            dir: dir.to_path_buf(),
            scope,
        }
    }

    #[test]
    fn project_scope_shadows_user_scope_with_a_diagnostic() {
        let project = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        write_skill(
            project.path(),
            "review",
            "name: review\ndescription: project review",
        );
        write_skill(
            user.path(),
            "review",
            "name: review\ndescription: user review",
        );
        write_skill(user.path(), "pdf", "name: pdf\ndescription: pdfs");

        let set = discover(
            &[
                root(project.path(), Scope::Project),
                root(user.path(), Scope::User),
            ],
            &[],
        );
        let names: Vec<_> = set.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["pdf", "review"]);
        let review = set.get("review").unwrap();
        assert_eq!(review.description, "project review");
        assert_eq!(review.scope, Scope::Project);
        assert!(set
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Skipped && d.message.contains("shadowed")));
    }

    #[test]
    fn lenient_validation_warns_but_loads_and_skips_only_hard_failures() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "mismatch", "name: Other_Name\ndescription: d");
        write_skill(dir.path(), "noname", "description: d");
        write_skill(dir.path(), "nodesc", "name: nodesc");
        std::fs::create_dir_all(dir.path().join("nofm")).unwrap();
        std::fs::write(dir.path().join("nofm/SKILL.md"), "# no frontmatter").unwrap();

        let set = discover(&[root(dir.path(), Scope::User)], &[]);
        let names: Vec<_> = set.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Other_Name", "noname"]);
        let skipped = set
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Skipped)
            .count();
        assert_eq!(skipped, 2, "{:?}", set.diagnostics);
        assert!(set
            .diagnostics
            .iter()
            .any(|d| d.message.contains("doesn't match its directory")));
        assert!(set
            .diagnostics
            .iter()
            .any(|d| d.message.contains("lowercase")));
    }

    #[test]
    fn nested_and_symlinked_skill_dirs_are_found_but_not_their_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "pack/inner", "name: inner\ndescription: d");
        write_skill(dir.path(), "outer", "name: outer\ndescription: d");
        // A SKILL.md inside a skill's own subdir is not a separate skill.
        write_skill(
            dir.path(),
            "outer/references/sub",
            "name: sub\ndescription: d",
        );
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/x/SKILL.md"),
            "---\nname: x\ndescription: d\n---\n",
        )
        .unwrap();

        let elsewhere = tempfile::tempdir().unwrap();
        write_skill(elsewhere.path(), "linked", "name: linked\ndescription: d");
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path().join("linked"), dir.path().join("linked"))
            .unwrap();

        let set = discover(&[root(dir.path(), Scope::User)], &[]);
        let names: Vec<_> = set.skills.iter().map(|s| s.name.as_str()).collect();
        #[cfg(unix)]
        assert_eq!(names, ["inner", "linked", "outer"]);
        #[cfg(not(unix))]
        assert_eq!(names, ["inner", "outer"]);
    }

    #[test]
    fn disabled_and_user_only_skills() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "a", "name: a\ndescription: d");
        write_skill(
            dir.path(),
            "b",
            "name: b\ndescription: d\ndisable-model-invocation: true",
        );
        write_skill(dir.path(), "c", "name: c\ndescription: d");

        let set = discover(&[root(dir.path(), Scope::User)], &["c".to_string()]);
        let names: Vec<_> = set.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        let invocable: Vec<_> = set.invocable().map(|s| s.name.as_str()).collect();
        assert_eq!(invocable, ["a"]);
    }

    #[test]
    fn a_root_that_is_itself_a_skill_and_a_repeated_root() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "solo", "name: solo\ndescription: d");
        let solo = dir.path().join("solo");
        let set = discover(&[root(&solo, Scope::User), root(&solo, Scope::User)], &[]);
        assert_eq!(set.skills.len(), 1);
        assert!(set.diagnostics.is_empty(), "{:?}", set.diagnostics);
    }

    #[test]
    fn overlong_description_is_truncated_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(DESCRIPTION_MAX + 50);
        write_skill(
            dir.path(),
            "long",
            &format!("name: long\ndescription: {long}"),
        );
        let set = discover(&[root(dir.path(), Scope::User)], &[]);
        assert_eq!(set.get("long").unwrap().description.len(), DESCRIPTION_MAX);
        assert!(set
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Warning));
    }
}
