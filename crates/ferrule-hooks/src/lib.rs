//! Lifecycle hooks (M18, `docs/m18-hooks.md`), the parts outside the loop:
//! command hooks, the `[hooks]` config and the workspace's
//! `.ferrule/hooks.toml`, workspace trust, and the audit log. The events,
//! the payload and what a hook's result means are in
//! `ferrule_core::lifecycle`.
//!
//! Hooks run as the owner, outside the sandbox. Nothing here is reachable
//! from a tool: config comes from the trusted config file, and the trust
//! record lives under `<data dir>/private/`.

pub mod audit;
pub mod command;
pub mod config;
pub mod lint;
pub mod trust;

pub use audit::{recent_runs, JsonlAudit};
pub use command::CommandHook;
pub use config::{HookEntry, HooksConfig, WorkspaceHooks};
pub use lint::LintHook;
pub use trust::{load_workspace, TrustStore, WorkspaceState};

use ferrule_core::lifecycle::{Hook, HookLimits, HookSet, HookSource, Matcher};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

/// An agent's hooks, and the owner's notice about workspace hooks that
/// won't run.
pub struct Loaded {
    pub set: HookSet,
    pub notice: Option<String>,
}

fn add(
    set: &mut HookSet,
    config: &HooksConfig,
    entries: Vec<(ferrule_core::HookEvent, &HookEntry)>,
    source: HookSource,
) {
    for (event, entry) in entries {
        set.add(Hook::new(
            event,
            Matcher::parse(entry.matcher.as_deref()),
            source,
            Arc::new(CommandHook::new(
                entry.command.clone(),
                config.timeout(entry),
            )),
        ));
    }
}

/// The hooks for an agent in `workspace`: the user's, then the workspace's
/// if they're switched on and trusted. Every run is logged under
/// `data_dir`.
pub fn build(config: &HooksConfig, workspace: &Path, data_dir: &Path) -> Loaded {
    let mut set = HookSet::new()
        .with_audit(Arc::new(JsonlAudit::in_data_dir(data_dir)))
        .with_limits(HookLimits {
            max_context_chars: config.max_context_chars,
            max_stop_blocks: config.max_stop_blocks,
        });
    add(&mut set, config, config.entries(), HookSource::User);
    let state = load_workspace(
        workspace,
        config.project,
        &TrustStore::in_data_dir(data_dir),
    );
    let notice = state.notice(workspace);
    if let WorkspaceState::Trusted(ws) = &state {
        add(&mut set, config, ws.entries(), HookSource::Workspace);
    }
    Loaded { set, notice }
}

/// `ferrule hooks list`: the configured hooks by event, the workspace's
/// trust state, and the last `runs` runs from the audit log.
pub fn render_list(
    config: &HooksConfig,
    verify_command: Option<&str>,
    workspace: &Path,
    data_dir: &Path,
    runs: usize,
) -> String {
    let mut out = String::new();
    let state = load_workspace(
        workspace,
        config.project,
        &TrustStore::in_data_dir(data_dir),
    );
    let mut rows: Vec<(ferrule_core::HookEvent, &str, String, String)> = Vec::new();
    if let Some(check) = verify_command {
        rows.push((
            ferrule_core::HookEvent::Stop,
            "builtin",
            "*".into(),
            format!("verify_command: {check}"),
        ));
    }
    for (event, entry) in config.entries() {
        rows.push((
            event,
            "user",
            Matcher::parse(entry.matcher.as_deref()).to_string(),
            entry.command.clone(),
        ));
    }
    let ws_label = match &state {
        WorkspaceState::Trusted(_) => "workspace",
        _ => "workspace (won't run)",
    };
    let ws_hooks = match load_workspace(workspace, true, &TrustStore::in_data_dir(data_dir)) {
        WorkspaceState::Trusted(h) => Some(*h),
        _ => std::fs::read_to_string(trust::workspace_file(workspace))
            .ok()
            .and_then(|t| WorkspaceHooks::parse(&t).ok()),
    };
    if let Some(ws) = &ws_hooks {
        for (event, entry) in ws.entries() {
            rows.push((
                event,
                ws_label,
                Matcher::parse(entry.matcher.as_deref()).to_string(),
                entry.command.clone(),
            ));
        }
    }
    rows.sort_by_key(|r| r.0);

    if rows.is_empty() {
        out.push_str("No hooks configured.\n");
    } else {
        out.push_str("Hooks (they run as you, outside the sandbox):\n");
        for (event, source, matcher, command) in &rows {
            let _ = writeln!(
                out,
                "  {:<17} {:<22} {:<16} {}",
                event.name(),
                source,
                matcher,
                command
            );
        }
    }
    if let Some(notice) = state.notice(workspace) {
        let _ = writeln!(out, "\n{notice}");
    }
    if runs > 0 {
        let recent = recent_runs(JsonlAudit::in_data_dir(data_dir).path(), runs);
        if recent.is_empty() {
            out.push_str("\nNo hook runs recorded yet.\n");
        } else {
            let _ = writeln!(out, "\nLast {} runs:", recent.len());
            for r in recent {
                let ts = r["ts"]
                    .as_i64()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_default();
                let outcome = if r["skipped"].as_bool() == Some(true) {
                    "skipped".to_string()
                } else if r["timed_out"].as_bool() == Some(true) {
                    "timed out".to_string()
                } else if r["blocked"].as_bool() == Some(true) {
                    "blocked".to_string()
                } else {
                    match r["exit_code"].as_i64() {
                        Some(code) => format!("exit {code}"),
                        None => "no exit code".into(),
                    }
                };
                let tool = r["tool"]
                    .as_str()
                    .map(|t| format!(" [{t}]"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "  {ts}  {:<16} {:<9} {:<10} {:>6}ms  {}{tool}",
                    r["event"].as_str().unwrap_or("?"),
                    r["source"].as_str().unwrap_or("?"),
                    outcome,
                    r["duration_ms"].as_u64().unwrap_or(0),
                    r["command"].as_str().unwrap_or("?"),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_hooks_come_first_and_untrusted_workspace_hooks_are_left_out() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".ferrule")).unwrap();
        std::fs::write(
            trust::workspace_file(ws.path()),
            "[[PreToolUse]]\ncommand = \"ws-guard\"\n",
        )
        .unwrap();
        let config: HooksConfig = toml::from_str(
            "project = true\nmax_stop_blocks = 5\n[[PreToolUse]]\nmatcher = \"shell\"\ncommand = \"user-guard\"\n",
        )
        .unwrap();

        let loaded = build(&config, ws.path(), data.path());
        assert_eq!(loaded.set.hooks().len(), 1);
        assert_eq!(loaded.set.limits.max_stop_blocks, 5);
        assert!(loaded.notice.unwrap().contains("haven't trusted it yet"));
        let list = render_list(&config, Some("cargo test"), ws.path(), data.path(), 5);
        assert!(list.contains("workspace (won't run)"), "{list}");
        assert!(list.contains("verify_command: cargo test"), "{list}");

        TrustStore::in_data_dir(data.path())
            .trust(ws.path())
            .unwrap();
        let loaded = build(&config, ws.path(), data.path());
        assert!(loaded.notice.is_none());
        let commands: Vec<String> = loaded.set.hooks().iter().map(|h| h.command()).collect();
        assert_eq!(commands, ["user-guard", "ws-guard"]);
        assert_eq!(loaded.set.hooks()[1].source, HookSource::Workspace);
    }
}
