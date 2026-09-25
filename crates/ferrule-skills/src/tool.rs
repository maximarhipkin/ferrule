//! The two tools the model uses to work with skills: `activate_skill` loads
//! a skill's instructions (tier 2 of progressive disclosure), and
//! `read_skill_file` reads the resources a skill bundles (tier 3). Both only
//! accept names from the catalog, so they can't be pointed at arbitrary
//! files — the general fs tools stay confined to the workspace, and skills
//! typically live outside it.

use crate::discover::{discover, Skill, SkillRoot, SkillSet};
use ferrule_core::agent::{SKILL_CONTENT_CLOSE, SKILL_CONTENT_OPEN};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput, ToolSource};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub const ACTIVATE_TOOL: &str = "activate_skill";
pub const READ_TOOL: &str = "read_skill_file";

/// Skill bodies get a larger cap than ordinary tool output (30k): cutting a
/// skill's instructions mid-way is worse than spending the context on them.
pub const SKILL_MAX_CHARS: usize = 60_000;
/// How much of a skill's directory `activate_skill` lists, so a skill with a
/// vendored dependency tree can't flood the context.
const RESOURCE_MAX_FILES: usize = 50;
const RESOURCE_MAX_DEPTH: usize = 3;

/// Both tools for `skills`, or none when no skill is model-invocable — an
/// empty tool with an empty enum would only confuse the model.
pub fn tools(skills: Arc<SkillSet>) -> Vec<Arc<dyn Tool>> {
    LiveSkillTools::new(SkillsHandle::fixed(skills)).tools()
}

/// A skill set that can change while agents run (M13: a skill installed or
/// removed mid-session). Clones share the set; `refresh` re-runs discovery
/// over the roots it was made with.
#[derive(Clone)]
pub struct SkillsHandle {
    current: Arc<RwLock<Arc<SkillSet>>>,
    roots: Arc<[SkillRoot]>,
    /// The owner can change it while agents run (M24).
    disabled: Arc<RwLock<Vec<String>>>,
}

impl SkillsHandle {
    /// Discover now, and again on every `refresh`.
    pub fn discovering(roots: Vec<SkillRoot>, disabled: Vec<String>) -> Self {
        let set = discover(&roots, &disabled);
        Self {
            current: Arc::new(RwLock::new(Arc::new(set))),
            roots: roots.into(),
            disabled: Arc::new(RwLock::new(disabled)),
        }
    }

    /// A set that `refresh` leaves as it is.
    pub fn fixed(set: Arc<SkillSet>) -> Self {
        Self {
            current: Arc::new(RwLock::new(set)),
            roots: Arc::new([]),
            disabled: Arc::default(),
        }
    }

    pub fn get(&self) -> Arc<SkillSet> {
        self.current.read().unwrap().clone()
    }

    /// Rediscover: what changed on disk is what the tools offer from the
    /// next request on.
    pub fn refresh(&self) {
        if self.roots.is_empty() {
            return;
        }
        let disabled = self.disabled.read().unwrap().clone();
        let set = discover(&self.roots, &disabled);
        *self.current.write().unwrap() = Arc::new(set);
    }

    /// The skills to leave out, rediscovering when they changed. `true`
    /// when they did.
    pub fn set_disabled(&self, disabled: Vec<String>) -> bool {
        {
            let mut now = self.disabled.write().unwrap();
            if *now == disabled {
                return false;
            }
            *now = disabled;
        }
        self.refresh();
        true
    }
}

/// `activate_skill` and `read_skill_file` over a live set, as a dynamic tool
/// source: offered only while some skill is model-invocable, and their name
/// enum follows the set. One per agent — the activation tool remembers what
/// this session already loaded.
pub struct LiveSkillTools {
    activate: Arc<ActivateSkillTool>,
    read: Arc<ReadSkillFileTool>,
    skills: SkillsHandle,
}

impl LiveSkillTools {
    pub fn new(skills: SkillsHandle) -> Self {
        Self {
            activate: Arc::new(ActivateSkillTool {
                skills: skills.clone(),
                active: Mutex::new(HashSet::new()),
            }),
            read: Arc::new(ReadSkillFileTool {
                skills: skills.clone(),
            }),
            skills,
        }
    }
}

impl ToolSource for LiveSkillTools {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self.skills.get().invocable().next().is_none() {
            return Vec::new();
        }
        vec![self.activate.clone(), self.read.clone()]
    }
}

fn names_schema(skills: &SkillSet) -> Value {
    let names: Vec<&str> = skills.invocable().map(|s| s.name.as_str()).collect();
    json!({ "type": "string", "enum": names, "description": "Skill name, exactly as listed in <available_skills>" })
}

fn failed(tool: &str, message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: tool.into(),
        message: message.into(),
    }
}

fn lookup(skills: &SkillsHandle, tool: &str, args: &Value) -> Result<Skill, CoreError> {
    let name = args["name"].as_str().unwrap_or("");
    skills
        .get()
        .get(name)
        .filter(|s| s.model_invocable)
        .cloned()
        .ok_or_else(|| failed(tool, format!("no skill named `{name}` is available")))
}

pub struct ActivateSkillTool {
    skills: SkillsHandle,
    /// Names already loaded by this agent. One tool instance per agent, so
    /// this is per session.
    active: Mutex<HashSet<String>>,
}

#[async_trait::async_trait]
impl Tool for ActivateSkillTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ACTIVATE_TOOL.into(),
            description: "Load a skill's full instructions. Call this when a task matches a skill's description in \
                          <available_skills>, before starting the task."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": { "name": names_schema(&self.skills.get()) },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let skill = lookup(&self.skills, ACTIVATE_TOOL, &args)?;
        if self.active.lock().unwrap().contains(&skill.name) {
            return Ok(ToolOutput::ok(format!(
                "Skill `{}` is already active in this session — follow the instructions loaded earlier.",
                skill.name
            )));
        }
        // Read fresh rather than caching at discovery: a skill edited while
        // the gateway runs takes effect on its next activation.
        let text = tokio::fs::read_to_string(&skill.location)
            .await
            .map_err(|e| failed(ACTIVATE_TOOL, format!("{}: {e}", skill.location.display())))?;
        let body = crate::frontmatter::parse(&text)
            .map(|fm| fm.body)
            .map_err(|e| failed(ACTIVATE_TOOL, e))?;
        let content = render_activation(&skill, &body);
        self.active.lock().unwrap().insert(skill.name.clone());
        Ok(ToolOutput::ok(content))
    }
}

/// The activation result. The body is capped *inside* the wrapper so the
/// closing tag always survives — compaction keys on the full block.
pub fn render_activation(skill: &Skill, body: &str) -> String {
    let dir = dunce::canonicalize(skill.dir()).unwrap_or_else(|_| skill.dir().to_path_buf());
    let mut body_out: String = body.chars().take(SKILL_MAX_CHARS).collect();
    if body_out.len() < body.len() {
        body_out.push_str(&format!(
            "\n…[skill body truncated at {SKILL_MAX_CHARS} of {} chars]",
            body.chars().count()
        ));
    }
    let mut out = format!("{SKILL_CONTENT_OPEN}{}\">\n{body_out}\n\n", skill.name);
    out.push_str(&format!(
        "Skill directory: {}\nRelative paths in this skill are relative to the skill directory. \
         Read bundled files with {READ_TOOL}; run bundled scripts with the shell tool using absolute paths.\n",
        dir.display()
    ));
    let resources = list_resources(&dir);
    if !resources.is_empty() {
        out.push_str("\n<skill_resources>\n");
        for r in &resources {
            out.push_str(&format!("  <file>{r}</file>\n"));
        }
        out.push_str("</skill_resources>\n");
    }
    out.push_str(SKILL_CONTENT_CLOSE);
    out
}

/// Relative paths of the files a skill ships besides SKILL.md — listed, not
/// read, so the model loads only what the instructions call for.
fn list_resources(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect(dir, dir, 0, &mut out);
    out.sort();
    if out.len() > RESOURCE_MAX_FILES {
        let more = out.len() - RESOURCE_MAX_FILES;
        out.truncate(RESOURCE_MAX_FILES);
        out.push(format!("…and {more} more"));
    }
    out
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    // Stop well past the cap rather than walking a whole vendored tree.
    if depth >= RESOURCE_MAX_DEPTH || out.len() > RESOURCE_MAX_FILES * 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || matches!(name, "node_modules" | "__pycache__") {
            continue;
        }
        if p.is_dir() {
            collect(root, &p, depth + 1, out);
        } else if !(depth == 0 && name == "SKILL.md") {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

pub struct ReadSkillFileTool {
    skills: SkillsHandle,
}

#[async_trait::async_trait]
impl Tool for ReadSkillFileTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: READ_TOOL.into(),
            description:
                "Read a text file bundled with a skill (scripts, references, templates listed in \
                          <skill_resources>). Paths are relative to the skill directory."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": names_schema(&self.skills.get()),
                    "path": { "type": "string", "description": "Path relative to the skill directory, e.g. references/REFERENCE.md" }
                },
                "required": ["name", "path"]
            }),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let skill = lookup(&self.skills, READ_TOOL, &args)?;
        let rel = args["path"].as_str().unwrap_or("");
        let path = resolve_in_skill(skill.dir(), rel)?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| failed(READ_TOOL, format!("{rel}: {e}")))?;
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

/// Resolve `rel` inside `skill_dir`, refusing anything that ends up
/// outside it — lexically (`..`, absolute paths) and after following
/// symlinks. The skill directory itself may be a symlink; that's fine, the
/// check is against its resolved location.
fn resolve_in_skill(skill_dir: &Path, rel: &str) -> Result<PathBuf, CoreError> {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(failed(
            READ_TOOL,
            format!("`{rel}` must be a relative path inside the skill directory"),
        ));
    }
    let base = dunce::canonicalize(skill_dir)
        .map_err(|e| failed(READ_TOOL, format!("skill directory: {e}")))?;
    let target = dunce::canonicalize(base.join(rel_path))
        .map_err(|e| failed(READ_TOOL, format!("{rel}: {e}")))?;
    if !target.starts_with(&base) {
        return Err(failed(
            READ_TOOL,
            format!("`{rel}` resolves outside the skill directory"),
        ));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::{discover, Scope, SkillRoot};

    fn fixture() -> (tempfile::TempDir, Arc<SkillSet>) {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("pdf");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::create_dir_all(skill.join("__pycache__")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: pdf\ndescription: Work with PDFs\n---\n# PDF\nRun scripts/extract.py\n",
        )
        .unwrap();
        std::fs::write(skill.join("scripts/extract.py"), "print('x')").unwrap();
        std::fs::write(skill.join("references/REFERENCE.md"), "reference text").unwrap();
        std::fs::write(skill.join("__pycache__/x.pyc"), "junk").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
        let hidden = dir.path().join("manual");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(
            hidden.join("SKILL.md"),
            "---\nname: manual\ndescription: d\ndisable-model-invocation: true\n---\nbody",
        )
        .unwrap();
        let set = discover(
            &[SkillRoot {
                dir: dir.path().to_path_buf(),
                scope: Scope::User,
            }],
            &[],
        );
        (dir, Arc::new(set))
    }

    fn ctx() -> ToolContext {
        ToolContext::default()
    }

    #[tokio::test]
    async fn a_skill_added_on_disk_is_offered_and_activatable_after_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let handle = SkillsHandle::discovering(
            vec![SkillRoot {
                dir: dir.path().to_path_buf(),
                scope: Scope::User,
            }],
            vec![],
        );
        let live = LiveSkillTools::new(handle.clone());
        assert!(live.tools().is_empty(), "no skills, no tools");

        let skill = dir.path().join("late");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: late\ndescription: Installed mid-session\n---\nDo the late thing.\n",
        )
        .unwrap();
        assert!(live.tools().is_empty(), "nothing changes before a refresh");
        handle.refresh();
        let tools = live.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[0].definition().parameters["properties"]["name"]["enum"],
            json!(["late"])
        );
        let out = tools[0]
            .call(json!({"name": "late"}), &ctx())
            .await
            .unwrap()
            .content;
        assert!(out.contains("Do the late thing."), "{out}");

        std::fs::remove_dir_all(&skill).unwrap();
        handle.refresh();
        assert!(live.tools().is_empty());
        assert!(live
            .activate
            .call(json!({"name": "late"}), &ctx())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn activation_wraps_body_lists_resources_and_dedupes() {
        let (_dir, set) = fixture();
        let tools = tools(set);
        let activate = &tools[0];
        let enum_names = &activate.definition().parameters["properties"]["name"]["enum"];
        assert_eq!(
            enum_names,
            &json!(["pdf"]),
            "user-only skills must not be offered to the model"
        );

        let out = activate
            .call(json!({"name": "pdf"}), &ctx())
            .await
            .unwrap()
            .content;
        assert!(
            out.starts_with("<skill_content name=\"pdf\">\n# PDF\nRun scripts/extract.py"),
            "{out}"
        );
        assert!(out.ends_with("</skill_content>"));
        assert!(
            !out.contains("description: Work with PDFs"),
            "frontmatter must be stripped"
        );
        assert!(out.contains("<file>scripts/extract.py</file>"));
        assert!(out.contains("<file>references/REFERENCE.md</file>"));
        assert!(!out.contains("SKILL.md</file>") && !out.contains("pycache"));

        let again = activate
            .call(json!({"name": "pdf"}), &ctx())
            .await
            .unwrap()
            .content;
        assert!(again.contains("already active"), "{again}");

        assert!(activate
            .call(json!({"name": "manual"}), &ctx())
            .await
            .is_err());
        assert!(activate
            .call(json!({"name": "nope"}), &ctx())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn read_skill_file_stays_inside_the_skill() {
        let (_dir, set) = fixture();
        let read = &tools(set)[1];
        let ok = read
            .call(
                json!({"name": "pdf", "path": "references/REFERENCE.md"}),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(ok.content, "reference text");
        for bad in [
            "../secret.txt",
            "scripts/../../secret.txt",
            "/etc/passwd",
            "",
        ] {
            assert!(
                read.call(json!({"name": "pdf", "path": bad}), &ctx())
                    .await
                    .is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_skill_file_refuses_a_symlink_pointing_out() {
        let (dir, set) = fixture();
        std::os::unix::fs::symlink(
            dir.path().join("secret.txt"),
            dir.path().join("pdf/leak.txt"),
        )
        .unwrap();
        let read = &tools(set)[1];
        let err = read
            .call(json!({"name": "pdf", "path": "leak.txt"}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[test]
    fn no_invocable_skills_means_no_tools() {
        let dir = tempfile::tempdir().unwrap();
        let set = discover(
            &[SkillRoot {
                dir: dir.path().to_path_buf(),
                scope: Scope::User,
            }],
            &[],
        );
        assert!(tools(Arc::new(set)).is_empty());
    }

    #[test]
    fn oversized_body_is_capped_inside_the_wrapper() {
        let (_dir, set) = fixture();
        let skill = set.get("pdf").unwrap();
        let out = render_activation(skill, &"y".repeat(SKILL_MAX_CHARS + 10));
        assert!(out.contains("skill body truncated"));
        assert!(out.ends_with(SKILL_CONTENT_CLOSE));
    }

    #[test]
    fn a_skill_disabled_while_running_leaves_the_set_and_comes_back() {
        let (dir, _) = fixture();
        let roots = vec![SkillRoot {
            dir: dir.path().to_path_buf(),
            scope: Scope::User,
        }];
        let h = SkillsHandle::discovering(roots, vec![]);
        let shared = h.clone();
        assert!(h.get().get("pdf").is_some());
        assert!(h.set_disabled(vec!["pdf".into()]));
        assert!(shared.get().get("pdf").is_none(), "clones see it at once");
        assert!(!h.set_disabled(vec!["pdf".into()]), "no change");
        shared.refresh();
        assert!(h.get().get("pdf").is_none(), "a refresh keeps it out");
        assert!(h.set_disabled(vec![]));
        assert!(h.get().get("pdf").is_some());
    }
}
