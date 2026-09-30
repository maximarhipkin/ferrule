use std::path::{Path, PathBuf};

/// Context-baseline files, in priority order. "Living documentation written
/// specifically for agents, not humans" — the single highest-leverage prep
/// step in production agentic engineering (Google's Industrial Agentic
/// Engineering, T3chFest 2026; also Claude Code's CLAUDE.md hierarchy).
pub const BASELINE_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "GEMINI.md", "ferrule.md"];

/// Hard cap so a giant baseline can't eat the window; these files should be
/// short and curated anyway.
pub const BASELINE_MAX_CHARS: usize = 16_000;

/// Find and load the workspace's context baseline, if any.
/// Returns (filename, content).
pub fn load_context_baseline(workspace: &Path) -> Option<(String, String)> {
    for name in BASELINE_FILES {
        let path = workspace.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed: String = content.chars().take(BASELINE_MAX_CHARS).collect();
            if !trimmed.trim().is_empty() {
                return Some((name.to_string(), trimmed));
            }
        }
    }
    None
}

/// One layer of a hierarchical baseline: a file and where it was found.
struct Layer {
    label: String,
    content: String,
}

/// Hierarchical baseline load, after Claude Code's memory hierarchy: the
/// user-level file in `global_dir` first, then every parent directory of
/// `workspace` from the filesystem root down, then the workspace itself.
/// Broadest first, most specific last, so the most specific instructions
/// land closest to the prompt's end. Per directory the first of
/// [`BASELINE_FILES`] wins; a directory is read once even when `global_dir`
/// is also a workspace ancestor. The combined content is capped at
/// [`BASELINE_MAX_CHARS`], the budget spent on the most specific layers
/// first. A lone workspace file returns exactly what
/// [`load_context_baseline`] would.
pub fn load_context_baseline_layered(
    workspace: &Path,
    global_dir: Option<&Path>,
) -> Option<(String, String)> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(g) = global_dir {
        dirs.push(g.to_path_buf());
    }
    let ancestors: Vec<PathBuf> = workspace.ancestors().map(Path::to_path_buf).collect();
    for dir in ancestors.iter().rev() {
        dirs.push(dir.to_path_buf());
    }

    let mut seen = std::collections::HashSet::new();
    let mut layers: Vec<Layer> = Vec::new();
    for dir in &dirs {
        if !seen.insert(dir.clone()) {
            continue;
        }
        for name in BASELINE_FILES {
            let path = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    let label = if *dir == workspace {
                        name.to_string()
                    } else {
                        path.display().to_string()
                    };
                    layers.push(Layer { label, content });
                }
                break;
            }
        }
    }
    if layers.is_empty() {
        return None;
    }
    if layers.len() == 1 {
        let layer = layers.pop()?;
        let trimmed: String = layer.content.chars().take(BASELINE_MAX_CHARS).collect();
        return Some((layer.label, trimmed));
    }

    // Budget to the most specific layers first (they sit last).
    let mut budget = BASELINE_MAX_CHARS;
    let mut bodies: Vec<String> = vec![String::new(); layers.len()];
    for (i, layer) in layers.iter().enumerate().rev() {
        if budget == 0 {
            break;
        }
        let take: String = layer.content.chars().take(budget).collect();
        budget -= take.chars().count();
        bodies[i] = take;
    }
    let name = layers
        .iter()
        .map(|l| l.label.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let content = layers
        .iter()
        .zip(bodies.iter())
        .filter(|(_, b)| !b.trim().is_empty())
        .map(|(l, b)| format!("# From {}\n{}", l.label, b.trim_end()))
        .collect::<Vec<_>>()
        .join("\n\n");
    if content.trim().is_empty() {
        return None;
    }
    Some((name, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_agents_md_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("GEMINI.md"), "gemini rules").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "agents rules").unwrap();
        let (name, content) = load_context_baseline(dir.path()).unwrap();
        assert_eq!(name, "AGENTS.md");
        assert_eq!(content, "agents rules");
    }

    #[test]
    fn missing_baseline_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_context_baseline(dir.path()).is_none());
    }

    #[test]
    fn baseline_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            "x".repeat(BASELINE_MAX_CHARS * 2),
        )
        .unwrap();
        let (_, content) = load_context_baseline(dir.path()).unwrap();
        assert_eq!(content.chars().count(), BASELINE_MAX_CHARS);
    }

    #[test]
    fn layered_single_workspace_file_matches_flat_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "agents rules").unwrap();
        let (name, content) = load_context_baseline_layered(dir.path(), None).unwrap();
        assert_eq!(name, "AGENTS.md");
        assert_eq!(content, "agents rules");
    }

    #[test]
    fn layered_merges_global_parents_and_workspace_most_specific_last() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let parent = root.path().join("proj");
        let workspace = parent.join("crates/app");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(global.join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(parent.join("AGENTS.md"), "parent rules").unwrap();
        std::fs::write(workspace.join("CLAUDE.md"), "workspace rules").unwrap();

        let (name, content) = load_context_baseline_layered(&workspace, Some(&global)).unwrap();
        let gi = content.find("global rules").unwrap();
        let pi = content.find("parent rules").unwrap();
        let wi = content.find("workspace rules").unwrap();
        assert!(gi < pi && pi < wi, "broadest first: {content}");
        assert!(name.contains("CLAUDE.md"), "labels every layer: {name}");
        assert!(content.contains("# From "), "layers are headed: {content}");
    }

    #[test]
    fn layered_reads_a_global_dir_that_is_a_workspace_ancestor_once() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("proj");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();

        let (_, content) = load_context_baseline_layered(&workspace, Some(root.path())).unwrap();
        assert_eq!(content.matches("root rules").count(), 1);
    }

    #[test]
    fn layered_spends_the_budget_on_the_most_specific_layer_first() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let workspace = root.path().join("proj");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(global.join("AGENTS.md"), "g".repeat(BASELINE_MAX_CHARS)).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "w".repeat(100)).unwrap();

        let (_, content) = load_context_baseline_layered(&workspace, Some(&global)).unwrap();
        assert!(content.contains(&"w".repeat(100)), "workspace kept whole");
        let total: usize = content
            .lines()
            .filter(|l| !l.starts_with("# From "))
            .map(|l| l.chars().count() + 1)
            .sum();
        assert!(total <= BASELINE_MAX_CHARS + 8, "capped overall: {total}");
    }
}
