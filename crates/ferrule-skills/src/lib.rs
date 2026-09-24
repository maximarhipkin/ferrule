//! Agent Skills: folders with a `SKILL.md` (YAML frontmatter + Markdown
//! instructions), in the format shared by Claude Code, Codex and the other
//! agentskills.io clients — skills written for them work here unchanged.
//!
//! Progressive disclosure, per the spec:
//! 1. **Catalog** — each skill's name and description go into the system
//!    prompt (`SkillSet::catalog`), ~50-100 tokens a skill.
//! 2. **Instructions** — the model calls `activate_skill` when a task matches;
//!    the SKILL.md body enters the conversation wrapped in `<skill_content>`,
//!    which compaction carries forward verbatim.
//! 3. **Resources** — files the skill bundles, read on demand with
//!    `read_skill_file`.

pub mod discover;
pub mod frontmatter;
pub mod tool;

pub use discover::{
    default_roots, discover, Diagnostic, Scope, Severity, Skill, SkillRoot, SkillSet,
};
pub use tool::{tools, LiveSkillTools, SkillsHandle, ACTIVATE_TOOL, READ_TOOL};

impl SkillSet {
    /// The system-prompt section listing model-invocable skills, or `None`
    /// when there are none — an empty catalog would only invite the model to
    /// call a tool that isn't registered.
    pub fn catalog(&self) -> Option<String> {
        let mut entries = String::new();
        for s in self.invocable() {
            entries.push_str(&format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n  </skill>\n",
                xml_escape(&s.name),
                xml_escape(&s.description)
            ));
        }
        if entries.is_empty() {
            return None;
        }
        Some(format!(
            "The following skills provide specialized instructions for specific tasks. When a task matches a \
             skill's description, call the {ACTIVATE_TOOL} tool with the skill's name to load its full \
             instructions before proceeding. Read files a skill bundles with {READ_TOOL}.\n\n\
             <available_skills>\n{entries}</available_skills>"
        ))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_invocable_skills_escaped_and_is_absent_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        for (name, extra) in [
            ("a", "description: use for <html> & xml"),
            ("b", "description: d\ndisable-model-invocation: true"),
        ] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            std::fs::write(
                dir.path().join(name).join("SKILL.md"),
                format!("---\nname: {name}\n{extra}\n---\nbody"),
            )
            .unwrap();
        }
        let set = discover(
            &[SkillRoot {
                dir: dir.path().to_path_buf(),
                scope: Scope::User,
            }],
            &[],
        );
        let catalog = set.catalog().unwrap();
        assert!(catalog.contains("<name>a</name>"));
        assert!(catalog.contains("use for &lt;html&gt; &amp; xml"));
        assert!(
            !catalog.contains("<name>b</name>"),
            "user-only skills stay out of the catalog"
        );
        assert!(catalog.contains(ACTIVATE_TOOL));

        assert!(SkillSet::default().catalog().is_none());
    }
}
