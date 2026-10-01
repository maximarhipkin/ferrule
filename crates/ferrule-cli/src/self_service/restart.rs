//! Restarting from the chat: how this setup restarts, and when (after the
//! turn that asked has finished, so its answer isn't cut off).

use ferrule_gateway::Router;
use std::sync::Weak;
use std::time::Duration;

/// How long a restart waits for the asking turn to end.
const WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The same signal a stop is; `main` runs ferrule again in this
    /// process (the container's init, or a terminal).
    Reexec,
    /// A clean exit; the service manager starts it again.
    Terminate,
    Unavailable,
}

pub fn how(managed: bool, supervised: bool, unix: bool) -> How {
    if managed {
        How::Reexec
    } else if supervised {
        How::Terminate
    } else if unix {
        How::Reexec
    } else {
        How::Unavailable
    }
}

/// What the owner approves: nothing here asks for a setup that can't
/// restart.
pub fn card() -> Result<String, String> {
    let how = how(
        crate::managed::on(),
        crate::last_good::supervised(),
        cfg!(unix),
    );
    if how == How::Unavailable {
        return Err(
            "Restarting from the chat isn't available on Windows yet; nothing changed.".into(),
        );
    }
    Ok(
        "Restart Ferrule. Running turns get a few seconds to finish, then it is offline for \
        about a minute; I tell you here when it's back."
            .into(),
    )
}

/// Restarts this process now.
pub fn now() -> Result<(), String> {
    match how(
        crate::managed::on(),
        crate::last_good::supervised(),
        cfg!(unix),
    ) {
        How::Reexec => crate::lifecycle::request_restart(),
        How::Terminate => crate::dashboard::api::terminate_self(),
        How::Unavailable => {
            return Err(
                "Restarting from the chat isn't available on Windows yet; nothing changed.".into(),
            )
        }
    }
    Ok(())
}

/// Calls `then` once `session`'s lane is idle (or after two minutes, or
/// when the router is gone).
pub fn after_turn(router: Weak<Router>, session: String, then: impl FnOnce() + Send + 'static) {
    tokio::spawn(async move {
        turn_over(&router, &session).await;
        then();
    });
}

/// Waits for `session`'s lane to be idle (two minutes at most).
pub async fn turn_over(router: &Weak<Router>, session: &str) {
    let until = tokio::time::Instant::now() + WAIT;
    loop {
        let busy = router.upgrade().map(|r| r.busy(session));
        if busy != Some(true) || tokio::time::Instant::now() >= until {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_picks_the_restart_for_each_setup() {
        for managed in [false, true] {
            for supervised in [false, true] {
                for unix in [false, true] {
                    let want = if managed {
                        How::Reexec
                    } else if supervised {
                        How::Terminate
                    } else if unix {
                        How::Reexec
                    } else {
                        How::Unavailable
                    };
                    assert_eq!(how(managed, supervised, unix), want);
                }
            }
        }
        assert_eq!(how(false, false, false), How::Unavailable);
        assert_eq!(how(false, true, false), How::Terminate);
        assert_eq!(how(false, false, true), How::Reexec);
        assert_eq!(how(true, false, false), How::Reexec);
    }

    #[tokio::test]
    async fn after_turn_runs_when_the_router_is_gone() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        after_turn(Weak::new(), "s".into(), move || {
            let _ = tx.send(());
        });
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
    }
}
