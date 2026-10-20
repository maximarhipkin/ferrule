//! M57.3: what each file looked like when the agent last read or wrote it,
//! so `edit_file` can refuse an edit planned against bytes that are no
//! longer there (docs/m57-context-hygiene.md, "A stale-file guard").
//!
//! One map per agent, shared by its `read_file`, `write_file` and
//! `edit_file` (and handed to the agent, which adopts what its own hooks
//! and checks change). A file the agent never read has no stamp and stays
//! editable: SEARCH must match exactly either way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// A file's bytes, reduced to what a comparison needs. `modified` only
/// lets an unchanged file skip a re-read; the length and hash decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    hash: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(path: &Path, bytes: &[u8]) -> Stamp {
        Stamp {
            len: bytes.len() as u64,
            hash: crate::transcript::fnv64(bytes),
            modified: modified(path),
        }
    }

    fn matches(&self, bytes: &[u8]) -> bool {
        self.len == bytes.len() as u64 && self.hash == crate::transcript::fnv64(bytes)
    }

    /// Whether the file's metadata says it wasn't touched since.
    fn untouched(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        self.modified.is_some() && meta.len() == self.len && meta.modified().ok() == self.modified
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Paths (as the file tools resolved them) to the last bytes the agent
/// read or wrote there. Cheap to clone: the clones share the map.
#[derive(Debug, Clone, Default)]
pub struct FileStamps {
    files: Arc<Mutex<HashMap<PathBuf, Stamp>>>,
}

impl FileStamps {
    /// `bytes` is what `path` holds now, as the agent just read or wrote it.
    pub fn record(&self, path: &Path, bytes: &[u8]) {
        let stamp = Stamp::of(path, bytes);
        self.files.lock().unwrap().insert(path.to_path_buf(), stamp);
    }

    /// Whether `bytes`, just read from `path`, differ from what the agent
    /// last read or wrote there. A file it never read hasn't changed.
    pub fn changed(&self, path: &Path, bytes: &[u8]) -> bool {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|s| !s.matches(bytes))
    }

    /// The files that still hold what the agent last saw. Blocking I/O.
    pub fn current(&self) -> Vec<PathBuf> {
        let files: Vec<(PathBuf, Stamp)> = self
            .files
            .lock()
            .unwrap()
            .iter()
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        files
            .into_iter()
            .filter(|(path, stamp)| {
                stamp.untouched(path) || std::fs::read(path).is_ok_and(|b| stamp.matches(&b))
            })
            .map(|(path, _)| path)
            .collect()
    }

    /// Takes what `paths` hold now as seen: ferrule's own hooks and checks
    /// changed them (a formatter in the verify command, say), not someone
    /// the agent should re-read for. A file gone is forgotten. Blocking I/O.
    pub fn adopt(&self, paths: &[PathBuf]) {
        for path in paths {
            let known = self.files.lock().unwrap().get(path).copied();
            if known.is_some_and(|s| s.untouched(path)) {
                continue;
            }
            match std::fs::read(path) {
                Ok(bytes) => self.record(path, &bytes),
                Err(_) => {
                    self.files.lock().unwrap().remove(path);
                }
            }
        }
    }

    /// Runs `work` (the agent's hooks or checks) and adopts what it changed
    /// in the files that were current before it.
    pub async fn adopting<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        let me = self.clone();
        let before = tokio::task::spawn_blocking(move || me.current())
            .await
            .unwrap_or_default();
        let out = work.await;
        if !before.is_empty() {
            let me = self.clone();
            let _ = tokio::task::spawn_blocking(move || me.adopt(&before)).await;
        }
        out
    }
}
