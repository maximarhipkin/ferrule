//! Where extensions live under the data dir. The model's sandbox can't
//! write any of it, and can't read `private/`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    data_dir: PathBuf,
}

impl Layout {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn root(&self) -> PathBuf {
        self.data_dir.join("extensions")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root().join("extensions.lock.json")
    }

    /// A git checkout, one per name and commit.
    pub fn checkout(&self, name: &str, sha: &str) -> PathBuf {
        self.root()
            .join("src")
            .join(format!("{name}-{}", &sha[..sha.len().min(12)]))
    }

    /// Installed skills: a discovery root, after the project roots.
    pub fn skills_dir(&self) -> PathBuf {
        self.root().join("skills")
    }

    /// Installed plugins (M32): `<name>/plugin.json` and its `.wasm`.
    pub fn plugins_dir(&self) -> PathBuf {
        self.root().join("plugins")
    }

    /// Suspended skills are moved here, out of discovery.
    pub fn suspended_skills_dir(&self) -> PathBuf {
        self.root().join("suspended")
    }

    /// Clones and copies in progress; a failed install leaves nothing
    /// elsewhere.
    pub fn staging(&self) -> PathBuf {
        self.root().join("staging")
    }

    pub fn pending_dir(&self) -> PathBuf {
        self.data_dir
            .join("private")
            .join("extensions")
            .join("pending")
    }

    /// A server's own state dir, the same one a configured server got
    /// before the manager ran them (`ferrule-cli`'s `mcp_dir_name`).
    pub fn mcp_state(&self, name: &str) -> PathBuf {
        self.data_dir.join("mcp").join(mcp_dir_name(name))
    }

    /// M63.4: the state dir of a server with `private_state`, under
    /// `<data>/mcp-private`, which commands and the file tools can't read
    /// (the server itself can: `Sandbox::for_helper`).
    pub fn mcp_private_state(&self, name: &str) -> PathBuf {
        self.data_dir.join(PRIVATE_MCP).join(mcp_dir_name(name))
    }

    /// The state dir `cfg`'s server runs with.
    pub fn mcp_state_for(&self, cfg: &ferrule_mcp::McpServerConfig) -> PathBuf {
        if cfg.private_state {
            self.mcp_private_state(&cfg.name)
        } else {
            self.mcp_state(&cfg.name)
        }
    }
}

/// Under the data dir: the private servers' state (M63.4). Hidden from
/// commands like `private/` (it is in the backup's secrets list).
pub const PRIVATE_MCP: &str = "mcp-private";

/// A server name as a directory name. A name that had to be changed gets a
/// hash of the original, so `a.b` and `a_b` don't share a home. Installed
/// servers' names are already safe; configured ones may not be.
pub fn mcp_dir_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe == name && !safe.is_empty() {
        return safe;
    }
    // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{safe}-{:08x}", hash as u32)
}
