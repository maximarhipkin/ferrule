//! M38: `install.sh`'s upgrade restarts every instance running the new
//! binary: the default one and each `ferrule@<name>` /
//! `ai.ferrule.gateway.<name>`. Everything it calls outside the shell is a
//! stub on PATH (curl serves a fake release, uname picks the OS, systemctl
//! and launchctl log what they're asked), so nothing touches the network or
//! the machine's services.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Machine {
    /// A fake release for `os` ("Linux" or "Darwin"), with `active` the
    /// services that report running.
    fn new(os: &str, active: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stubs = root.join("stubs");
        let release = root.join("release");
        for d in [&stubs, &release, &root.join("home")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let log = root.join("log");
        // The binary in the release: it logs every call and fails a refresh
        // for the instance named in FAIL_REFRESH.
        script(
            &release.join("ferrule"),
            &format!(
                r#"case "$*" in
  --version) echo "ferrule 9.9.9"; exit 0 ;;
  "config path") echo "config  /somewhere/config.toml"; exit 0 ;;
esac
echo "ferrule $*" >> "{log}"
case "$*" in "--instance ${{FAIL_REFRESH:-none}} "*) exit 1 ;; esac
exit 0
"#,
                log = log.display()
            ),
        );
        let tar = Command::new("tar")
            .args(["-czf", "ferrule.tar.gz", "ferrule"])
            .current_dir(&release)
            .status()
            .unwrap();
        assert!(tar.success());
        let bytes = std::fs::read(release.join("ferrule.tar.gz")).unwrap();
        let sum = sha256(&bytes);
        std::fs::write(release.join("ferrule.tar.gz.sha256"), format!("{sum}  x\n")).unwrap();

        script(
            &stubs.join("curl"),
            &format!(
                r#"for a; do out=$url; url=$a; done
while [ $# -gt 0 ]; do [ "$1" = -o ] && out=$2; shift; done
case $url in
  *.sha256) cp "{r}/ferrule.tar.gz.sha256" "$out" ;;
  *) cp "{r}/ferrule.tar.gz" "$out" ;;
esac
"#,
                r = release.display()
            ),
        );
        script(
            &stubs.join("uname"),
            &format!("case $1 in -m) echo x86_64 ;; *) echo {os} ;; esac\n"),
        );
        script(&stubs.join("id"), "echo 1000\n");
        let active = active.join(" ");
        script(
            &stubs.join("systemctl"),
            &format!(
                r#"echo "systemctl $*" >> "{log}"
case "$*" in *is-active*) for u in {active}; do [ "$u" = "$4" ] && exit 0; done; exit 3 ;; esac
exit 0
"#,
                log = log.display()
            ),
        );
        script(
            &stubs.join("launchctl"),
            &format!(
                r#"echo "launchctl $*" >> "{log}"
case $1 in print) for u in {active}; do [ "gui/1000/$u" = "$2" ] && exit 0; done; exit 113 ;; esac
exit 0
"#,
                log = log.display()
            ),
        );
        std::fs::write(&log, "").unwrap();
        Machine { _dir: dir, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin/ferrule")
    }

    /// A unit file under the home; `ours` says whether it runs the new
    /// binary.
    fn unit(&self, rel: &str, ours: bool) {
        let path = self.home().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let exe = if ours {
            self.bin()
        } else {
            PathBuf::from("/opt/elsewhere/ferrule")
        };
        std::fs::write(path, format!("ExecStart={} gateway\n", exe.display())).unwrap();
    }

    fn install(&self, env: &[(&str, &str)]) -> (String, String) {
        let path = format!(
            "{}:{}",
            self.root.join("stubs").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("sh");
        cmd.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install.sh"))
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.home())
            .env("FERRULE_INSTALL_DIR", self.root.join("bin"))
            .env("FERRULE_NO_SETUP", "1")
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{text}");
        let log = std::fs::read_to_string(self.root.join("log")).unwrap();
        (text, log)
    }
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_linux_upgrade_refreshes_every_running_instance_of_this_binary() {
    let m = Machine::new(
        "Linux",
        &[
            "ferrule.service",
            "ferrule@work.service",
            "ferrule@old.service",
            "ferrule@shaky.service",
        ],
    );
    let units = ".config/systemd/user";
    m.unit(&format!("{units}/ferrule.service"), true);
    m.unit(&format!("{units}/ferrule@work.service"), true);
    m.unit(&format!("{units}/ferrule@shaky.service"), true);
    m.unit(&format!("{units}/ferrule@idle.service"), true); // installed, not running
    m.unit(&format!("{units}/ferrule@old.service"), false); // another binary
    m.unit(&format!("{units}/ferrule-update@work.service"), true); // not a gateway

    let (out, log) = m.install(&[("FAIL_REFRESH", "shaky")]);
    let calls: Vec<&str> = log.lines().filter(|l| !l.contains("is-active")).collect();
    assert!(calls.contains(&"ferrule setup --refresh-service"), "{log}");
    assert!(
        calls.contains(&"ferrule --instance work setup --refresh-service"),
        "{log}"
    );
    // A refresh that fails falls back to a plain restart of that one.
    assert!(
        calls.contains(&"ferrule --instance shaky setup --refresh-service"),
        "{log}"
    );
    assert!(
        calls.contains(&"systemctl --user restart ferrule@shaky.service"),
        "{log}"
    );
    assert!(
        out.contains("Restarted the `shaky` instance's gateway service."),
        "{out}"
    );
    // One on another binary is restarted as it is, with a note naming it.
    assert!(
        calls.contains(&"systemctl --user restart ferrule@old.service"),
        "{log}"
    );
    assert!(
        out.contains("`ferrule --instance old setup` → service"),
        "{out}"
    );
    // Nothing for the one not running, or for an update unit.
    assert!(
        !log.contains("idle setup") && !log.contains("restart ferrule@idle"),
        "{log}"
    );
    assert!(!log.contains("ferrule-update"), "{log}");
    assert_eq!(calls.len(), 5, "{log}");
}

#[test]
fn a_mac_upgrade_refreshes_every_running_instance_of_this_binary() {
    let m = Machine::new("Darwin", &["ai.ferrule.gateway", "ai.ferrule.gateway.work"]);
    let agents = "Library/LaunchAgents";
    m.unit(&format!("{agents}/ai.ferrule.gateway.plist"), true);
    m.unit(&format!("{agents}/ai.ferrule.gateway.work.plist"), true);
    m.unit(&format!("{agents}/ai.ferrule.gateway.idle.plist"), true);
    m.unit(&format!("{agents}/ai.ferrule.update.work.plist"), true);

    let (_, log) = m.install(&[]);
    let calls: Vec<&str> = log
        .lines()
        .filter(|l| !l.starts_with("launchctl print"))
        .collect();
    assert_eq!(
        calls,
        [
            "ferrule setup --refresh-service",
            "ferrule --instance work setup --refresh-service"
        ],
        "{log}"
    );
    assert!(
        log.contains("launchctl print gui/1000/ai.ferrule.gateway.idle"),
        "{log}"
    );
}

#[test]
fn a_first_install_with_no_service_restarts_nothing() {
    let m = Machine::new("Linux", &[]);
    let (out, log) = m.install(&[]);
    assert!(out.contains("Installed ferrule 9.9.9"), "{out}");
    assert_eq!(log, "", "{out}");
}
