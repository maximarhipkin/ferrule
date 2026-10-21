//! Standing approvals (M56.2): what the owner said "Always here" to, kept
//! across runs and restarts in `<data>/private/grants.json`. The
//! directory is hidden from sandboxed commands and refused to the file
//! tools, so the agent can neither read nor write it.
//!
//! Two shapes, both narrow on purpose:
//! - a shell command: its gated command word for word (one simple command,
//!   nothing the shell would expand), in one workspace;
//! - a connected service's change: one tool to one address, or to every
//!   address at a domain (`@example.com`, widened on the dashboard).
//!
//! The file is read on every check, so a revoke in another `ferrule`
//! process holds at once.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    /// When the owner gave it (RFC 3339).
    pub created: String,
    /// What the gate called it ("recursive delete").
    pub kind: String,
    /// `shell`, or the connected service's tool (`mcp__gmail__gmail_send`).
    pub tool: String,
    /// A shell grant: the command's words, quotes removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// A shell grant: the workspace it ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// A connected change: the address, or `@domain`, lower-cased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl Grant {
    /// One line for the owner: what it lets through, and where.
    pub fn describe(&self) -> String {
        match (&self.cwd, &self.to) {
            (Some(cwd), _) => format!("`{}` in {cwd} ({})", self.argv.join(" "), self.kind),
            (_, Some(to)) => format!("{} to {to}", self.tool),
            _ => self.tool.clone(),
        }
    }

    fn covers_address(&self, address: &str) -> bool {
        match self.to.as_deref() {
            Some(d) if d.starts_with('@') => address.ends_with(d) && address.len() > d.len(),
            Some(a) => a == address,
            None => false,
        }
    }
}

/// What "Always here" on one question would keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Command {
        kind: String,
        argv: Vec<String>,
        cwd: String,
    },
    Send {
        kind: String,
        tool: String,
        to: Vec<String>,
    },
}

impl Spec {
    /// The words on the card, after "or `remember` to allow ".
    pub fn offer(&self) -> String {
        match self {
            Spec::Command { cwd, .. } => format!("this exact command in {cwd} from now on"),
            Spec::Send { tool, to, .. } => format!("{tool} to {} from now on", to.join(", ")),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    grants: Vec<Grant>,
}

pub struct Grants {
    path: PathBuf,
    /// One writer at a time in this process.
    lock: Mutex<()>,
}

impl Grants {
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// `<data>/private/grants.json`.
    pub fn in_data_dir(data: &Path) -> Self {
        Self::at(data.join("private").join("grants.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every standing grant, oldest first. A missing or unreadable file is
    /// none: nothing is let through by a file that doesn't parse.
    pub fn list(&self) -> Vec<Grant> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str::<File>(&t).ok())
            .map(|f| f.grants)
            .unwrap_or_default()
    }

    /// The grant that lets `argv` run in `cwd`, if one does.
    pub fn command(&self, argv: &[String], cwd: &str) -> Option<Grant> {
        self.list()
            .into_iter()
            .find(|g| g.tool == "shell" && g.argv == argv && g.cwd.as_deref() == Some(cwd))
    }

    /// The grants that let `tool` change things for every one of `to`;
    /// `None` when one address isn't covered (or there's none).
    pub fn send(&self, tool: &str, to: &[String]) -> Option<Vec<String>> {
        let list = self.list();
        if to.is_empty() {
            return None;
        }
        let mut ids = Vec::new();
        for a in to {
            let g = list
                .iter()
                .find(|g| g.tool == tool && g.covers_address(a))?;
            if !ids.contains(&g.id) {
                ids.push(g.id.clone());
            }
        }
        Some(ids)
    }

    /// Keeps what `spec` says, `now` as its date. Returns the new grants.
    pub fn add(&self, spec: &Spec, now: &str) -> Result<Vec<Grant>, String> {
        let new: Vec<Grant> = match spec {
            Spec::Command { kind, argv, cwd } => vec![Grant {
                id: new_id(),
                created: now.to_string(),
                kind: kind.clone(),
                tool: "shell".into(),
                argv: argv.clone(),
                cwd: Some(cwd.clone()),
                to: None,
            }],
            Spec::Send { kind, tool, to } => to
                .iter()
                .map(|a| Grant {
                    id: new_id(),
                    created: now.to_string(),
                    kind: kind.clone(),
                    tool: tool.clone(),
                    argv: Vec::new(),
                    cwd: None,
                    to: Some(a.clone()),
                })
                .collect(),
        };
        self.edit(|list| {
            for g in &new {
                let same = |o: &Grant| {
                    o.tool == g.tool && o.argv == g.argv && o.cwd == g.cwd && o.to == g.to
                };
                if !list.iter().any(same) {
                    list.push(g.clone());
                }
            }
        })?;
        Ok(new)
    }

    /// Drops grant `id`; `None` when there's no such grant.
    pub fn revoke(&self, id: &str) -> Result<Option<Grant>, String> {
        let mut gone = None;
        self.edit(|list| {
            if let Some(i) = list.iter().position(|g| g.id == id) {
                gone = Some(list.remove(i));
            }
        })?;
        Ok(gone)
    }

    /// A send grant to one address becomes one to its whole domain.
    pub fn widen(&self, id: &str) -> Result<Option<Grant>, String> {
        let mut done = None;
        self.edit(|list| {
            if let Some(g) = list.iter_mut().find(|g| g.id == id) {
                if let Some(domain) =
                    g.to.as_deref()
                        .filter(|t| !t.starts_with('@'))
                        .and_then(|t| t.rsplit_once('@'))
                        .map(|(_, d)| format!("@{d}"))
                        .filter(|d| d.len() > 1)
                {
                    g.to = Some(domain);
                    done = Some(g.clone());
                }
            }
        })?;
        Ok(done)
    }

    fn edit(&self, change: impl FnOnce(&mut Vec<Grant>)) -> Result<(), String> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut list = self.list();
        change(&mut list);
        let text =
            serde_json::to_string_pretty(&File { grants: list }).map_err(|e| e.to_string())?;
        write_private(&self.path, &text).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// 0600 in a 0700 directory, written whole and renamed into place.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}
