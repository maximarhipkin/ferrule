//! The suite file: a `[suite]` table and `[[task]]`s, TOML (see
//! `docs/m14-eval.md`, "Suite file format").

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file a suite directory holds.
pub const SUITE_FILE: &str = "suite.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SuiteKind {
    /// Tasks the harness may or may not manage: a pass rate to raise.
    #[default]
    Capability,
    /// Tasks it must keep passing: any failure is an exit code.
    Regression,
}

impl SuiteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SuiteKind::Capability => "capability",
            SuiteKind::Regression => "regression",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteFile {
    suite: SuiteHeader,
    #[serde(default, rename = "task")]
    tasks: Vec<toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteHeader {
    name: String,
    #[serde(default)]
    kind: SuiteKind,
    #[serde(default)]
    description: String,
    #[serde(default = "default_max_iterations")]
    max_iterations: usize,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default)]
    context_window: Option<usize>,
    #[serde(default)]
    check: Option<String>,
    #[serde(default)]
    owner_playbook: bool,
    #[serde(default)]
    owner_trust: bool,
}

fn default_max_iterations() -> usize {
    40
}

fn default_timeout() -> u64 {
    900
}

/// A loaded suite. Paths are absolute; `{suite_dir}` is already replaced
/// in every command.
#[derive(Debug, Clone)]
pub struct Suite {
    pub name: String,
    pub kind: SuiteKind,
    pub description: String,
    /// The directory the suite file is in.
    pub dir: PathBuf,
    pub max_iterations: usize,
    pub timeout_secs: u64,
    pub context_window: Option<usize>,
    /// M16: the engineered variant gets the owner's playbook. Off by
    /// default, so a run doesn't depend on one machine's lessons.
    pub owner_playbook: bool,
    /// M19: both variants run under the owner's caps, gates and kill
    /// switch, and count toward the owner's day. Off by default, so an
    /// A/B run doesn't depend on the owner's state.
    pub owner_trust: bool,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// A directory copied into the workspace, relative to the suite.
    #[serde(default)]
    pub fixture: Option<PathBuf>,
    /// Extra files written into the workspace: relative path → content.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Runs in the workspace before the agent does.
    #[serde(default)]
    pub setup: Option<String>,
    /// `git init` and one commit of the prepared workspace.
    #[serde(default)]
    pub git: bool,
    /// The engineered variant's `verify_command`.
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    pub grade: Grade,
    /// A hash of the definition and the fixture's files, for the diff
    /// report's "this task changed". Filled on load.
    #[serde(skip)]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Grade {
    /// Exit 0 passes.
    #[serde(default)]
    pub command: Option<String>,
    /// One criterion per non-empty line, judged by a model.
    #[serde(default)]
    pub rubric: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl Suite {
    /// Loads `path`: a suite file, or a directory holding `suite.toml`.
    pub fn load(path: &Path) -> Result<Suite> {
        let file = if path.is_dir() {
            path.join(SUITE_FILE)
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading suite file {}", file.display()))?;
        let dir = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = dir.canonicalize().unwrap_or(dir);
        let parsed: SuiteFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let h = parsed.suite;
        let dir_str = dir.to_string_lossy().into_owned();
        let subst = |s: Option<String>| s.map(|s| s.replace("{suite_dir}", &dir_str));

        let mut tasks = Vec::with_capacity(parsed.tasks.len());
        for raw in parsed.tasks {
            let definition = toml::to_string(&raw).unwrap_or_default();
            let mut task: Task = raw
                .try_into()
                .with_context(|| format!("a [[task]] in {}", file.display()))?;
            if task.id.is_empty()
                || !task
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!(
                    "task id {:?}: use letters, digits, '-' and '_' only",
                    task.id
                );
            }
            if tasks.iter().any(|t: &Task| t.id == task.id) {
                bail!("task id {:?} appears twice", task.id);
            }
            if task.grade.command.is_none() && task.grade.rubric.is_none() {
                bail!(
                    "task {:?} has no grader: set grade.command and/or grade.rubric",
                    task.id
                );
            }
            task.setup = subst(task.setup);
            task.check = subst(task.check.or_else(|| h.check.clone()));
            task.grade.command = subst(task.grade.command);
            task.fixture = task.fixture.map(|f| dir.join(f));
            if let Some(f) = &task.fixture {
                if !f.is_dir() {
                    bail!(
                        "task {:?}: fixture {} is not a directory",
                        task.id,
                        f.display()
                    );
                }
            }
            task.fingerprint = fingerprint(&definition, task.fixture.as_deref());
            tasks.push(task);
        }
        if tasks.is_empty() {
            bail!("{} has no [[task]]", file.display());
        }
        Ok(Suite {
            name: h.name,
            kind: h.kind,
            description: h.description,
            dir,
            max_iterations: h.max_iterations,
            timeout_secs: h.timeout_secs,
            context_window: h.context_window,
            owner_playbook: h.owner_playbook,
            owner_trust: h.owner_trust,
            tasks,
        })
    }

    /// The tasks with any of `tags` (all when empty) and one of `ids` (all
    /// when empty). An id that matches nothing is an error, a typo would
    /// otherwise run nothing.
    pub fn select(&self, tags: &[String], ids: &[String]) -> Result<Vec<&Task>> {
        for id in ids {
            if !self.tasks.iter().any(|t| &t.id == id) {
                bail!("suite {:?} has no task {:?}", self.name, id);
            }
        }
        let picked: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| tags.is_empty() || t.tags.iter().any(|g| tags.contains(g)))
            .filter(|t| ids.is_empty() || ids.contains(&t.id))
            .collect();
        if picked.is_empty() {
            bail!("no task in suite {:?} matches the selection", self.name);
        }
        Ok(picked)
    }
}

impl Task {
    pub fn max_iterations(&self, suite: &Suite) -> usize {
        self.max_iterations.unwrap_or(suite.max_iterations)
    }

    pub fn timeout_secs(&self, suite: &Suite) -> u64 {
        self.timeout_secs.unwrap_or(suite.timeout_secs)
    }
}

/// FNV-1a over the task's definition and its fixture's files (path and
/// content, in path order): stable across builds and platforms.
fn fingerprint(definition: &str, fixture: Option<&Path>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // A separator, so ("ab","c") and ("a","bc") differ.
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    eat(definition.as_bytes());
    if let Some(root) = fixture {
        let mut files = Vec::new();
        collect_files(root, root, &mut files);
        files.sort();
        for rel in files {
            eat(rel.as_bytes());
            eat(&std::fs::read(root.join(&rel)).unwrap_or_default());
        }
    }
    format!("{h:016x}")
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            // '/' on every platform, so a fingerprint made on Windows
            // matches one made on macOS.
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(rel.join("/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let f = dir.join(SUITE_FILE);
        std::fs::write(&f, text).unwrap();
        f
    }

    #[test]
    fn loads_tasks_with_suite_dir_substituted_and_defaults_applied() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fx")).unwrap();
        std::fs::write(dir.path().join("fx/a.txt"), "a").unwrap();
        write(
            dir.path(),
            r#"
[suite]
name = "s"
kind = "regression"
check = "make check"

[[task]]
id = "one"
prompt = "do it"
fixture = "fx"
tags = ["smoke"]
[task.grade]
command = 'python3 "{suite_dir}/g.py"'

[[task]]
id = "two"
prompt = "other"
check = "own check"
max_iterations = 5
[task.grade]
rubric = "- it's done"
"#,
        );
        let s = Suite::load(dir.path()).unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(s.kind, SuiteKind::Regression);
        assert_eq!(s.tasks.len(), 2);
        let one = &s.tasks[0];
        assert_eq!(
            one.grade.command.as_deref().unwrap(),
            format!("python3 \"{}/g.py\"", root.display())
        );
        assert_eq!(one.check.as_deref(), Some("make check"));
        assert_eq!(one.fixture.as_deref(), Some(root.join("fx").as_path()));
        assert_eq!(one.max_iterations(&s), 40);
        assert_eq!(s.tasks[1].check.as_deref(), Some("own check"));
        assert_eq!(s.tasks[1].max_iterations(&s), 5);

        assert_eq!(s.select(&["smoke".into()], &[]).unwrap().len(), 1);
        assert_eq!(s.select(&[], &["two".into()]).unwrap()[0].id, "two");
        assert!(s.select(&[], &["nope".into()]).is_err());
    }

    #[test]
    fn fingerprint_follows_the_fixture() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fx")).unwrap();
        std::fs::write(dir.path().join("fx/a.txt"), "a").unwrap();
        let text = "[suite]\nname = \"s\"\n[[task]]\nid = \"t\"\nprompt = \"p\"\nfixture = \"fx\"\ngrade = { command = \"true\" }\n";
        write(dir.path(), text);
        let before = Suite::load(dir.path()).unwrap().tasks[0]
            .fingerprint
            .clone();
        assert_eq!(
            Suite::load(dir.path()).unwrap().tasks[0].fingerprint,
            before
        );
        std::fs::write(dir.path().join("fx/a.txt"), "b").unwrap();
        assert_ne!(
            Suite::load(dir.path()).unwrap().tasks[0].fingerprint,
            before
        );
    }

    #[test]
    fn rejects_typos_and_graderless_tasks() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "[suite]\nname = \"s\"\n[[task]]\nid = \"t\"\nprompt = \"p\"\ngrade = { comand = \"true\" }\n",
        );
        assert!(Suite::load(dir.path()).is_err());
        write(
            dir.path(),
            "[suite]\nname = \"s\"\n[[task]]\nid = \"t\"\nprompt = \"p\"\ngrade = {}\n",
        );
        let err = Suite::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("no grader"), "{err}");
    }
}
