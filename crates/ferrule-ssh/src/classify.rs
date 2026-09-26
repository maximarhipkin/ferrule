//! What went wrong, from `ssh`'s exit code 255 and its stderr
//! (docs/m34-ssh-local.md §3). The strings are OpenSSH's own, stable
//! since 7.x; `LogLevel=ERROR` keeps everything else out.

use crate::target::Target;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No known_hosts entry. Never retried, never trusted silently.
    UnknownHost,
    /// The host's key isn't the one on file. A hard stop for the life of
    /// the process: it may be an attack.
    HostKeyChanged { fingerprint: Option<String> },
    /// The server refused every key tried.
    Auth,
    /// No connection (refused, timed out, name not found, dropped before
    /// the command started). Retried with backoff.
    Unreachable(String),
    /// A `-R` port was taken, or forwarding is refused.
    Forward,
    /// Anything else ssh said.
    Other(String),
}

/// Classify a failed `ssh`'s stderr.
pub fn classify(stderr: &str) -> Failure {
    let s = stderr;
    if s.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || s.contains("has changed and you have requested strict checking")
    {
        let fingerprint = s
            .split_whitespace()
            .find(|w| w.starts_with("SHA256:") || w.starts_with("MD5:"))
            .map(|w| w.trim_end_matches('.').to_string());
        return Failure::HostKeyChanged { fingerprint };
    }
    if s.contains("host key is known for") || s.contains("Host key verification failed") {
        return Failure::UnknownHost;
    }
    if s.contains("Permission denied (") || s.contains("Too many authentication failures") {
        return Failure::Auth;
    }
    if s.contains("remote port forwarding failed") || s.contains("forwarding request failed") {
        return Failure::Forward;
    }
    const DOWN: &[&str] = &[
        "Connection refused",
        "timed out",
        "Could not resolve hostname",
        "No route to host",
        "Network is unreachable",
        "Connection closed by",
        "Connection reset",
        "Broken pipe",
        "kex_exchange_identification",
        "Connection to ",
        "banner exchange",
        "mux_client",
        "Control socket",
    ];
    let first = first_line(s);
    if DOWN.iter().any(|d| s.contains(d)) {
        return Failure::Unreachable(first);
    }
    Failure::Other(first)
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('@'))
        .unwrap_or("ssh failed without saying why")
        .to_string()
}

impl Failure {
    /// What the owner (and the model) read: what happened, and what to do.
    pub fn message(&self, target: &Target) -> String {
        let who = target.describe();
        let label = &target.label;
        match self {
            Failure::UnknownHost => format!(
                "{label}: the host key of {} isn't known, so ferrule won't connect. \
                 Check its fingerprint and trust it with `ferrule ssh trust {label}` (or `ferrule setup`); \
                 ferrule never accepts a new host key by itself.",
                target.host
            ),
            Failure::HostKeyChanged { fingerprint } => format!(
                "{label}: STOPPED — the host key of {} has CHANGED{}. This can be a machine-in-the-middle attack, \
                 or the server was reinstalled. ferrule won't connect to it again in this process. \
                 Confirm the new key with the server's admin, then remove the old entry \
                 (`ssh-keygen -R {}`, or the line in ferrule's own known_hosts) and run `ferrule ssh trust {label}`.",
                target.host,
                fingerprint
                    .as_deref()
                    .map(|f| format!(" (it now presents {f})"))
                    .unwrap_or_default(),
                known_hosts_name(target),
            ),
            Failure::Auth => format!(
                "{label}: {who} refused authentication. ferrule runs ssh with BatchMode, so it can't ask for a passphrase: \
                 {}. Test with `ferrule ssh test {label}`.",
                match &target.identity_file {
                    Some(key) => format!(
                        "it offered only {} — add the key to the server's authorized_keys, and if it has a passphrase load it with `ssh-add {}`",
                        key.display(),
                        key.display()
                    ),
                    None => "load a key into ssh-agent (`ssh-add`), or name one with `identity_file` in [ssh.<name>]".into(),
                }
            ),
            Failure::Unreachable(why) => format!("{label}: can't reach {who}: {why}"),
            Failure::Forward => format!(
                "{label}: the server refused the port forward for the credential proxy"
            ),
            Failure::Other(why) => format!("{label}: ssh failed: {why}"),
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Failure::Unreachable(_))
    }

    /// A short name, for `/status` and doctor.
    pub fn kind(&self) -> &'static str {
        match self {
            Failure::UnknownHost => "unknown host key",
            Failure::HostKeyChanged { .. } => "HOST KEY CHANGED",
            Failure::Auth => "auth failed",
            Failure::Unreachable(_) => "unreachable",
            Failure::Forward => "forwarding refused",
            Failure::Other(_) => "ssh error",
        }
    }
}

/// The host as known_hosts spells it: `[host]:port` off port 22.
pub fn known_hosts_name(target: &Target) -> String {
    match target.port {
        Some(p) if p != 22 => format!("[{}]:{p}", target.host),
        _ => target.host.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_messages_are_classified() {
        let unknown = "No ED25519 host key is known for [127.0.0.1]:2299 and you have requested strict checking.\nHost key verification failed.\n";
        assert_eq!(classify(unknown), Failure::UnknownHost);
        let changed = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
            @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
            @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
            IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
            The fingerprint for the ED25519 key sent by the remote host is\n\
            SHA256:q2Vw0yQyqk3n1bI4xA0Z7mW9tT0cJmQ4b7s2m1QqN1c.\n\
            Host key for [127.0.0.1]:2299 has changed and you have requested strict checking.\n\
            Host key verification failed.\n";
        assert_eq!(
            classify(changed),
            Failure::HostKeyChanged {
                fingerprint: Some("SHA256:q2Vw0yQyqk3n1bI4xA0Z7mW9tT0cJmQ4b7s2m1QqN1c".into())
            }
        );
        assert_eq!(
            classify("node@127.0.0.1: Permission denied (publickey).\n"),
            Failure::Auth
        );
        assert!(matches!(
            classify("ssh: connect to host 127.0.0.1 port 1: Connection refused\n"),
            Failure::Unreachable(m) if m.contains("Connection refused")
        ));
        assert!(classify(
            "ssh: Could not resolve hostname nope.invalid: Name or service not known"
        )
        .retryable());
        assert_eq!(
            classify("Error: remote port forwarding failed for listen port 40000\n"),
            Failure::Forward
        );
        assert!(matches!(classify("something new"), Failure::Other(_)));
    }

    #[test]
    fn messages_say_what_to_do() {
        let t = crate::Target::parse("ssh://me@h:2200/srv", &Default::default()).unwrap();
        assert!(Failure::UnknownHost
            .message(&t)
            .contains("ferrule ssh trust ssh://me@h:2200/srv"));
        let m = Failure::HostKeyChanged {
            fingerprint: Some("SHA256:x".into()),
        }
        .message(&t);
        assert!(
            m.contains("CHANGED") && m.contains("SHA256:x") && m.contains("[h]:2200"),
            "{m}"
        );
        assert!(Failure::Auth.message(&t).contains("ssh-add"));
    }
}
