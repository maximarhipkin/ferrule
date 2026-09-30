//! M44: how a managed bot starts again and stops (docs/m44-managed-mode.md §6).

use std::ffi::OsString;
use std::sync::OnceLock;

type Snapshot = (Vec<(OsString, OsString)>, Vec<OsString>);

static SNAP: OnceLock<Snapshot> = OnceLock::new();

/// Keeps the env and args as `main` got them, before any secret was taken
/// out, so a restart in place can start ferrule with the same ones.
pub fn snapshot_env() {
    let _ = SNAP.set((std::env::vars_os().collect(), std::env::args_os().collect()));
}

#[allow(dead_code)] // the re-exec reads it (M44 part 5)
pub fn snapshot() -> &'static Snapshot {
    SNAP.get_or_init(|| (std::env::vars_os().collect(), std::env::args_os().collect()))
}
