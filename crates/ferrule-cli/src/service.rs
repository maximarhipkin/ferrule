//! The gateway as a background service that starts at login: a systemd
//! user unit on Linux, a launchd agent on macOS. It runs
//! `ferrule gateway --workspace <dir>` with `FERRULE_CONFIG` pinned to the
//! config `ferrule setup` wrote and the PATH setup ran with, so the agent's
//! commands find the same tools. No keys go in the unit: the gateway reads
//! the secrets file itself.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SYSTEMD_UNIT: &str = "ferrule.service";
pub const LAUNCHD_LABEL: &str = "ai.ferrule.gateway";

/// What to run, with every path absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub exe: PathBuf,
    pub workspace: PathBuf,
    pub config: PathBuf,
    pub path_env: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No service manager this knows how to drive (or none running).
    Unsupported(String),
    NotInstalled,
    Installed {
        running: bool,
        unit: PathBuf,
    },
}

/// Where the unit file goes.
pub fn unit_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"))
    } else {
        dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user")
            .join(SYSTEMD_UNIT)
    })
}

/// Where launchd sends the gateway's output; systemd has the journal.
pub fn log_path() -> Option<PathBuf> {
    cfg!(target_os = "macos")
        .then(dirs::home_dir)
        .flatten()
        .map(|home| home.join("Library/Logs/ferrule/gateway.log"))
}

/// How to read the gateway's logs, for messages.
pub fn logs_hint() -> String {
    match log_path() {
        Some(path) => format!("tail -f {}", path.display()),
        None => "journalctl --user -u ferrule -f".into(),
    }
}

pub fn status() -> Status {
    let unit = match unit_path() {
        Ok(unit) => unit,
        Err(e) => return Status::Unsupported(e.to_string()),
    };
    if cfg!(target_os = "macos") {
        if !unit.exists() {
            return Status::NotInstalled;
        }
        let running = launchctl(&["print", &launchd_target()])
            .map(|out| out.contains("state = running"))
            .unwrap_or(false);
        return Status::Installed { running, unit };
    }
    if !cfg!(target_os = "linux") {
        return Status::Unsupported(
            "background services are set up on Linux and macOS only for now".into(),
        );
    }
    if let Err(why) = systemd_available() {
        return Status::Unsupported(why);
    }
    if !unit.exists() {
        return Status::NotInstalled;
    }
    let running = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT])
        .status()
        .is_ok_and(|s| s.success());
    Status::Installed { running, unit }
}

/// The config and workspace an installed unit pins — for `ferrule doctor`
/// to compare with what this shell sees, and for setup's defaults.
pub fn installed() -> Option<(PathBuf, PathBuf)> {
    parse_unit(&std::fs::read_to_string(unit_path().ok()?).ok()?)
}

fn parse_unit(text: &str) -> Option<(PathBuf, PathBuf)> {
    if text.contains("<plist") {
        let value = |key: &str| {
            let after = &text[text.find(&format!("<key>{key}</key>"))?..];
            let value = after.split_once("<string>")?.1.split_once("</string>")?.0;
            Some(PathBuf::from(xml_unescape(value)))
        };
        return Some((value("FERRULE_CONFIG")?, value("WorkingDirectory")?));
    }
    let value = |prefix: &str| {
        text.lines().find_map(|line| {
            let value = line.strip_prefix(prefix)?.strip_suffix('"')?;
            Some(PathBuf::from(systemd_unescape(value)))
        })
    };
    let workspace = text
        .lines()
        .find_map(|line| line.strip_prefix("WorkingDirectory="))?;
    Some((
        value("Environment=\"FERRULE_CONFIG=")?,
        PathBuf::from(workspace.replace("%%", "%")),
    ))
}

/// Write the unit, then enable and start it (restarting it if it was
/// already running, so a changed unit takes effect).
pub fn install(spec: &Spec) -> Result<Vec<String>> {
    let unit = unit_path()?;
    let dir = unit.parent().context("unit path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut notes = Vec::new();
    if cfg!(target_os = "macos") {
        let log = log_path().context("no home dir for the log")?;
        std::fs::create_dir_all(log.parent().context("log path has no parent")?)?;
        std::fs::write(&unit, launchd_plist(spec, &log))
            .with_context(|| format!("writing {}", unit.display()))?;
        // bootstrap refuses a label that's already loaded; bootout first.
        let _ = launchctl(&["bootout", &launchd_target()]);
        launchctl(&[
            "bootstrap",
            &format!("gui/{}", uid()),
            &unit.to_string_lossy(),
        ])?;
        return Ok(notes);
    }
    if !cfg!(target_os = "linux") {
        bail!("background services are set up on Linux and macOS only");
    }
    systemd_available().map_err(|why| anyhow!(why))?;
    let paths = [&spec.exe, &spec.workspace, &spec.config];
    if paths
        .iter()
        .any(|p| p.to_string_lossy().contains(['\n', '\r']))
    {
        bail!("a path with a line break can't go in a systemd unit");
    }
    std::fs::write(&unit, systemd_unit(spec))
        .with_context(|| format!("writing {}", unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SYSTEMD_UNIT])?;
    systemctl(&["restart", SYSTEMD_UNIT])?;
    // Without lingering, user services stop at logout and don't start at
    // boot — which is the whole point on a server.
    if !lingering() {
        let user = std::env::var("USER").unwrap_or_default();
        let ok = Command::new("loginctl")
            .args(["enable-linger", &user])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            notes.push(format!(
                "couldn't turn on lingering, so the service stops when you log out. \
                 To keep it running: sudo loginctl enable-linger {user}"
            ));
        }
    }
    Ok(notes)
}

pub fn restart() -> Result<()> {
    if cfg!(target_os = "macos") {
        launchctl(&["kickstart", "-k", &launchd_target()])?;
        Ok(())
    } else {
        systemctl(&["restart", SYSTEMD_UNIT])
    }
}

/// Stop, disable and delete the unit.
pub fn uninstall() -> Result<()> {
    let unit = unit_path()?;
    if cfg!(target_os = "macos") {
        let _ = launchctl(&["bootout", &launchd_target()]);
    } else {
        let _ = systemctl(&["disable", "--now", SYSTEMD_UNIT]);
    }
    match std::fs::remove_file(&unit) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", unit.display()))
        }
        _ => {}
    }
    if !cfg!(target_os = "macos") {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(())
}

pub fn systemd_unit(spec: &Spec) -> String {
    let arg = |p: &Path| systemd_quote(&p.to_string_lossy(), true);
    let env = |name: &str, value: &str| systemd_quote(&format!("{name}={value}"), false);
    format!(
        "# Written by `ferrule setup` — re-run it to change this service.\n\
         [Unit]\n\
         Description=ferrule gateway\n\
         \n\
         [Service]\n\
         ExecStart={} gateway --workspace {}\n\
         WorkingDirectory={}\n\
         Environment={}\n\
         Environment={}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        arg(&spec.exe),
        arg(&spec.workspace),
        // A bare path: this setting takes no quotes, only specifiers.
        spec.workspace.to_string_lossy().replace('%', "%%"),
        env("FERRULE_CONFIG", &spec.config.to_string_lossy()),
        env("PATH", &spec.path_env),
    )
}

pub fn launchd_plist(spec: &Spec, log: &Path) -> String {
    let s = |text: &str| format!("<string>{}</string>", xml_escape(text));
    let p = |path: &Path| s(&path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `ferrule setup` — re-run it to change this service. -->
<plist version="1.0">
<dict>
  <key>Label</key>{label}
  <key>ProgramArguments</key>
  <array>{exe}{gateway}{flag}{ws}</array>
  <key>WorkingDirectory</key>{ws}
  <key>EnvironmentVariables</key>
  <dict>
    <key>FERRULE_CONFIG</key>{config}
    <key>PATH</key>{path}
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key>{log}
  <key>StandardErrorPath</key>{log}
</dict>
</plist>
"#,
        label = s(LAUNCHD_LABEL),
        exe = p(&spec.exe),
        gateway = s("gateway"),
        flag = s("--workspace"),
        ws = p(&spec.workspace),
        config = p(&spec.config),
        path = s(&spec.path_env),
        log = p(log),
    )
}

/// A double-quoted systemd word: `\` and `"` escaped, `%` (specifiers)
/// doubled, and `$` too where variables expand (`ExecStart=`, not
/// `Environment=`).
fn systemd_quote(text: &str, dollar: bool) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            '$' if dollar => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Undo `systemd_quote(_, false)`, quotes already stripped.
fn systemd_unescape(text: &str) -> String {
    let text = text.replace("%%", "%");
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

/// Is there a systemd user manager to talk to? Containers, WSL without
/// systemd and plain SSH sessions without a user bus often have none.
fn systemd_available() -> std::result::Result<(), String> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map_err(|_| "systemctl isn't installed".to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "no systemd user session ({})",
            err.lines()
                .next()
                .unwrap_or("systemctl --user failed")
                .trim()
        ))
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    if !out.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn lingering() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "yes")
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid can't fail and touches no memory.
    unsafe { libc::getuid() }
}

/// launchd only, so never reached here.
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn launchd_target() -> String {
    format!("gui/{}/{LAUNCHD_LABEL}", uid())
}

fn launchctl(args: &[&str]) -> Result<String> {
    let out = Command::new("launchctl").args(args).output()?;
    if !out.status.success() {
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: "/home/a b/.local/bin/ferrule".into(),
            workspace: "/home/a b/ws $1 50%".into(),
            config: "/home/a b/.config/ferrule/100%\"x\".toml".into(),
            path_env: "/home/a b/.local/bin:/usr/bin:$HOME/bin".into(),
        }
    }

    #[test]
    fn systemd_unit_quotes_every_path_and_pins_the_config() {
        let unit = systemd_unit(&spec());
        assert!(unit.contains(
            "ExecStart=\"/home/a b/.local/bin/ferrule\" gateway --workspace \"/home/a b/ws $$1 50%%\"\n"
        ));
        assert!(unit.contains("WorkingDirectory=/home/a b/ws $1 50%%\n"));
        assert!(unit.contains("Environment=\"PATH=/home/a b/.local/bin:/usr/bin:$HOME/bin\"\n"));
        assert!(unit.contains("Restart=always\n") && unit.contains("WantedBy=default.target\n"));
        let line = unit.lines().find(|l| l.contains("FERRULE_CONFIG")).unwrap();
        assert_eq!(
            line,
            "Environment=\"FERRULE_CONFIG=/home/a b/.config/ferrule/100%%\\\"x\\\".toml\""
        );
        assert_eq!(parse_unit(&unit), Some((spec().config, spec().workspace)));
    }

    #[test]
    fn launchd_plist_escapes_xml_and_keeps_the_gateway_alive() {
        let mut spec = spec();
        spec.workspace = "/Users/a/<ws> & co".into();
        let plist = launchd_plist(
            &spec,
            Path::new("/Users/a/Library/Logs/ferrule/gateway.log"),
        );
        assert!(plist.contains("<string>/Users/a/&lt;ws&gt; &amp; co</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains(
            "<array><string>/home/a b/.local/bin/ferrule</string><string>gateway</string>\
             <string>--workspace</string><string>/Users/a/&lt;ws&gt; &amp; co</string></array>"
        ));
        assert!(!plist.contains("<ws>"));
        assert_eq!(
            parse_unit(&plist),
            Some((spec.config.clone(), spec.workspace.clone()))
        );
    }
}
