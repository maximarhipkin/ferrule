//! `[hooks]` in the config file and the workspace's `.ferrule/hooks.toml`:
//! the same entries, per event, under Claude Code's event names.

use ferrule_core::lifecycle::HookEvent;
use serde::Deserialize;
use std::time::Duration;

/// No hook runs longer than this, whatever its entry says.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// One hook: `[[hooks.PreToolUse]]` (config) or `[[PreToolUse]]` (workspace).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEntry {
    /// Run through the platform's shell, as the owner, outside the sandbox.
    pub command: String,
    /// Tool name, source, trigger or role; `|`-separated names or globs.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Overrides `[hooks] timeout_secs`; capped at 600.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Defines a struct with the given fields plus one `Vec<HookEntry>` per
/// event, and `entries()` over them in event order.
macro_rules! with_events {
    (
        $(#[$m:meta])*
        pub struct $name:ident { $($(#[$fm:meta])* pub $f:ident : $t:ty,)* }
    ) => {
        $(#[$m])*
        pub struct $name {
            $($(#[$fm])* pub $f: $t,)*
            #[serde(rename = "SessionStart")]
            pub session_start: Vec<HookEntry>,
            #[serde(rename = "SessionEnd")]
            pub session_end: Vec<HookEntry>,
            #[serde(rename = "UserPromptSubmit")]
            pub user_prompt_submit: Vec<HookEntry>,
            #[serde(rename = "PreToolUse")]
            pub pre_tool_use: Vec<HookEntry>,
            #[serde(rename = "PostToolUse")]
            pub post_tool_use: Vec<HookEntry>,
            #[serde(rename = "Stop")]
            pub stop: Vec<HookEntry>,
            #[serde(rename = "PreCompact")]
            pub pre_compact: Vec<HookEntry>,
            #[serde(rename = "PostCompact")]
            pub post_compact: Vec<HookEntry>,
            #[serde(rename = "SubagentStart")]
            pub subagent_start: Vec<HookEntry>,
            #[serde(rename = "SubagentStop")]
            pub subagent_stop: Vec<HookEntry>,
        }

        impl $name {
            /// Every entry with its event, in event order then file order.
            pub fn entries(&self) -> Vec<(HookEvent, &HookEntry)> {
                [
                    (HookEvent::SessionStart, &self.session_start),
                    (HookEvent::SessionEnd, &self.session_end),
                    (HookEvent::UserPromptSubmit, &self.user_prompt_submit),
                    (HookEvent::PreToolUse, &self.pre_tool_use),
                    (HookEvent::PostToolUse, &self.post_tool_use),
                    (HookEvent::Stop, &self.stop),
                    (HookEvent::PreCompact, &self.pre_compact),
                    (HookEvent::PostCompact, &self.post_compact),
                    (HookEvent::SubagentStart, &self.subagent_start),
                    (HookEvent::SubagentStop, &self.subagent_stop),
                ]
                .into_iter()
                .flat_map(|(event, list)| list.iter().map(move |e| (event, e)))
                .collect()
            }

            /// A matcher where the event takes none, or an empty command.
            pub fn validate(&self) -> Result<(), String> {
                for (event, entry) in self.entries() {
                    if entry.command.trim().is_empty() {
                        return Err(format!("a {event} hook has an empty `command`"));
                    }
                    let has_matcher = entry.matcher.as_deref().is_some_and(|m| !m.trim().is_empty());
                    if has_matcher && !event.takes_matcher() {
                        return Err(format!(
                            "the {event} hook `{}` has a matcher, but {event} takes none",
                            entry.command
                        ));
                    }
                }
                Ok(())
            }
        }
    };
}

with_events! {
    /// `[hooks]` in the (trusted) config file.
    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct HooksConfig {
        /// Run the workspace's `.ferrule/hooks.toml` once it's trusted.
        pub project: bool,
        /// A hook's timeout when its entry sets none.
        pub timeout_secs: u64,
        /// Times Stop (or SubagentStop) hooks may send one run back.
        pub max_stop_blocks: usize,
        /// Cap on a note or a block reason, in characters.
        pub max_context_chars: usize,
    }
}

impl Default for HooksConfig {
    fn default() -> Self {
        HooksConfig {
            project: false,
            timeout_secs: 60,
            max_stop_blocks: 3,
            max_context_chars: 10_000,
            session_start: Vec::new(),
            session_end: Vec::new(),
            user_prompt_submit: Vec::new(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            stop: Vec::new(),
            pre_compact: Vec::new(),
            post_compact: Vec::new(),
            subagent_start: Vec::new(),
            subagent_stop: Vec::new(),
        }
    }
}

impl HooksConfig {
    /// The timeout for `entry`: its own or the default, at most 600s.
    pub fn timeout(&self, entry: &HookEntry) -> Duration {
        let secs = entry
            .timeout_secs
            .unwrap_or(self.timeout_secs)
            .min(MAX_TIMEOUT_SECS);
        Duration::from_secs(secs)
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
}

with_events! {
    /// `.ferrule/hooks.toml`: entries only, no settings.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct WorkspaceHooks {}
}

impl WorkspaceHooks {
    pub fn parse(text: &str) -> Result<WorkspaceHooks, String> {
        let hooks: WorkspaceHooks = toml::from_str(text).map_err(|e| e.to_string())?;
        hooks.validate()?;
        Ok(hooks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_section_parses_with_its_defaults() {
        let cfg: HooksConfig = toml::from_str(
            r#"
            project = true
            [[PreToolUse]]
            matcher = "shell"
            command = "guard.sh"
            timeout_secs = 5000
            [[Stop]]
            command = "notify"
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert!(cfg.project);
        assert_eq!(cfg.max_stop_blocks, 3);
        let entries = cfg.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, HookEvent::PreToolUse);
        assert_eq!(cfg.timeout(entries[0].1), Duration::from_secs(600));
        assert_eq!(cfg.timeout(entries[1].1), Duration::from_secs(60));
    }

    #[test]
    fn unknown_events_keys_and_misplaced_matchers_are_errors() {
        assert!(toml::from_str::<HooksConfig>("[[PreToolUze]]\ncommand = \"x\"").is_err());
        assert!(toml::from_str::<HooksConfig>("[[Stop]]\ncommand = \"x\"\nmatch = \"y\"").is_err());
        let cfg: HooksConfig =
            toml::from_str("[[Stop]]\ncommand = \"x\"\nmatcher = \"y\"").unwrap();
        assert!(cfg.validate().unwrap_err().contains("Stop takes none"));
        // A workspace file can't set the switch or raise its limits.
        assert!(WorkspaceHooks::parse("project = true").is_err());
        assert!(WorkspaceHooks::parse("max_stop_blocks = 99").is_err());
        let ws = WorkspaceHooks::parse("[[PostToolUse]]\ncommand = \"lint\"").unwrap();
        assert_eq!(ws.entries().len(), 1);
    }
}
