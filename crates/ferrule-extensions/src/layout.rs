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

    /// A server's own state dir, the same one a configured server gets.
    pub fn mcp_state(&self, name: &str) -> PathBuf {
        self.data_dir.join("mcp").join(name)
    }
}
