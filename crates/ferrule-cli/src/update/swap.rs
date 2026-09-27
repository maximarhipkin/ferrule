//! Putting the new binary in place (docs/m36-self-update.md §3.3): a copy
//! beside the old one, renamed over it, with the old one kept as
//! `ferrule.previous`. A running process keeps its open file and never sees
//! a half-written one. Windows can't replace a running exe but can rename
//! it, so the running one steps aside as `ferrule.exe.<8 hex>.old`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// `ferrule.previous` (`ferrule.previous.exe` on Windows) beside `exe`.
pub fn previous_path(exe: &Path) -> PathBuf {
    sibling(exe, "previous")
}

/// Install `new` as `exe`, keeping the old one as `previous_path(exe)`.
pub fn swap(exe: &Path, new: &Path) -> Result<PathBuf> {
    let previous = previous_path(exe);
    let _ = std::fs::remove_file(&previous);
    if std::fs::hard_link(exe, &previous).is_err() {
        std::fs::copy(exe, &previous)
            .with_context(|| format!("keeping {} as {}", exe.display(), previous.display()))?;
    }
    replace(exe, new)?;
    Ok(previous)
}

/// Put `ferrule.previous` back as `exe` (it stays, for another rollback).
pub fn roll_back(exe: &Path) -> Result<()> {
    let previous = previous_path(exe);
    replace(exe, &previous).with_context(|| format!("restoring {}", previous.display()))
}

/// Copy `from` beside `exe`, then rename it over `exe`.
fn replace(exe: &Path, from: &Path) -> Result<()> {
    let staged = sibling(exe, "new");
    let staged = staged.with_file_name(format!(
        ".{}",
        staged.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(from, &staged)
        .with_context(|| format!("copying the new binary to {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let result = rename_over(&staged, exe);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

#[cfg(not(windows))]
fn rename_over(staged: &Path, exe: &Path) -> Result<()> {
    std::fs::rename(staged, exe).with_context(|| format!("replacing {}", exe.display()))
}

/// The running exe can be renamed, not overwritten: it steps aside first,
/// and comes back if the new one can't take its place.
#[cfg(windows)]
fn rename_over(staged: &Path, exe: &Path) -> Result<()> {
    let aside = exe.with_file_name(format!(
        "{}.{}.old",
        exe.file_name().unwrap_or_default().to_string_lossy(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    if exe.exists() {
        std::fs::rename(exe, &aside).with_context(|| format!("moving {} aside", exe.display()))?;
    }
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&aside, exe);
        return Err(e).with_context(|| format!("replacing {}", exe.display()));
    }
    // Deleted now if nothing runs it, else at the next start.
    let _ = std::fs::remove_file(&aside);
    Ok(())
}

/// Delete the `.old` files a Windows swap left beside `exe`.
pub fn clean_old(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    let prefix = format!("{}.", name.to_string_lossy());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with(&prefix) && file.ends_with(".old") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `ferrule.<word>` beside `exe`, keeping `.exe` last on Windows.
fn sibling(exe: &Path, word: &str) -> PathBuf {
    let stem = exe
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    match exe.extension() {
        Some(ext) => exe.with_file_name(format!("{stem}.{word}.{}", ext.to_string_lossy())),
        None => exe.with_file_name(format!("{stem}.{word}")),
    }
}
