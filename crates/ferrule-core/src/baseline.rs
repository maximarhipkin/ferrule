use std::path::Path;

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
}
