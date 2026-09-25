//! Writing a file other processes read: a lock next to it, then a
//! temporary file renamed over it, so a reader sees the old text or the new
//! one (docs/m21-models.md §4).

use anyhow::{Context, Result};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// A lock older than this was left by a process that died holding it.
const STALE: Duration = Duration::from_secs(10);
/// How long to wait for a lock, or for Windows to let go of a file.
const WAIT: Duration = Duration::from_secs(5);

/// Held while a read-modify-write of `path` runs; removed on drop.
pub struct Lock(PathBuf);

impl Lock {
    /// `<path>.lock`, taken with `create_new`.
    pub fn take(path: &Path) -> Result<Self> {
        let mut lk = path.as_os_str().to_owned();
        lk.push(".lock");
        let lk = PathBuf::from(lk);
        if let Some(dir) = lk.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lk)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self(lk));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&lk)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > STALE) {
                        tracing::warn!(path = %lk.display(), "taking over a stale lock");
                        let _ = std::fs::remove_file(&lk);
                        continue;
                    }
                    if start.elapsed() > WAIT {
                        anyhow::bail!("{} is held by another process; try again", lk.display());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    // A lock we can't even create (a read-only dir, or
                    // Windows' access denied on a file being deleted):
                    // retry for a moment, then say so.
                    if start.elapsed() > WAIT {
                        return Err(e).with_context(|| format!("taking {}", lk.display()));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Rename `tmp` over `path`. On Windows a scanner or an editor holding
/// `path` open makes the rename fail with access denied for a moment, so
/// that's retried for up to 5 s.
pub fn replace(tmp: &Path, path: &Path) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        match std::fs::rename(tmp, path) {
            Err(e) if e.kind() == ErrorKind::PermissionDenied && start.elapsed() < WAIT => {
                std::thread::sleep(Duration::from_millis(25));
            }
            done => return done,
        }
    }
}

/// Write `text` to `path` through `<path>.tmp-<pid>`. The caller holds the
/// lock.
pub fn write(path: &Path, text: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    replace(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("writing {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_goes_on_drop_and_a_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        let held = Lock::take(&path).unwrap();
        let lk = dir.path().join("c.toml.lock");
        assert!(lk.exists());
        // Released on drop, so a second take succeeds.
        drop(held);
        assert!(!lk.exists());
        let _again = Lock::take(&path).unwrap();
        write(&path, b"x = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1\n");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "no temp file left"
        );
    }
}
