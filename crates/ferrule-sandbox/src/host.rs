//! The gateway's own spawns (hooks, scheduler gates, transcription, tunnel
//! and updater helpers) in owner mode.
//!
//! In owner mode nothing the model can influence may run outside the
//! sandbox. Those spawns live in crates that don't carry a [`Sandbox`], so
//! the gateway sets one process-wide launcher here and they all go through
//! [`command`]. Unset, it is a plain `Command`.

use crate::Sandbox;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ponytail: one process-wide launcher, not a sandbox threaded through five
// crates. Per-session launchers if a guest ever needs hooks.
static HOST: OnceLock<Sandbox> = OnceLock::new();

/// Sets the sandbox the gateway's spawns run under. Once per process, only
/// in owner mode; a second call keeps the first.
pub fn set(sandbox: Sandbox) {
    let _ = HOST.set(sandbox);
}

/// A `Command` for `program args…` in `cwd`: under the sandbox set by
/// [`set`], else a plain one. Callers add stdio and spawn.
pub fn command<I, S>(program: impl AsRef<OsStr>, args: I, cwd: &Path) -> io::Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match HOST.get() {
        Some(sandbox) => sandbox.command(program, args, cwd),
        None => {
            let mut cmd = Command::new(program);
            cmd.args(args).current_dir(cwd);
            Ok(cmd)
        }
    }
}

/// Where owner mode looks for a program the gateway runs by bare name
/// outside the sandbox: only these system dirs, never `PATH`, where the
/// bot's own `~/.local/bin` may come first.
const SYSTEM_DIRS: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// `name` as a path for a spawn that stays outside the sandbox. Owner mode
/// (a launcher is set) resolves it in the system dirs, skipping any entry
/// that isn't root-owned and closed to group/other writes, so a program the
/// bot dropped on `PATH` or in a user-writable dir can't stand in. With no
/// match it is `/usr/bin/<name>`, so the spawn fails "not found" instead of
/// searching `PATH`. With no launcher it is `name` unchanged.
// ponytail: trusts the dirs themselves to be root-owned; a bot that can
// write them has already won.
pub fn system_program(name: &str) -> PathBuf {
    if HOST.get().is_none() {
        return PathBuf::from(name);
    }
    SYSTEM_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| trusted(p))
        .unwrap_or_else(|| Path::new("/usr/bin").join(name))
}

/// Root-owned and not writable by group or other.
#[cfg(unix)]
fn trusted(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
}

#[cfg(not(unix))]
fn trusted(p: &Path) -> bool {
    p.exists()
}
