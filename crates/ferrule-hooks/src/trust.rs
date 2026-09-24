//! Workspace hooks run only once the owner has trusted the file as it is
//! now: its SHA-256, recorded against the workspace in
//! `<data dir>/private/hooks-trust.json`. Any change to the file, by
//! anyone, needs trusting again.

use crate::config::WorkspaceHooks;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The workspace's hooks file, relative to the workspace.
pub const WORKSPACE_FILE: &str = ".ferrule/hooks.toml";

pub fn workspace_file(workspace: &Path) -> PathBuf {
    workspace.join(WORKSPACE_FILE)
}

/// SHA-256 of `bytes`, as lowercase hex.
pub fn fingerprint(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A workspace's key in the record: its canonical path when it has one.
fn key(workspace: &Path) -> String {
    std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The trust record.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    /// The record under `data_dir` (`<data dir>/private/hooks-trust.json`).
    pub fn in_data_dir(data_dir: &Path) -> TrustStore {
        TrustStore {
            path: data_dir.join("private").join("hooks-trust.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, map: &BTreeMap<String, String>) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(map)?)?;
        std::fs::rename(tmp, &self.path)
    }

    /// The fingerprint trusted for `workspace`, if any.
    pub fn trusted(&self, workspace: &Path) -> Option<String> {
        self.load().remove(&key(workspace))
    }

    /// Trusts the workspace's hooks file as it is now. Returns how many
    /// hooks it has. A file that doesn't parse can't be trusted.
    pub fn trust(&self, workspace: &Path) -> Result<usize, String> {
        let file = workspace_file(workspace);
        let bytes =
            std::fs::read(&file).map_err(|e| format!("can't read {}: {e}", file.display()))?;
        let hooks = WorkspaceHooks::parse(&String::from_utf8_lossy(&bytes))
            .map_err(|e| format!("{} doesn't parse: {e}", file.display()))?;
        let mut map = self.load();
        map.insert(key(workspace), fingerprint(&bytes));
        self.save(&map)
            .map_err(|e| format!("can't write {}: {e}", self.path.display()))?;
        Ok(hooks.entries().len())
    }

    /// Forgets the workspace. Returns whether it was trusted.
    pub fn untrust(&self, workspace: &Path) -> Result<bool, String> {
        let mut map = self.load();
        let was = map.remove(&key(workspace)).is_some();
        if was {
            self.save(&map)
                .map_err(|e| format!("can't write {}: {e}", self.path.display()))?;
        }
        Ok(was)
    }
}

/// What a workspace's hooks file comes to.
#[derive(Debug, Clone)]
pub enum WorkspaceState {
    /// No `.ferrule/hooks.toml`.
    Absent,
    /// It has hooks that won't run, and why.
    Untrusted {
        count: usize,
        why: String,
    },
    Trusted(Box<WorkspaceHooks>),
}

impl WorkspaceState {
    /// The owner's notice for hooks that won't run.
    pub fn notice(&self, workspace: &Path) -> Option<String> {
        match self {
            WorkspaceState::Untrusted { count, why } => Some(format!(
                "ferrule: {} has {count} hook{} that won't run: {why}. Run `ferrule hooks trust` if you trust them.",
                workspace_file(workspace).display(),
                if *count == 1 { "" } else { "s" },
            )),
            _ => None,
        }
    }
}

/// Reads the workspace's hooks file and decides whether it may run.
/// A file that doesn't parse is reported, not fatal: a broken cloned repo
/// shouldn't stop the agent.
pub fn load_workspace(workspace: &Path, project: bool, trust: &TrustStore) -> WorkspaceState {
    let file = workspace_file(workspace);
    let Ok(bytes) = std::fs::read(&file) else {
        return WorkspaceState::Absent;
    };
    let hooks = match WorkspaceHooks::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(hooks) => hooks,
        Err(e) => {
            return WorkspaceState::Untrusted {
                count: 0,
                why: format!("it doesn't parse ({e})"),
            }
        }
    };
    let count = hooks.entries().len();
    if count == 0 {
        return WorkspaceState::Absent;
    }
    let why = if !project {
        "`[hooks] project` is off in your config"
    } else {
        match trust.trusted(workspace) {
            None => "you haven't trusted it yet",
            Some(fp) if fp != fingerprint(&bytes) => "it changed since you trusted it",
            Some(_) => return WorkspaceState::Trusted(Box::new(hooks)),
        }
    };
    WorkspaceState::Untrusted {
        count,
        why: why.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, TrustStore) {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".ferrule")).unwrap();
        std::fs::write(
            workspace_file(ws.path()),
            "[[PreToolUse]]\nmatcher = \"shell\"\ncommand = \"guard\"\n",
        )
        .unwrap();
        let store = TrustStore::in_data_dir(data.path());
        (ws, data, store)
    }

    #[test]
    fn hooks_run_only_when_switched_on_and_trusted_as_they_are() {
        let (ws, _data, store) = setup();
        let state = load_workspace(ws.path(), false, &store);
        assert!(state
            .notice(ws.path())
            .unwrap()
            .contains("`[hooks] project` is off"));
        let state = load_workspace(ws.path(), true, &store);
        let notice = state.notice(ws.path()).unwrap();
        assert!(notice.contains("has 1 hook that won't run: you haven't trusted it yet"));
        assert!(notice.contains("ferrule hooks trust"));

        assert_eq!(store.trust(ws.path()).unwrap(), 1);
        assert!(store.path().starts_with(_data.path().join("private")));
        assert!(matches!(
            load_workspace(ws.path(), true, &store),
            WorkspaceState::Trusted(_)
        ));

        // Edited afterwards (by anyone): not trusted any more.
        std::fs::write(
            workspace_file(ws.path()),
            "[[PreToolUse]]\ncommand = \"something-else\"\n",
        )
        .unwrap();
        let state = load_workspace(ws.path(), true, &store);
        assert!(state
            .notice(ws.path())
            .unwrap()
            .contains("changed since you trusted it"));

        store.trust(ws.path()).unwrap();
        assert!(store.untrust(ws.path()).unwrap());
        assert!(!store.untrust(ws.path()).unwrap());
        assert!(matches!(
            load_workspace(ws.path(), true, &store),
            WorkspaceState::Untrusted { .. }
        ));
    }

    #[test]
    fn a_broken_file_is_reported_and_cant_be_trusted() {
        let (ws, _data, store) = setup();
        std::fs::write(
            workspace_file(ws.path()),
            "[[PreToolUze]]\ncommand = \"x\"\n",
        )
        .unwrap();
        let state = load_workspace(ws.path(), true, &store);
        assert!(state.notice(ws.path()).unwrap().contains("doesn't parse"));
        assert!(store.trust(ws.path()).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_workspace(empty.path(), true, &store),
            WorkspaceState::Absent
        ));
    }
}
