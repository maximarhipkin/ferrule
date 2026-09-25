//! M19b: systemd's watchdog. The unit sets `WatchdogSec`; systemd hands the
//! service `NOTIFY_SOCKET` and `WATCHDOG_USEC`, and restarts it when the
//! `WATCHDOG=1` datagrams stop. The gateway sends them only while it can
//! actually hear the owner (see `Health::watchdog_ok`). Linux only; with
//! no `WATCHDOG_USEC` (an older unit, or not under systemd) it's off.

use std::time::Duration;

/// Where and how often to ping.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemdWatchdog {
    /// `NOTIFY_SOCKET`: a path, or `@name` for an abstract socket.
    pub socket: String,
    /// A third of `WATCHDOG_USEC`.
    pub every: Duration,
}

impl SystemdWatchdog {
    /// From the environment systemd sets; `None` when the watchdog isn't
    /// on for this process.
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::from_vars(
            var("NOTIFY_SOCKET"),
            var("WATCHDOG_USEC"),
            var("WATCHDOG_PID"),
            std::process::id(),
        )
    }

    pub fn from_vars(
        socket: Option<String>,
        usec: Option<String>,
        pid: Option<String>,
        own_pid: u32,
    ) -> Option<Self> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let socket = socket?;
        let usec: u64 = usec?.trim().parse().ok().filter(|u| *u > 0)?;
        // Set for another process (a parent that forked us): not ours.
        if pid.is_some_and(|p| p.trim().parse::<u32>().ok() != Some(own_pid)) {
            return None;
        }
        Some(Self {
            socket,
            every: (Duration::from_micros(usec) / 3).max(Duration::from_millis(10)),
        })
    }

    /// Sends one datagram (`WATCHDOG=1`, `READY=1`, `STOPPING=1`).
    pub fn notify(&self, state: &str) -> std::io::Result<()> {
        send(&self.socket, state.as_bytes())
    }
}

#[cfg(target_os = "linux")]
fn send(socket: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let sock = UnixDatagram::unbound()?;
    match socket.strip_prefix('@') {
        Some(name) => {
            let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
            sock.send_to_addr(bytes, &addr)?;
        }
        None => {
            sock.send_to(bytes, socket)?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send(_socket: &str, _bytes: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "sd_notify is Linux only",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn the_environment_turns_it_on_only_for_this_process() {
        let on = |s: Option<&str>, u: Option<&str>, p: Option<&str>| {
            SystemdWatchdog::from_vars(s.map(Into::into), u.map(Into::into), p.map(Into::into), 7)
        };
        assert_eq!(
            on(Some("/run/n"), Some("120000000"), None),
            Some(SystemdWatchdog {
                socket: "/run/n".into(),
                every: Duration::from_secs(40)
            })
        );
        assert!(on(Some("/run/n"), Some("120000000"), Some("7")).is_some());
        assert_eq!(on(Some("/run/n"), Some("120000000"), Some("8")), None);
        assert_eq!(on(Some("/run/n"), None, None), None);
        assert_eq!(on(Some("/run/n"), Some("0"), None), None);
        assert_eq!(on(None, Some("120000000"), None), None);
    }

    #[test]
    fn a_datagram_reaches_a_path_and_an_abstract_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        send(path.to_str().unwrap(), b"WATCHDOG=1").unwrap();
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");

        use std::os::linux::net::SocketAddrExt;
        let name = format!("ferrule-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let rx = UnixDatagram::bind_addr(&addr).unwrap();
        send(&format!("@{name}"), b"READY=1").unwrap();
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }
}
