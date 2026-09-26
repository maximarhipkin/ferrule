//! The Unix-socket allowlist (M33, docs/egress.md): which local sockets a
//! sandboxed command may `connect(2)` to.
//!
//! A socket is a door past every other rule. Write access to
//! `/var/run/docker.sock` is root on the host, the D-Bus session bus
//! starts programs, `systemd --user` runs units — none of which Landlock or
//! the network flag sees, because connecting to a Unix socket isn't a file
//! write and isn't the network. So commands get the sockets ordinary tools
//! need (the SSH and GPG agents, name service, journald, local databases)
//! plus the ones they made themselves, and nothing else. "Themselves" is
//! checked, not assumed from where the socket lives: `/tmp` also holds the
//! owner's tmux server, an editor's server socket and VS Code's IPC, each a
//! way to run anything outside the sandbox. Enforced by a seccomp supervisor on Linux ([`crate::linux`]) and by
//! Seatbelt rules on macOS; Windows has no equivalent here (named pipes are
//! a DACL question, not a path one).

use std::path::{Path, PathBuf};

/// The entries that apply when `unix_sockets_default` is on, unexpanded:
/// `~/` is the home dir, `$VAR` an environment variable (skipped when
/// unset), a trailing `/` a directory, `@` an abstract socket name (Linux),
/// a trailing `*` a prefix.
pub fn default_unix_sockets() -> Vec<String> {
    let mut out: Vec<String> = vec![
        "$SSH_AUTH_SOCK".into(),
        "~/.gnupg/".into(),
        "$GNUPGHOME/".into(),
    ];
    if cfg!(target_os = "macos") {
        out.extend(
            [
                "/private/var/run/mDNSResponder",
                "/private/var/run/syslog",
                "/private/tmp/com.apple.launchd.*/",
                "/private/tmp/mysql.sock",
                "/private/tmp/.s.PGSQL.*",
            ]
            .map(String::from),
        );
    } else {
        // SAFETY: getuid never fails.
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        out.push(format!("/run/user/{uid}/gnupg/"));
        out.extend(
            [
                "/run/nscd/",
                "/var/run/nscd/",
                "/run/systemd/resolve/",
                "/run/systemd/userdb/",
                "/run/systemd/journal/",
                "/dev/log",
                "/run/postgresql/",
                "/var/run/postgresql/",
                "/run/mysqld/",
                "/var/run/mysqld/",
            ]
            .map(String::from),
        );
    }
    out
}

/// A resolved allowlist. Paths are canonical.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnixSockets {
    /// Sockets allowed by exact path.
    pub files: Vec<PathBuf>,
    /// Directories whose sockets are allowed (at any depth).
    pub dirs: Vec<PathBuf>,
    /// Abstract names without the leading NUL, and whether the entry was a
    /// prefix (`@name*`).
    pub abstract_names: Vec<(Vec<u8>, bool)>,
    /// Directories the command can write (the workspace, temp dirs): a
    /// socket there is allowed when the command's own processes listen on
    /// it — [`Verdict::IfOwn`].
    pub own_dirs: Vec<PathBuf>,
}

/// What [`UnixSockets::check_path`] says about one socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Allowed if the listening process belongs to the sandboxed command.
    IfOwn,
    Deny,
}

impl UnixSockets {
    /// Resolves `entries` (see [`default_unix_sockets`] for the syntax)
    /// and `own_dirs`. Returns `None` when an entry is `*`: every socket
    /// allowed, no supervision.
    pub fn resolve(entries: &[String], own_dirs: &[PathBuf], workspace: &Path) -> Option<Self> {
        let home = crate::home_dir();
        let mut out = Self::default();
        for entry in entries {
            let entry = entry.trim();
            if entry == "*" {
                return None;
            }
            if let Some(name) = entry.strip_prefix('@') {
                let (name, prefix) = match name.strip_suffix('*') {
                    Some(n) => (n, true),
                    None => (name, false),
                };
                out.abstract_names.push((name.as_bytes().to_vec(), prefix));
                continue;
            }
            let Some(raw) = expand_vars(entry) else {
                continue;
            };
            let dir = raw.ends_with('/');
            if raw.contains('*') {
                // `/private/tmp/com.apple.launchd.*/`: every match, now.
                out.glob(&raw, dir);
                continue;
            }
            let path = crate::expand(Path::new(&raw), home.as_deref(), workspace);
            out.add(path, dir);
        }
        out.own_dirs = own_dirs.iter().map(|d| canonical(d)).collect();
        out.own_dirs.sort();
        out.own_dirs.dedup();
        out.files.sort();
        out.files.dedup();
        out.dirs.sort();
        out.dirs.dedup();
        Some(out)
    }

    fn add(&mut self, path: PathBuf, dir: bool) {
        let real = canonical(&path);
        if dir || real.is_dir() {
            self.dirs.push(real);
        } else {
            self.files.push(real);
        }
    }

    /// One `*` in the last or second-to-last component, matched against
    /// what exists.
    fn glob(&mut self, raw: &str, dir: bool) {
        let trimmed = raw.trim_end_matches('/');
        let path = Path::new(trimmed);
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let name = name.to_string_lossy();
        let Some((pre, post)) = name.split_once('*') else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post) {
                self.add(e.path(), dir);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.files.len() + self.dirs.len() + self.abstract_names.len() + self.own_dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the socket at canonical `real` may be connected to. A socket
    /// with more than one hard link is only allowed by its exact path: a
    /// link to `docker.sock` made inside an allowed dir would otherwise
    /// ride in on the directory rule.
    pub fn check_path(&self, real: &Path, links: u64) -> Verdict {
        if self.files.iter().any(|f| f == real) {
            return Verdict::Allow;
        }
        if links > 1 {
            return Verdict::Deny;
        }
        if self.dirs.iter().any(|d| real.starts_with(d)) {
            Verdict::Allow
        } else if self.own_dirs.iter().any(|d| real.starts_with(d)) {
            Verdict::IfOwn
        } else {
            Verdict::Deny
        }
    }

    /// Whether the abstract socket `name` (no leading NUL) may be
    /// connected to.
    pub fn allows_abstract(&self, name: &[u8]) -> bool {
        self.abstract_names.iter().any(|(n, prefix)| {
            if *prefix {
                name.starts_with(n)
            } else {
                name == n.as_slice()
            }
        })
    }
}

/// `$VAR` at the start, from the environment; `None` when it's unset or
/// empty.
fn expand_vars(entry: &str) -> Option<String> {
    let Some(rest) = entry.strip_prefix('$') else {
        return Some(entry.to_string());
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let value = std::env::var(&rest[..end]).ok().filter(|v| !v.is_empty())?;
    Some(format!("{value}{}", &rest[end..]))
}

/// The real path, or for one that doesn't exist yet, its parent's real path
/// plus the name (a socket made later still matches).
fn canonical(path: &Path) -> PathBuf {
    if let Ok(p) = dunce::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => dunce::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn entries_resolve_to_files_dirs_and_abstract_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join("run")).unwrap();
        std::fs::create_dir(root.join("ws")).unwrap();
        std::os::unix::fs::symlink(root.join("run"), root.join("link")).unwrap();
        let entries: Vec<String> = [
            format!("{}/run/", root.display()),
            format!("{}/link/agent.sock", root.display()),
            "later.sock".into(),
            "@exact".into(),
            "@prefix.*".into(),
            "$FERRULE_TEST_UNSET_VAR".into(),
        ]
        .into();
        let u = UnixSockets::resolve(&entries, &[root.join("ws")], &root.join("ws")).unwrap();
        assert_eq!(u.dirs, [root.join("run")]);
        assert!(
            u.files.contains(&root.join("run/agent.sock")),
            "symlinks resolved: {u:?}"
        );
        assert!(
            u.files.contains(&root.join("ws/later.sock")),
            "relative to the workspace"
        );
        assert_eq!(u.len(), 6, "the unset variable is skipped");

        assert_eq!(u.check_path(&root.join("run/x/y.sock"), 1), Verdict::Allow);
        assert_eq!(
            u.check_path(&root.join("run/hardlinked.sock"), 2),
            Verdict::Deny
        );
        assert_eq!(
            u.check_path(&root.join("ws/later.sock"), 2),
            Verdict::Allow,
            "exact entries don't care"
        );
        assert_eq!(u.check_path(&root.join("ws/tmux.sock"), 1), Verdict::IfOwn);
        assert_eq!(
            u.check_path(Path::new("/var/run/docker.sock"), 1),
            Verdict::Deny
        );
        assert!(u.allows_abstract(b"exact"));
        assert!(!u.allows_abstract(b"exactly"));
        assert!(u.allows_abstract(b"prefix.1"));
        assert!(!u.allows_abstract(b"/tmp/dbus-abc"));

        assert!(UnixSockets::resolve(&["*".into()], &[], &root).is_none());
    }

    #[test]
    fn the_defaults_never_include_a_container_or_bus_socket() {
        let all = default_unix_sockets().join(" ");
        for bad in [
            "docker",
            "podman",
            "containerd",
            "bus",
            "systemd/private",
            "X11",
            "tmux",
        ] {
            assert!(!all.contains(bad), "{bad} in {all}");
        }
    }
}
