//! macOS backend: a Seatbelt profile run through `/usr/bin/sandbox-exec`.
//!
//! The base, network and preferences policies are OpenAI Codex's
//! (Apache-2.0), vendored under `seatbelt/` so upstream fixes port over;
//! the assembly below follows Codex's `create_seatbelt_command_args`, minus
//! its proxy and per-path carve-out features. Profile generation compiles on
//! every platform so the tests run in Linux CI. **Not yet run on a real
//! Mac** — `ferrule sandbox` there is the check.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

/// Never resolved through PATH: a planted `sandbox-exec` would get to decide
/// what the sandbox is.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const BASE: &str = include_str!("seatbelt/base.sbpl");
const NETWORK: &str = include_str!("seatbelt/network.sbpl");
const PREFS: &str = include_str!("seatbelt/prefs.sbpl");

/// What a desktop app (Chrome) needs on top of the base profile to start at
/// all: every Mach and XPC service, IOKit, POSIX shared memory, sysctl reads
/// and process info. Measured on GitHub's macos-14 image and its Chrome — anything less
/// and Chrome aborts at startup. It's a real loosening: the process can talk
/// to the window server, the pasteboard, the keychain daemon and the rest of
/// the per-user services a normal app could. File writes and the hidden
/// paths stay exactly as confined, and this is only ever set for the
/// browser helper, never for the model's commands.
const DESKTOP: &str = "; ferrule: desktop services, for the browser helper only
(allow mach-lookup)
(allow mach-register)
(allow iokit-open)
(allow iokit-get-properties)
(allow ipc-posix-shm*)
(allow sysctl-read)
(allow process-info*)";

/// The profile text plus the `-D` parameters it refers to. Roots go in as
/// parameters rather than being spliced into the text, so a path containing
/// `"` or `)` can't rewrite the policy. Roots must already be canonical —
/// Seatbelt matches real paths (`/private/tmp`, not `/tmp`).
pub fn profile(
    network: bool,
    desktop: bool,
    writable: &[PathBuf],
    hidden: &[PathBuf],
) -> (String, Vec<(String, PathBuf)>) {
    let mut sections = vec![
        BASE.to_string(),
        "; ferrule: reads are unrestricted, the agent needs its toolchain\n(allow file-read*)"
            .to_string(),
    ];
    let mut params = Vec::new();
    if !writable.is_empty() {
        let mut filters = Vec::new();
        let mut anchors = Vec::new();
        for (i, root) in writable.iter().enumerate() {
            let key = format!("WRITABLE_ROOT_{i}");
            let matcher = if root.is_dir() { "subpath" } else { "literal" };
            filters.push(format!("  ({matcher} (param \"{key}\"))"));
            // Renaming a root away and putting something else in its place
            // would move the boundary; later rules win, so these come after.
            anchors.push(format!(
                "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
            ));
            params.push((key, root.clone()));
        }
        sections.push(format!("(allow file-write*\n{}\n)", filters.join("\n")));
        sections.extend(anchors);
    }
    if network {
        sections.push(
            "(allow network-outbound)\n(allow network-inbound)\n(allow network-bind)".to_string(),
        );
        sections.push(NETWORK.to_string());
    }
    // Only safe because reads are unrestricted anyway (see prefs.sbpl).
    sections.push(PREFS.to_string());
    sections.push("(deny mach-lookup (xpc-service-name-prefix \"\"))".to_string());
    // F_MAKECOMPRESSED (80) and F_TRANSFEREXTENTS (110) modify files through
    // read-only descriptors, bypassing file-write*.
    sections.push("(deny system-fcntl (fcntl-command 80 110))".to_string());
    if desktop {
        // After the XPC deny, so it wins. See `DESKTOP` for what this opens.
        sections.push(DESKTOP.to_string());
    }
    // Last, so they override both the blanket read grant and any writable
    // root the hidden path sits in.
    for (i, path) in hidden.iter().enumerate() {
        let key = format!("HIDDEN_{i}");
        let matcher = if path.is_dir() { "subpath" } else { "literal" };
        sections.push(format!(
            "(deny file-read* file-write* ({matcher} (param \"{key}\")))"
        ));
        params.push((key, path.clone()));
    }
    (sections.join("\n"), params)
}

/// `sandbox-exec -p <profile> -DKEY=path… -- program args…`
pub fn command<I, S>(
    network: bool,
    desktop: bool,
    writable: &[PathBuf],
    hidden: &[PathBuf],
    program: impl AsRef<OsStr>,
    args: I,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (policy, params) = profile(network, desktop, writable, hidden);
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(policy);
    for (key, path) in params {
        let mut define = OsString::from(format!("-D{key}="));
        define.push(path.as_os_str());
        cmd.arg(define);
    }
    cmd.arg("--").arg(program).args(args);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parens balance outside comments, strings and `#"regex"` literals —
    /// the cheapest check that an edit didn't break the S-expression.
    fn balanced(sbpl: &str) -> bool {
        let mut depth = 0i32;
        let mut chars = sbpl.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ';' => {
                    for c in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                '"' => {
                    while let Some(c) = chars.next() {
                        match c {
                            '\\' => {
                                chars.next();
                            }
                            '"' => break,
                            _ => {}
                        }
                    }
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0
    }

    #[test]
    fn profile_is_deny_default_with_params_for_roots() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "").unwrap();
        let (p, params) = profile(false, false, &[dir.path().to_path_buf(), file.clone()], &[]);
        assert!(p.starts_with("(version 1)"), "version must lead");
        assert!(p.contains("(deny default)"));
        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_0\"))"));
        assert!(
            p.contains("(literal (param \"WRITABLE_ROOT_1\"))"),
            "files match literally"
        );
        assert!(
            !p.contains(dir.path().to_str().unwrap()),
            "paths go in as params, not text"
        );
        assert!(!p.contains("network-outbound"));
        assert!(!p.contains("(allow mach-lookup)"), "no desktop services");
        assert_eq!(params.len(), 2);
        assert!(balanced(&p));

        let (with_net, _) = profile(true, false, &[], &[]);
        assert!(with_net.contains("(allow network-outbound)"));
        assert!(
            with_net.contains("com.apple.trustd.agent"),
            "TLS needs trustd"
        );
        assert!(
            !with_net.contains("allow file-write*"),
            "read-only: no write grant"
        );
        assert!(balanced(&with_net));
    }

    #[test]
    fn hidden_paths_are_denied_after_every_grant() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("private");
        std::fs::create_dir(&secret).unwrap();
        let (p, params) = profile(
            true,
            true,
            &[dir.path().to_path_buf()],
            std::slice::from_ref(&secret),
        );
        let deny = "(deny file-read* file-write* (subpath (param \"HIDDEN_0\")))";
        let at = p.find(deny).expect("deny rule present");
        assert!(at > p.find("(allow file-read*)").unwrap());
        assert!(at > p.find("(allow file-write*").unwrap());
        assert!(at > p.find("(allow network-outbound)").unwrap());
        assert!(at > p.find("(allow mach-lookup)").unwrap());
        // The desktop grant overrides the XPC deny, not the other way round.
        assert!(
            p.find("(allow mach-lookup)").unwrap() > p.find("xpc-service-name-prefix").unwrap()
        );
        assert!(params.contains(&("HIDDEN_0".to_string(), secret)));
        assert!(balanced(&p));
    }

    #[test]
    fn command_uses_the_absolute_sandbox_exec() {
        let cmd = command(
            true,
            false,
            &[PathBuf::from("/private/tmp")],
            &[],
            "sh",
            ["-c", "true"],
        );
        assert_eq!(cmd.get_program(), SANDBOX_EXEC);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-p");
        assert_eq!(args[2], "-DWRITABLE_ROOT_0=/private/tmp");
        assert_eq!(&args[3..], ["--", "sh", "-c", "true"]);
    }

    #[test]
    fn balance_checker_catches_breakage() {
        assert!(!balanced("(allow (x)"));
        assert!(balanced("(a \"(\" #\"^/dev/ttys[0-9]+\") ; )"));
    }
}
