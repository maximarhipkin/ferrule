//! The gateway as a background service that starts at login: a systemd
//! user unit on Linux, a launchd agent on macOS. It runs
//! `ferrule gateway --workspace <dir>` with `FERRULE_CONFIG` pinned to the
//! config `ferrule setup` wrote and the PATH setup ran with, so the agent's
//! commands find the same tools. No keys go in the unit: the gateway reads
//! the secrets file itself.
//!
//! Set up as root on Linux, it's a system unit instead, run as a `ferrule`
//! system user (no login, no sudo) that owns only its data dir and
//! workspace, under systemd's own hardening — a wall under the sandbox,
//! since otherwise every command the agent runs would start as root.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub const SYSTEMD_UNIT: &str = "ferrule.service";
pub const LAUNCHD_LABEL: &str = "ai.ferrule.gateway";

/// The system service's account and where its files live: config root-owned
/// and read-only to it, data and workspace its own, none under /home (which
/// `ProtectHome=` hides from it).
pub const SYSTEM_USER: &str = "ferrule";
pub const SYSTEM_CONFIG: &str = "/etc/ferrule/config.toml";
pub const SYSTEM_HOME: &str = "/var/lib/ferrule";
pub const SYSTEM_DATA: &str = "/var/lib/ferrule/data";
pub const SYSTEM_WORKSPACE: &str = "/var/lib/ferrule/workspace";
const SYSTEM_UNIT_PATH: &str = "/etc/systemd/system/ferrule.service";

/// Whose service this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A systemd user unit or launchd agent, run as whoever set it up.
    User,
    /// A systemd system unit run as [`SYSTEM_USER`]. Linux, as root.
    System,
}

/// `ferrule setup --system` / `--user`, and the default: a system service
/// when root on Linux, since a user unit there would run the agent as root.
pub fn decide_scope(
    linux: bool,
    root: bool,
    system: bool,
    user: bool,
) -> std::result::Result<Scope, String> {
    match (system, user) {
        (true, true) => Err("--system and --user exclude each other".into()),
        (true, false) if !linux => {
            Err("--system is for Linux; elsewhere the service runs as you".into())
        }
        (true, false) if !root => Err(
            "--system creates a system user and a system unit, so it needs root: \
             sudo ferrule setup --system"
                .into(),
        ),
        (true, false) => Ok(Scope::System),
        (false, true) => Ok(Scope::User),
        (false, false) if linux && root => Ok(Scope::System),
        (false, false) => Ok(Scope::User),
    }
}

static SCOPE: OnceLock<Scope> = OnceLock::new();

/// Fix the scope for this process; `ferrule setup`'s flags do, before
/// anything asks.
pub fn set_scope(scope: Scope) {
    let _ = SCOPE.set(scope);
}

pub fn scope() -> Scope {
    *SCOPE.get_or_init(|| {
        decide_scope(cfg!(target_os = "linux"), is_root(), false, false).unwrap_or(Scope::User)
    })
}

#[cfg(unix)]
pub fn is_root() -> bool {
    // SAFETY: geteuid can't fail and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}

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
    if scope() == Scope::System {
        return Ok(PathBuf::from(SYSTEM_UNIT_PATH));
    }
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
        None if scope() == Scope::System => "journalctl -u ferrule -f".into(),
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
        .args(scope_flag())
        .args(["is-active", "--quiet", SYSTEMD_UNIT])
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
    if scope() == Scope::System {
        return install_system(spec, &crate::config::data_dir()?);
    }
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

/// The system unit: create the account if needed, hand it its data dir and
/// workspace, and let it read (only read) the config.
fn install_system(spec: &Spec, data: &Path) -> Result<Vec<String>> {
    let problems = system_problems(spec, data);
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    let created = ensure_system_user()?;
    std::fs::create_dir_all(data)?;
    own_system_files(Some(&spec.workspace))?;
    std::fs::write(SYSTEM_UNIT_PATH, system_unit(spec, data))
        .with_context(|| format!("writing {SYSTEM_UNIT_PATH}"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SYSTEMD_UNIT])?;
    systemctl(&["restart", SYSTEMD_UNIT])?;
    let mut notes = Vec::new();
    if created {
        notes.push(format!(
            "created the system user `{SYSTEM_USER}` (no login, no sudo); it owns {} and {}",
            data.display(),
            spec.workspace.display()
        ));
    }
    Ok(notes)
}

/// What would stop the system unit from working: `ProtectHome=` hides
/// /home, /root and /run/user from the service, and a workspace that is a
/// system directory would be handed to the service's user.
pub fn system_problems(spec: &Spec, data: &Path) -> Vec<String> {
    let hidden = |p: &Path| {
        ["/home", "/root", "/run/user"]
            .iter()
            .any(|h| p.starts_with(h))
    };
    let mut problems = Vec::new();
    for (what, path) in [
        ("the ferrule binary", spec.exe.as_path()),
        ("the config", spec.config.as_path()),
        ("the data dir", data),
        ("the workspace", spec.workspace.as_path()),
    ] {
        if path.to_string_lossy().contains(['\n', '\r']) {
            problems.push(format!("{what} has a line break in its path"));
        } else if hidden(path) {
            let fix = if what == "the ferrule binary" {
                " (install it system-wide: FERRULE_INSTALL_DIR=/usr/local/bin, or copy it there)"
            } else {
                ""
            };
            problems.push(format!(
                "{what} is in {}, which the system service can't see{fix}",
                path.display()
            ));
        }
    }
    let system_dirs = [
        "/", "/bin", "/boot", "/dev", "/etc", "/lib", "/proc", "/sbin", "/sys", "/usr", "/var",
    ];
    if system_dirs.iter().any(|d| spec.workspace == Path::new(d))
        || ["/etc", "/usr", "/boot", "/proc", "/sys", "/dev"]
            .iter()
            .any(|d| spec.workspace.starts_with(d))
    {
        problems.push(format!(
            "{} is a system directory; the service's user would own it",
            spec.workspace.display()
        ));
    }
    problems
}

/// Create [`SYSTEM_USER`] if it doesn't exist; `true` if it was created.
/// An existing one must be a system account nobody can log in as, outside
/// every admin group.
fn ensure_system_user() -> Result<bool> {
    let out = Command::new("getent")
        .args(["passwd", SYSTEM_USER])
        .output()
        .context("running getent")?;
    if out.status.success() {
        let groups = Command::new("id")
            .args(["-nG", SYSTEM_USER])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        check_existing_user(&String::from_utf8_lossy(&out.stdout), &groups)
            .map_err(|why| anyhow!(why))?;
        return Ok(false);
    }
    let shell = ["/usr/sbin/nologin", "/sbin/nologin"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("/bin/false");
    run(Command::new("useradd").args([
        "--system",
        "--user-group",
        "--home-dir",
        SYSTEM_HOME,
        "--no-create-home",
        "--shell",
        shell,
        "--comment",
        "ferrule agent",
        SYSTEM_USER,
    ]))?;
    Ok(true)
}

/// The passwd line and group list of an existing [`SYSTEM_USER`]: fine
/// only if it's no one's login and holds no admin rights.
pub fn check_existing_user(passwd: &str, groups: &str) -> std::result::Result<(), String> {
    let fields: Vec<&str> = passwd.trim().split(':').collect();
    let (Some(uid), Some(shell)) = (fields.get(2), fields.get(6)) else {
        return Err(format!(
            "can't read the `{SYSTEM_USER}` account: {passwd:?}"
        ));
    };
    if *uid == "0" {
        return Err(format!("the existing `{SYSTEM_USER}` account is uid 0"));
    }
    if !(shell.ends_with("/nologin") || shell.ends_with("/false")) {
        return Err(format!(
            "an account `{SYSTEM_USER}` already exists and can log in ({shell}); \
             it isn't safe to run the service as it"
        ));
    }
    let admin = ["root", "sudo", "wheel", "admin", "adm", "docker", "lxd"];
    if let Some(g) = groups.split_whitespace().find(|g| admin.contains(g)) {
        return Err(format!(
            "the existing `{SYSTEM_USER}` account is in the `{g}` group; remove it from there first"
        ));
    }
    Ok(())
}

/// After root wrote them: the service's user owns its home, data dir and
/// workspace; the config stays root's, readable by its group. A no-op while
/// the account doesn't exist yet.
pub fn own_system_files(workspace: Option<&Path>) -> Result<()> {
    let exists = Command::new("getent")
        .args(["passwd", SYSTEM_USER])
        .output()
        .is_ok_and(|o| o.status.success());
    if !exists {
        return Ok(());
    }
    let owner = format!("{SYSTEM_USER}:{SYSTEM_USER}");
    std::fs::create_dir_all(SYSTEM_HOME)?;
    run(Command::new("chown").args([&owner, SYSTEM_HOME]))?;
    run(Command::new("chmod").args(["750", SYSTEM_HOME]))?;
    let data = crate::config::data_dir()?;
    let mut mine = vec![data.as_path()];
    mine.extend(workspace);
    for dir in mine {
        run(Command::new("chown").arg("-R").arg(&owner).arg(dir))?;
    }
    let config = Path::new(SYSTEM_CONFIG);
    let group = format!("root:{SYSTEM_USER}");
    if let Some(dir) = config.parent().filter(|d| d.exists()) {
        run(Command::new("chown").arg(&group).arg(dir))?;
        run(Command::new("chmod").arg("750").arg(dir))?;
    }
    if config.exists() {
        run(Command::new("chown").arg(&group).arg(config))?;
        run(Command::new("chmod").arg("640").arg(config))?;
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let what = format!("{cmd:?}");
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn system_unit(spec: &Spec, data: &Path) -> String {
    let arg = |p: &Path| systemd_quote(&p.to_string_lossy(), true);
    let env = |name: &str, value: &str| systemd_quote(&format!("{name}={value}"), false);
    let path = |p: &Path| systemd_quote(&p.to_string_lossy(), false);
    format!(
        "# Written by `ferrule setup` as root — re-run it to change this service.\n\
         [Unit]\n\
         Description=ferrule gateway\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         User={SYSTEM_USER}\n\
         Group={SYSTEM_USER}\n\
         ExecStart={} gateway --workspace {}\n\
         WorkingDirectory={}\n\
         Environment={}\n\
         Environment={}\n\
         Environment={}\n\
         NoNewPrivileges=yes\n\
         ProtectSystem=strict\n\
         ProtectHome=yes\n\
         PrivateTmp=yes\n\
         ReadWritePaths={} {}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        arg(&spec.exe),
        arg(&spec.workspace),
        spec.workspace.to_string_lossy().replace('%', "%%"),
        env("FERRULE_CONFIG", &spec.config.to_string_lossy()),
        env("FERRULE_DATA_DIR", &data.to_string_lossy()),
        env("PATH", &spec.path_env),
        path(data),
        path(&spec.workspace),
    )
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
        .args(scope_flag())
        .arg("show-environment")
        .output()
        .map_err(|_| "systemctl isn't installed".to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "no systemd {} ({})",
            if scope() == Scope::System {
                "running"
            } else {
                "user session"
            },
            err.lines().next().unwrap_or("systemctl failed").trim()
        ))
    }
}

/// `--user`, except for the system unit.
fn scope_flag() -> &'static [&'static str] {
    match scope() {
        Scope::User => &["--user"],
        Scope::System => &[],
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(scope_flag())
        .args(args)
        .output()?;
    if !out.status.success() {
        bail!(
            "systemctl {} failed: {}",
            scope_flag()
                .iter()
                .chain(args)
                .copied()
                .collect::<Vec<_>>()
                .join(" "),
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
    fn root_on_linux_gets_a_system_service_unless_it_asks_otherwise() {
        use Scope::*;
        // (linux, root, --system, --user)
        assert_eq!(decide_scope(true, true, false, false), Ok(System));
        assert_eq!(decide_scope(true, true, true, false), Ok(System));
        assert_eq!(decide_scope(true, true, false, true), Ok(User));
        assert_eq!(decide_scope(true, false, false, false), Ok(User));
        assert_eq!(decide_scope(false, true, false, false), Ok(User));
        assert_eq!(decide_scope(false, false, false, false), Ok(User));
        assert!(decide_scope(true, false, true, false)
            .unwrap_err()
            .contains("sudo"));
        assert!(decide_scope(false, true, true, false).is_err());
        assert!(decide_scope(true, true, true, true).is_err());
    }

    fn system_spec() -> Spec {
        Spec {
            exe: "/usr/local/bin/ferrule".into(),
            workspace: SYSTEM_WORKSPACE.into(),
            config: SYSTEM_CONFIG.into(),
            path_env: "/usr/local/bin:/usr/bin".into(),
        }
    }

    #[test]
    fn the_system_unit_runs_as_its_own_user_behind_systemd_hardening() {
        let unit = system_unit(&system_spec(), Path::new(SYSTEM_DATA));
        let lines: Vec<&str> = unit.lines().collect();
        for line in [
            "User=ferrule",
            "Group=ferrule",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "PrivateTmp=yes",
            "ReadWritePaths=\"/var/lib/ferrule/data\" \"/var/lib/ferrule/workspace\"",
            "Environment=\"FERRULE_DATA_DIR=/var/lib/ferrule/data\"",
            "Environment=\"FERRULE_CONFIG=/etc/ferrule/config.toml\"",
            "ExecStart=\"/usr/local/bin/ferrule\" gateway --workspace \"/var/lib/ferrule/workspace\"",
            "WantedBy=multi-user.target",
            "Restart=always",
        ] {
            assert!(lines.contains(&line), "missing {line:?} in\n{unit}");
        }
        assert!(!unit.contains("default.target"));
        // doctor and setup read the pinned paths back the same way.
        assert_eq!(
            parse_unit(&unit),
            Some((SYSTEM_CONFIG.into(), SYSTEM_WORKSPACE.into()))
        );
    }

    #[test]
    fn the_system_unit_quotes_odd_paths() {
        let mut spec = system_spec();
        spec.workspace = "/srv/my ws 100%".into();
        let unit = system_unit(&spec, Path::new("/var/lib/ferrule/da ta"));
        assert!(unit.contains("ReadWritePaths=\"/var/lib/ferrule/da ta\" \"/srv/my ws 100%%\"\n"));
        assert_eq!(parse_unit(&unit).unwrap().1, spec.workspace);
    }

    #[test]
    fn a_system_service_refuses_what_it_could_not_see_or_should_not_own() {
        let data = Path::new(SYSTEM_DATA);
        assert_eq!(system_problems(&system_spec(), data), Vec::<String>::new());

        let mut spec = system_spec();
        spec.exe = "/root/.local/bin/ferrule".into();
        let problems = system_problems(&spec, data);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("/usr/local/bin"), "{problems:?}");

        for ws in [
            "/home/max/project",
            "/root/ws",
            "/",
            "/etc",
            "/usr/local/src",
            "/var",
        ] {
            let mut spec = system_spec();
            spec.workspace = ws.into();
            assert_eq!(system_problems(&spec, data).len(), 1, "{ws}");
        }
        for ws in ["/srv/agent", "/opt/work", "/var/lib/ferrule/workspace"] {
            let mut spec = system_spec();
            spec.workspace = ws.into();
            assert!(system_problems(&spec, data).is_empty(), "{ws}");
        }
        let problems = system_problems(&system_spec(), Path::new("/home/x/.local/share/ferrule"));
        assert!(problems[0].contains("data dir"), "{problems:?}");
    }

    #[test]
    fn an_existing_account_is_used_only_if_nobody_can_log_in_as_it() {
        let nologin = "ferrule:x:998:998:ferrule agent:/var/lib/ferrule:/usr/sbin/nologin\n";
        assert_eq!(check_existing_user(nologin, "ferrule\n"), Ok(()));
        assert_eq!(
            check_existing_user("ferrule:x:999:999::/var/lib/ferrule:/bin/false", "ferrule"),
            Ok(())
        );
        let bash = "ferrule:x:1001:1001::/home/ferrule:/bin/bash";
        assert!(check_existing_user(bash, "ferrule")
            .unwrap_err()
            .contains("log in"));
        assert!(check_existing_user(nologin, "ferrule sudo")
            .unwrap_err()
            .contains("`sudo`"));
        assert!(check_existing_user(nologin, "ferrule docker").is_err());
        assert!(check_existing_user("ferrule:x:0:0::/:/usr/sbin/nologin", "root").is_err());
        assert!(check_existing_user("garbage", "").is_err());
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
