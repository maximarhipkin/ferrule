//! M44: how a managed bot starts again and stops (docs/m44-managed-mode.md §6).

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use ferrule_gateway::{Drained, Router};

/// What a chat is told when its message is turned away or dropped by a stop.
pub const NOTICE: &str = "The bot is restarting; send that again in a minute.";

type Snapshot = (Vec<(OsString, OsString)>, Vec<OsString>);

static SNAP: OnceLock<Snapshot> = OnceLock::new();
static RESTART: AtomicBool = AtomicBool::new(false);

/// Keeps the env and args as `main` got them, before any secret was taken
/// out, so a restart in place can start ferrule with the same ones.
pub fn snapshot_env() {
    let _ = SNAP.set((std::env::vars_os().collect(), std::env::args_os().collect()));
}

fn snapshot() -> &'static Snapshot {
    SNAP.get_or_init(|| (std::env::vars_os().collect(), std::env::args_os().collect()))
}

/// The gateway's stop: no new message, the running turns get `grace`, the
/// ones still going are ended.
pub async fn drain(router: &Router, grace: Duration) -> Drained {
    tracing::info!(
        running = router.running(),
        queued = router.queued(),
        grace_secs = grace.as_secs(),
        "stopping: running turns get time to finish"
    );
    let got = router.drain(NOTICE, grace).await;
    tracing::info!(
        notified = got.notified,
        stopped = got.stopped,
        left = got.left,
        "stopped taking messages"
    );
    got
}

/// Asks for a restart in place: the same signal a stop is, and once the
/// gateway is down, `main` runs ferrule again in this process.
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    // SAFETY: signalling our own pid; the gateway's handler shuts down.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }
    #[cfg(not(unix))]
    std::process::exit(0);
}

/// [`request_restart`] was called.
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// Runs ferrule again in this process: same binary, same arguments, the
/// environment it was started with. Only returns to `main` as an exit.
#[cfg(unix)]
pub fn reexec() -> ! {
    use std::os::unix::process::CommandExt;
    let (env, args) = snapshot();
    let exe = if cfg!(target_os = "linux") {
        std::path::PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap_or_else(|_| "ferrule".into())
    };
    let mut cmd = std::process::Command::new(exe);
    if let Some(arg0) = args.first() {
        cmd.arg0(arg0);
    }
    cmd.args(args.iter().skip(1))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)));
    let err = cmd.exec();
    eprintln!("the restart couldn't start ferrule again: {err}");
    std::process::exit(1);
}
