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
//!
//! M38: every name here belongs to an instance ([`Svc`]). The default's are
//! the constants below, unchanged; a named instance's are derived from its
//! name (docs/m38-instances.md §2).

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
pub const SYSTEM_UNIT_PATH: &str = "/etc/systemd/system/ferrule.service";
/// M36: the apply unit, its daily timer and the path unit the gateway's
/// requests start it through (docs/m36-self-update.md §3.1).
pub const UPDATE_SERVICE: &str = "ferrule-update.service";
pub const UPDATE_TIMER: &str = "ferrule-update.timer";
pub const UPDATE_PATH: &str = "ferrule-update.path";
pub const UPDATE_LABEL: &str = "ai.ferrule.update";
pub const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

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
    /// M38: the instance the units belong to; `None` is the default.
    pub instance: Option<String>,
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

/// One instance's service in one scope (M38): every name and path that
/// names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Svc {
    pub instance: Option<String>,
    pub scope: Scope,
}

impl Svc {
    /// This process's instance, in this process's scope.
    pub fn current() -> Self {
        Svc {
            instance: crate::instance::current(),
            scope: scope(),
        }
    }

    pub fn new(instance: Option<&str>, scope: Scope) -> Self {
        Svc {
            instance: instance.map(str::to_string),
            scope,
        }
    }

    fn name(&self) -> Option<&str> {
        self.instance.as_deref()
    }

    /// `ferrule`, or `ferrule@<name>`: what `systemctl` and `journalctl -u`
    /// take.
    pub fn short(&self) -> String {
        match self.name() {
            None => "ferrule".into(),
            Some(n) => format!("ferrule@{n}"),
        }
    }

    pub fn systemd_unit(&self) -> String {
        match self.name() {
            None => SYSTEMD_UNIT.into(),
            Some(n) => format!("ferrule@{n}.service"),
        }
    }

    pub fn launchd_label(&self) -> String {
        label(LAUNCHD_LABEL, self.name())
    }

    /// `ferrule-update.<kind>`, or `ferrule-update@<name>.<kind>`.
    pub fn update_unit(&self, kind: &str) -> String {
        update_unit_name(kind, self.name())
    }

    pub fn update_label(&self) -> String {
        label(UPDATE_LABEL, self.name())
    }

    pub fn system_user(&self) -> String {
        match self.name() {
            None => SYSTEM_USER.into(),
            Some(n) => format!("ferrule-{n}"),
        }
    }

    pub fn system_config(&self) -> PathBuf {
        match self.name() {
            None => SYSTEM_CONFIG.into(),
            Some(n) => format!("/etc/ferrule-{n}/config.toml").into(),
        }
    }

    pub fn system_home(&self) -> PathBuf {
        match self.name() {
            None => SYSTEM_HOME.into(),
            Some(n) => format!("/var/lib/ferrule-{n}").into(),
        }
    }

    pub fn system_data(&self) -> PathBuf {
        match self.name() {
            None => SYSTEM_DATA.into(),
            Some(_) => self.system_home().join("data"),
        }
    }

    pub fn system_workspace(&self) -> PathBuf {
        match self.name() {
            None => SYSTEM_WORKSPACE.into(),
            Some(_) => self.system_home().join("workspace"),
        }
    }

    pub fn system_unit_path(&self) -> PathBuf {
        match self.name() {
            None => SYSTEM_UNIT_PATH.into(),
            Some(_) => Path::new(SYSTEM_UNIT_DIR).join(self.systemd_unit()),
        }
    }

    /// Where the unit file goes.
    pub fn unit_path(&self) -> Result<PathBuf> {
        if self.scope == Scope::System {
            return Ok(self.system_unit_path());
        }
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        Ok(if cfg!(target_os = "macos") {
            launch_agents(&home).join(format!("{}.plist", self.launchd_label()))
        } else {
            user_unit_dir(&home).join(self.systemd_unit())
        })
    }

    /// Where launchd sends the gateway's output; systemd has the journal.
    pub fn log_path(&self) -> Option<PathBuf> {
        cfg!(target_os = "macos")
            .then(dirs::home_dir)
            .flatten()
            .map(|home| {
                home.join("Library/Logs")
                    .join(crate::instance::dir_name(self.name()))
                    .join("gateway.log")
            })
    }

    /// How to read the gateway's logs, for messages.
    pub fn logs_hint(&self) -> String {
        match self.log_path() {
            Some(path) => format!("tail -f {}", path.display()),
            None if self.scope == Scope::System => format!("journalctl -u {} -f", self.short()),
            None => format!("journalctl --user -u {} -f", self.short()),
        }
    }

    /// The command that restarts the service, for messages.
    pub fn restart_hint(&self) -> String {
        if cfg!(target_os = "macos") {
            format!(
                "launchctl kickstart -k gui/$(id -u)/{}",
                self.launchd_label()
            )
        } else if self.scope == Scope::System {
            format!("sudo systemctl restart {}", self.short())
        } else {
            format!("systemctl --user restart {}", self.short())
        }
    }

    pub fn status(&self) -> Status {
        let unit = match self.unit_path() {
            Ok(unit) => unit,
            Err(e) => return Status::Unsupported(e.to_string()),
        };
        if cfg!(target_os = "macos") {
            if !unit.exists() {
                return Status::NotInstalled;
            }
            let running = launchctl(&["print", &self.launchd_target()])
                .map(|out| out.contains("state = running"))
                .unwrap_or(false);
            return Status::Installed { running, unit };
        }
        if !cfg!(target_os = "linux") {
            return Status::Unsupported(
                "background services are set up on Linux and macOS only for now".into(),
            );
        }
        if let Err(why) = self.systemd_available() {
            return Status::Unsupported(why);
        }
        if !unit.exists() {
            return Status::NotInstalled;
        }
        let running = manager("systemctl")
            .args(self.scope_flag())
            .args(["is-active", "--quiet", &self.systemd_unit()])
            .status()
            .is_ok_and(|s| s.success());
        Status::Installed { running, unit }
    }

    /// The config and workspace the installed unit pins.
    pub fn installed(&self) -> Option<(PathBuf, PathBuf)> {
        parse_unit(&std::fs::read_to_string(self.unit_path().ok()?).ok()?)
    }

    /// The binary the installed unit runs.
    pub fn installed_exe(&self) -> Option<PathBuf> {
        installed_exe_at(&self.unit_path().ok()?)
    }

    /// Has the service's binary been replaced since the service started —
    /// an upgrade it hasn't picked up? `None` when that can't be told (not
    /// running, or no `ps`).
    pub fn binary_changed_since_start(&self) -> Option<bool> {
        let exe = self.installed_exe()?;
        let pid = self.main_pid()?;
        #[cfg(target_os = "linux")]
        if let Ok(link) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if link.to_string_lossy().ends_with(" (deleted)") {
                return Some(true);
            }
        }
        let out = Command::new("ps")
            .args(["-o", "etime=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let running_for = parse_etime(String::from_utf8_lossy(&out.stdout).trim())?;
        let modified = std::fs::metadata(&exe).ok()?.modified().ok()?;
        let age = modified.elapsed().unwrap_or_default().as_secs();
        Some(changed_since_start(age, running_for))
    }

    /// The running service's process id.
    pub fn main_pid(&self) -> Option<u32> {
        let pid = if cfg!(target_os = "macos") {
            let out = launchctl(&["print", &self.launchd_target()]).ok()?;
            out.lines()
                .find_map(|l| l.trim().strip_prefix("pid = "))?
                .trim()
                .parse()
                .ok()?
        } else {
            let out = manager("systemctl")
                .args(self.scope_flag())
                .args([
                    "show",
                    "--property=MainPID",
                    "--value",
                    &self.systemd_unit(),
                ])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()?
        };
        (pid != 0).then_some(pid)
    }

    /// Write the unit, then enable and start it (restarting it if it was
    /// already running, so a changed unit takes effect).
    pub fn install(&self, spec: &Spec) -> Result<Vec<String>> {
        let unit = self.unit_path()?;
        let dir = unit.parent().context("unit path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut notes = Vec::new();
        let data = crate::config::data_dir()?;
        if cfg!(target_os = "macos") {
            let log = self.log_path().context("no home dir for the log")?;
            std::fs::create_dir_all(log.parent().context("log path has no parent")?)?;
            std::fs::write(&unit, launchd_plist(spec, &log))
                .with_context(|| format!("writing {}", unit.display()))?;
            // bootstrap refuses a label that's already loaded; bootout first.
            let _ = launchctl(&["bootout", &self.launchd_target()]);
            launchctl(&[
                "bootstrap",
                &format!("gui/{}", uid()),
                &unit.to_string_lossy(),
            ])?;
            self.install_update_units(spec, &data)?;
            return Ok(notes);
        }
        if !cfg!(target_os = "linux") {
            bail!("background services are set up on Linux and macOS only");
        }
        self.systemd_available().map_err(|why| anyhow!(why))?;
        if self.scope == Scope::System {
            return self.install_system(spec, &data);
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
        self.install_update_units(spec, &data)?;
        let name = self.systemd_unit();
        self.systemctl(&["enable", &name])?;
        self.systemctl(&["restart", &name])?;
        // Without lingering, user services stop at logout and don't start at
        // boot — which is the whole point on a server.
        if !lingering() {
            let user = std::env::var("USER").unwrap_or_default();
            let ok = manager("loginctl")
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

    pub fn restart(&self) -> Result<()> {
        if cfg!(target_os = "macos") {
            launchctl(&["kickstart", "-k", &self.launchd_target()])?;
            Ok(())
        } else {
            self.systemctl(&["restart", &self.systemd_unit()])
        }
    }

    /// Stop, disable and delete the unit.
    pub fn uninstall(&self) -> Result<()> {
        let unit = self.unit_path()?;
        self.uninstall_update_units()?;
        if cfg!(target_os = "macos") {
            let _ = launchctl(&["bootout", &self.launchd_target()]);
        } else {
            let _ = self.systemctl(&["disable", "--now", &self.systemd_unit()]);
        }
        remove_if_there(&unit)?;
        if !cfg!(target_os = "macos") {
            let _ = self.systemctl(&["daemon-reload"]);
        }
        Ok(())
    }

    /// Where the update units go: beside the gateway's.
    fn update_unit_dir(&self) -> Result<PathBuf> {
        if self.scope == Scope::System {
            return Ok(PathBuf::from(SYSTEM_UNIT_DIR));
        }
        Ok(self
            .unit_path()?
            .parent()
            .context("unit path has no parent")?
            .to_path_buf())
    }

    /// Are this instance's update units installed, the system's or this
    /// user's? The gateway asks as the service's user, not as root.
    pub fn update_units_installed(&self) -> bool {
        let name = if cfg!(target_os = "macos") {
            format!("{}.plist", self.update_label())
        } else {
            self.update_unit("timer")
        };
        (cfg!(target_os = "linux") && Path::new(SYSTEM_UNIT_DIR).join(&name).exists())
            || self
                .update_unit_dir()
                .is_ok_and(|dir| dir.join(&name).exists())
    }

    /// Write, then turn on, the apply unit with its timer and path unit
    /// (launchd: one agent with a calendar and a watched path). Called with
    /// the gateway's unit already written, so one `daemon-reload` covers
    /// both.
    pub fn install_update_units(&self, spec: &Spec, data: &Path) -> Result<()> {
        let dir = self.update_unit_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::create_dir_all(data.join("update"))?;
        let request = data.join("update").join("request");
        if cfg!(target_os = "macos") {
            let label = self.update_label();
            let plist = dir.join(format!("{label}.plist"));
            let log = self
                .log_path()
                .context("no home dir for the log")?
                .with_file_name("update.log");
            // A random minute of the night, so installs don't all ask at once.
            let slot = uuid::Uuid::new_v4().as_u128() as u32;
            let (hour, minute) = (2 + slot % 4, (slot >> 8) % 60);
            std::fs::write(
                &plist,
                launchd_update_plist(spec, &request, &log, hour, minute),
            )
            .with_context(|| format!("writing {}", plist.display()))?;
            let target = format!("gui/{}/{label}", uid());
            let _ = launchctl(&["bootout", &target]);
            launchctl(&[
                "bootstrap",
                &format!("gui/{}", uid()),
                &plist.to_string_lossy(),
            ])?;
            return Ok(());
        }
        if request.to_string_lossy().contains(['\n', '\r']) {
            bail!("a path with a line break can't go in a systemd unit");
        }
        let system = self.scope == Scope::System;
        if system {
            // Root's: the service's user reads it, only root writes it.
            let state = self.system_home().join("update");
            std::fs::create_dir_all(&state)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))?;
            }
            let user = self.system_user();
            let owner = format!("{user}:{user}");
            let _ = run(Command::new("chown").arg(&owner).arg(data.join("update")));
        }
        let service = if system {
            system_update_unit(spec, data)
        } else {
            user_update_unit(spec)
        };
        let (timer, path) = (self.update_unit("timer"), self.update_unit("path"));
        for (name, text) in [
            (self.update_unit("service"), service),
            (timer.clone(), update_timer()),
            (
                path.clone(),
                update_path_unit(&request, system, self.name()),
            ),
        ] {
            std::fs::write(dir.join(&name), text)
                .with_context(|| format!("writing {}", dir.join(&name).display()))?;
        }
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", "--now", &timer, &path])?;
        Ok(())
    }

    /// Stop and delete the update units; none there is fine.
    pub fn uninstall_update_units(&self) -> Result<()> {
        let dir = self.update_unit_dir()?;
        if cfg!(target_os = "macos") {
            let label = self.update_label();
            let _ = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
            remove_if_there(&dir.join(format!("{label}.plist")))?;
            return Ok(());
        }
        if !cfg!(target_os = "linux") {
            return Ok(());
        }
        let names = [
            self.update_unit("timer"),
            self.update_unit("path"),
            self.update_unit("service"),
        ];
        if names.iter().any(|n| dir.join(n).exists()) {
            let _ = self.systemctl(&["disable", "--now", &names[0], &names[1]]);
        }
        for name in &names {
            remove_if_there(&dir.join(name))?;
        }
        Ok(())
    }

    /// The system unit: create the account if needed, hand it its data dir
    /// and workspace, and let it read (only read) the config.
    fn install_system(&self, spec: &Spec, data: &Path) -> Result<Vec<String>> {
        let problems = system_problems(spec, data);
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        let created = self.ensure_system_user()?;
        std::fs::create_dir_all(data.join("update"))?;
        self.own_system_files(Some(&spec.workspace))?;
        let unit = self.system_unit_path();
        std::fs::write(&unit, system_unit(spec, data))
            .with_context(|| format!("writing {}", unit.display()))?;
        self.install_update_units(spec, data)?;
        let name = self.systemd_unit();
        self.systemctl(&["enable", &name])?;
        self.systemctl(&["restart", &name])?;
        let mut notes = Vec::new();
        if created {
            notes.push(format!(
                "created the system user `{}` (no login, no sudo); it owns {} and {}",
                self.system_user(),
                data.display(),
                spec.workspace.display()
            ));
        }
        Ok(notes)
    }

    /// Create the system user if it doesn't exist; `true` if it was
    /// created. An existing one must be a system account nobody can log in
    /// as, outside every admin group.
    fn ensure_system_user(&self) -> Result<bool> {
        let user = self.system_user();
        let out = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .context("running getent")?;
        if out.status.success() {
            let groups = Command::new("id")
                .args(["-nG", &user])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            check_existing_user(&user, &String::from_utf8_lossy(&out.stdout), &groups)
                .map_err(|why| anyhow!(why))?;
            return Ok(false);
        }
        let shell = ["/usr/sbin/nologin", "/sbin/nologin"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap_or("/bin/false");
        let home = self.system_home();
        run(Command::new("useradd")
            .args(["--system", "--user-group", "--home-dir"])
            .arg(&home)
            .args(["--no-create-home", "--shell", shell, "--comment"])
            .arg(match self.name() {
                None => "ferrule agent".to_string(),
                Some(n) => format!("ferrule agent {n}"),
            })
            .arg(&user))?;
        Ok(true)
    }

    /// After root wrote them: the service's user owns its home, data dir
    /// and workspace; the config stays root's, readable by its group. A
    /// no-op while the account doesn't exist yet.
    pub fn own_system_files(&self, workspace: Option<&Path>) -> Result<()> {
        let user = self.system_user();
        let exists = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .is_ok_and(|o| o.status.success());
        if !exists {
            return Ok(());
        }
        let owner = format!("{user}:{user}");
        let home = self.system_home();
        std::fs::create_dir_all(&home)?;
        run(Command::new("chown").arg(&owner).arg(&home))?;
        run(Command::new("chmod").arg("750").arg(&home))?;
        let data = crate::config::data_dir()?;
        let mut mine = vec![data.as_path()];
        mine.extend(workspace);
        for dir in mine {
            run(Command::new("chown").arg("-R").arg(&owner).arg(dir))?;
        }
        let config = self.system_config();
        let group = format!("root:{user}");
        if let Some(dir) = config.parent().filter(|d| d.exists()) {
            run(Command::new("chown").arg(&group).arg(dir))?;
            run(Command::new("chmod").arg("750").arg(dir))?;
        }
        if config.exists() {
            run(Command::new("chown").arg(&group).arg(&config))?;
            run(Command::new("chmod").arg("640").arg(&config))?;
        }
        Ok(())
    }

    /// Is there a systemd manager to talk to? Containers, WSL without
    /// systemd and plain SSH sessions without a user bus often have none.
    fn systemd_available(&self) -> std::result::Result<(), String> {
        let out = manager("systemctl")
            .args(self.scope_flag())
            .arg("show-environment")
            .output()
            .map_err(|_| "systemctl isn't installed".to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "no systemd {} ({})",
                if self.scope == Scope::System {
                    "running"
                } else {
                    "user session"
                },
                err.lines().next().unwrap_or("systemctl failed").trim()
            ))
        }
    }

    /// `--user`, except for the system unit.
    fn scope_flag(&self) -> &'static [&'static str] {
        match self.scope {
            Scope::User => &["--user"],
            Scope::System => &[],
        }
    }

    fn systemctl(&self, args: &[&str]) -> Result<()> {
        let out = manager("systemctl")
            .args(self.scope_flag())
            .args(args)
            .output()?;
        if !out.status.success() {
            bail!(
                "systemctl {} failed: {}",
                self.scope_flag()
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

    fn launchd_target(&self) -> String {
        format!("gui/{}/{}", uid(), self.launchd_label())
    }
}

/// `base`, or `base.<name>`: a launchd label (a name has no dots).
fn label(base: &str, instance: Option<&str>) -> String {
    match instance {
        None => base.into(),
        Some(n) => format!("{base}.{n}"),
    }
}

fn update_unit_name(kind: &str, instance: Option<&str>) -> String {
    match (instance, kind) {
        (None, "service") => UPDATE_SERVICE.into(),
        (None, "timer") => UPDATE_TIMER.into(),
        (None, "path") => UPDATE_PATH.into(),
        (None, _) => format!("ferrule-update.{kind}"),
        (Some(n), _) => format!("ferrule-update@{n}.{kind}"),
    }
}

fn launch_agents(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
}

fn user_unit_dir(home: &Path) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
}

/// The named instances with a gateway unit in `scope` here:
/// `ferrule@<name>.service`, or `ai.ferrule.gateway.<name>.plist`.
pub fn unit_names(scope: Scope) -> Vec<String> {
    let dir = match (scope, dirs::home_dir()) {
        (Scope::System, _) => PathBuf::from(SYSTEM_UNIT_DIR),
        (Scope::User, Some(home)) if cfg!(target_os = "macos") => launch_agents(&home),
        (Scope::User, Some(home)) => user_unit_dir(&home),
        (Scope::User, None) => return Vec::new(),
    };
    names_in_units(&dir, cfg!(target_os = "macos") && scope == Scope::User)
}

/// The instance names in a unit dir's file names.
pub fn names_in_units(dir: &Path, launchd: bool) -> Vec<String> {
    let (prefix, suffix) = if launchd {
        ("ai.ferrule.gateway.", ".plist")
    } else {
        ("ferrule@", ".service")
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name();
            let name = file.to_str()?.strip_prefix(prefix)?.strip_suffix(suffix)?;
            crate::instance::validate(name)
                .is_ok()
                .then(|| name.to_string())
        })
        .collect();
    names.sort();
    names
}

// The current instance's service: what setup, doctor, status, stop and the
// update flow use.

/// How to read the gateway's logs, for messages.
pub fn logs_hint() -> String {
    Svc::current().logs_hint()
}

pub fn status() -> Status {
    Svc::current().status()
}

/// The config and workspace an installed unit pins — for `ferrule doctor`
/// to compare with what this shell sees, and for setup's defaults.
pub fn installed() -> Option<(PathBuf, PathBuf)> {
    Svc::current().installed()
}

/// The binary an installed unit runs.
pub fn installed_exe() -> Option<PathBuf> {
    Svc::current().installed_exe()
}

/// The binary the unit at `unit` runs.
pub fn installed_exe_at(unit: &Path) -> Option<PathBuf> {
    parse_exe(&std::fs::read_to_string(unit).ok()?)
}

/// The pinned config and workspace of the unit at `unit`.
pub fn installed_at(unit: &Path) -> Option<(PathBuf, PathBuf)> {
    parse_unit(&std::fs::read_to_string(unit).ok()?)
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

/// The data dir a system unit pins (`FERRULE_DATA_DIR`); user units don't.
pub fn pinned_data(unit: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(unit).ok()?;
    text.lines().find_map(|line| {
        let value = line
            .strip_prefix("Environment=\"FERRULE_DATA_DIR=")?
            .strip_suffix('"')?;
        Some(PathBuf::from(systemd_unescape(value)))
    })
}

fn parse_exe(text: &str) -> Option<PathBuf> {
    if text.contains("<plist") {
        let after = text.split_once("<key>ProgramArguments</key>")?.1;
        let value = after.split_once("<string>")?.1.split_once("</string>")?.0;
        return Some(PathBuf::from(xml_unescape(value)));
    }
    let rest = text
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart=\""))?;
    // The closing quote is the first one not escaped by a backslash.
    let mut escaped = false;
    let end = rest.char_indices().find_map(|(i, c)| match c {
        '\\' if !escaped => {
            escaped = true;
            None
        }
        '"' if !escaped => Some(i),
        _ => {
            escaped = false;
            None
        }
    })?;
    Some(PathBuf::from(systemd_unescape(
        &rest[..end].replace("$$", "$"),
    )))
}

/// Has the service's binary been replaced since the service started?
pub fn binary_changed_since_start() -> Option<bool> {
    Svc::current().binary_changed_since_start()
}

/// A binary `binary_age` seconds old under a process `running_for`
/// seconds: newer means it was replaced after the start. `ps` rounds to
/// the second, hence the slack.
pub fn changed_since_start(binary_age: u64, running_for: u64) -> bool {
    binary_age + 2 < running_for
}

/// `ps -o etime`: `[[dd-]hh:]mm:ss`, in seconds.
pub fn parse_etime(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, text),
    };
    let parts: Vec<u64> = clock
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, s) = match parts[..] {
        [m, s] => (0, m, s),
        [h, m, s] => (h, m, s),
        _ => return None,
    };
    Some(((days * 24 + h) * 60 + m) * 60 + s)
}

/// The command that restarts the service, for messages.
pub fn restart_hint() -> String {
    Svc::current().restart_hint()
}

/// Write the unit, then enable and start it.
pub fn install(spec: &Spec) -> Result<Vec<String>> {
    Svc::current().install(spec)
}

pub fn restart() -> Result<()> {
    Svc::current().restart()
}

/// Stop, disable and delete the unit.
pub fn uninstall() -> Result<()> {
    Svc::current().uninstall()
}

/// Are the update units installed, the system's or this user's?
pub fn update_units_installed() -> bool {
    Svc::current().update_units_installed()
}

pub fn own_system_files(workspace: Option<&Path>) -> Result<()> {
    Svc::current().own_system_files(workspace)
}

fn remove_if_there(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// `Environment="FERRULE_INSTANCE=<name>"` and a line break, for a named
/// instance's systemd units; nothing for the default's.
fn instance_env(spec: &Spec) -> String {
    spec.instance.as_deref().map_or_else(String::new, |n| {
        format!(
            "Environment={}\n",
            systemd_quote(&format!("{}={n}", crate::instance::ENV), false)
        )
    })
}

/// The same for a launchd plist's `EnvironmentVariables`.
fn instance_plist_env(spec: &Spec) -> String {
    spec.instance.as_deref().map_or_else(String::new, |n| {
        format!(
            "\n    <key>{}</key><string>{n}</string>",
            crate::instance::ENV
        )
    })
}

/// ` (<name>)` after a description, for a named instance.
fn instance_suffix(spec: &Spec) -> String {
    spec.instance
        .as_deref()
        .map_or_else(String::new, |n| format!(" ({n})"))
}

/// The system apply unit: root, since it replaces a root-owned binary and
/// restarts the service; everything it doesn't need is closed off. The
/// idle wait is up to 6 h, hence the long start timeout.
pub fn system_update_unit(spec: &Spec, data: &Path) -> String {
    let arg = |p: &Path| systemd_quote(&p.to_string_lossy(), true);
    let env = |name: &str, value: &str| systemd_quote(&format!("{name}={value}"), false);
    format!(
        "# Written by `ferrule setup` as root; `ferrule setup --refresh-service` rewrites it.\n\
         [Unit]\n\
         Description=ferrule update{} (signed releases, rolled back if they don't start)\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} update --apply\n\
         {}\
         Environment={}\n\
         Environment={}\n\
         Environment={}\n\
         TimeoutStartSec=7h\n\
         Nice=10\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectHome=read-only\n\
         ProtectKernelTunables=yes\n\
         ProtectControlGroups=yes\n\
         RestrictSUIDSGID=yes\n",
        instance_suffix(spec),
        arg(&spec.exe),
        instance_env(spec),
        env("FERRULE_CONFIG", &spec.config.to_string_lossy()),
        env("FERRULE_DATA_DIR", &data.to_string_lossy()),
        env("PATH", &spec.path_env),
    )
}

/// The user apply unit: the same, as the user who owns the binary.
pub fn user_update_unit(spec: &Spec) -> String {
    let arg = |p: &Path| systemd_quote(&p.to_string_lossy(), true);
    let env = |name: &str, value: &str| systemd_quote(&format!("{name}={value}"), false);
    format!(
        "# Written by `ferrule setup`; `ferrule setup --refresh-service` rewrites it.\n\
         [Unit]\n\
         Description=ferrule update{} (signed releases, rolled back if they don't start)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} update --apply\n\
         {}\
         Environment={}\n\
         Environment={}\n\
         TimeoutStartSec=7h\n\
         Nice=10\n",
        instance_suffix(spec),
        arg(&spec.exe),
        instance_env(spec),
        env("FERRULE_CONFIG", &spec.config.to_string_lossy()),
        env("PATH", &spec.path_env),
    )
}

/// Daily, at a random time within six hours of midnight; a day missed while
/// the machine was off runs at the next boot.
pub fn update_timer() -> String {
    "# Written by `ferrule setup`.\n\
     [Unit]\n\
     Description=ferrule daily update check\n\
     \n\
     [Timer]\n\
     OnCalendar=daily\n\
     RandomizedDelaySec=6h\n\
     Persistent=true\n\
     \n\
     [Install]\n\
     WantedBy=timers.target\n"
        .into()
}

/// Starts the instance's apply unit when its gateway leaves a request.
pub fn update_path_unit(request: &Path, system: bool, instance: Option<&str>) -> String {
    format!(
        "# Written by `ferrule setup`.\n\
         [Unit]\n\
         Description=ferrule update requests from the gateway\n\
         \n\
         [Path]\n\
         PathExists={}\n\
         Unit={}\n\
         \n\
         [Install]\n\
         WantedBy={}\n",
        request.to_string_lossy().replace('%', "%%"),
        update_unit_name("service", instance),
        if system {
            "paths.target"
        } else {
            "default.target"
        },
    )
}

pub fn launchd_update_plist(
    spec: &Spec,
    request: &Path,
    log: &Path,
    hour: u32,
    minute: u32,
) -> String {
    let s = |text: &str| format!("<string>{}</string>", xml_escape(text));
    let p = |path: &Path| s(&path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `ferrule setup`; `ferrule setup --refresh-service` rewrites it. -->
<plist version="1.0">
<dict>
  <key>Label</key>{label}
  <key>ProgramArguments</key>
  <array>{exe}{update}{apply}</array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>FERRULE_CONFIG</key>{config}
    <key>PATH</key>{path}{instance}
  </dict>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key><integer>{hour}</integer>
    <key>Minute</key><integer>{minute}</integer>
  </dict>
  <key>WatchPaths</key>
  <array>{request}</array>
  <key>RunAtLoad</key><false/>
  <key>StandardOutPath</key>{log}
  <key>StandardErrorPath</key>{log}
</dict>
</plist>
"#,
        label = s(&label(UPDATE_LABEL, spec.instance.as_deref())),
        exe = p(&spec.exe),
        update = s("update"),
        apply = s("--apply"),
        config = p(&spec.config),
        path = s(&spec.path_env),
        instance = instance_plist_env(spec),
        request = p(request),
        log = p(log),
    )
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

/// The passwd line and group list of an existing system `user`: fine only
/// if it's no one's login and holds no admin rights.
pub fn check_existing_user(
    user: &str,
    passwd: &str,
    groups: &str,
) -> std::result::Result<(), String> {
    let fields: Vec<&str> = passwd.trim().split(':').collect();
    let (Some(uid), Some(shell)) = (fields.get(2), fields.get(6)) else {
        return Err(format!("can't read the `{user}` account: {passwd:?}"));
    };
    if *uid == "0" {
        return Err(format!("the existing `{user}` account is uid 0"));
    }
    if !(shell.ends_with("/nologin") || shell.ends_with("/false")) {
        return Err(format!(
            "an account `{user}` already exists and can log in ({shell}); \
             it isn't safe to run the service as it"
        ));
    }
    let admin = ["root", "sudo", "wheel", "admin", "adm", "docker", "lxd"];
    if let Some(g) = groups.split_whitespace().find(|g| admin.contains(g)) {
        return Err(format!(
            "the existing `{user}` account is in the `{g}` group; remove it from there first"
        ));
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
    let user = crate::instance::dir_name(spec.instance.as_deref());
    format!(
        "# Written by `ferrule setup` as root — re-run it to change this service.\n\
         [Unit]\n\
         Description=ferrule gateway{}\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         User={user}\n\
         Group={user}\n\
         ExecStart={} gateway --workspace {}\n\
         WorkingDirectory={}\n\
         {}\
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
         WatchdogSec=120\n\
         NotifyAccess=main\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        instance_suffix(spec),
        arg(&spec.exe),
        arg(&spec.workspace),
        spec.workspace.to_string_lossy().replace('%', "%%"),
        instance_env(spec),
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
         Description=ferrule gateway{}\n\
         \n\
         [Service]\n\
         ExecStart={} gateway --workspace {}\n\
         WorkingDirectory={}\n\
         {}\
         Environment={}\n\
         Environment={}\n\
         Restart=always\n\
         RestartSec=5\n\
         WatchdogSec=120\n\
         NotifyAccess=main\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        instance_suffix(spec),
        arg(&spec.exe),
        arg(&spec.workspace),
        // A bare path: this setting takes no quotes, only specifiers.
        spec.workspace.to_string_lossy().replace('%', "%%"),
        instance_env(spec),
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
    <key>PATH</key>{path}{instance}
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key>{log}
  <key>StandardErrorPath</key>{log}
</dict>
</plist>
"#,
        label = s(&label(LAUNCHD_LABEL, spec.instance.as_deref())),
        exe = p(&spec.exe),
        gateway = s("gateway"),
        flag = s("--workspace"),
        ws = p(&spec.workspace),
        config = p(&spec.config),
        path = s(&spec.path_env),
        instance = instance_plist_env(spec),
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

fn lingering() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    manager("loginctl")
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

/// `systemctl`, `loginctl` or `launchctl`, without the gateway's own
/// `NOTIFY_SOCKET`: a systemd tool that inherits it reports its exit status
/// there (`ERRNO=…`) as if it were the service.
fn manager(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("NOTIFY_SOCKET")
        .env_remove("WATCHDOG_USEC")
        .env_remove("WATCHDOG_PID");
    cmd
}

fn launchctl(args: &[&str]) -> Result<String> {
    let out = manager("launchctl").args(args).output()?;
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
            instance: None,
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
    fn both_units_turn_on_systemds_watchdog() {
        for unit in [
            systemd_unit(&spec()),
            system_unit(&system_spec(), Path::new(SYSTEM_DATA)),
        ] {
            assert!(
                unit.contains("\nWatchdogSec=120\nNotifyAccess=main\n"),
                "{unit}"
            );
        }
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
            instance: None,
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
        assert_eq!(check_existing_user("ferrule", nologin, "ferrule\n"), Ok(()));
        assert_eq!(
            check_existing_user(
                "ferrule",
                "ferrule:x:999:999::/var/lib/ferrule:/bin/false",
                "ferrule"
            ),
            Ok(())
        );
        let bash = "ferrule:x:1001:1001::/home/ferrule:/bin/bash";
        assert!(check_existing_user("ferrule", bash, "ferrule")
            .unwrap_err()
            .contains("log in"));
        assert!(check_existing_user("ferrule", nologin, "ferrule sudo")
            .unwrap_err()
            .contains("`sudo`"));
        assert!(check_existing_user("ferrule", nologin, "ferrule docker").is_err());
        assert!(
            check_existing_user("ferrule", "ferrule:x:0:0::/:/usr/sbin/nologin", "root").is_err()
        );
        assert!(check_existing_user("ferrule", "garbage", "").is_err());
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

    #[test]
    fn the_binary_a_unit_runs_reads_back_whatever_its_path() {
        let mut odd = spec();
        odd.exe = "/opt/a \"b\" $x 5%/ferrule".into();
        for spec in [spec(), odd, system_spec()] {
            assert_eq!(parse_exe(&systemd_unit(&spec)), Some(spec.exe.clone()));
            assert_eq!(
                parse_exe(&system_unit(&spec, Path::new(SYSTEM_DATA))),
                Some(spec.exe.clone())
            );
            let plist = launchd_plist(&spec, Path::new("/tmp/log"));
            assert_eq!(parse_exe(&plist), Some(spec.exe.clone()));
        }
        assert_eq!(parse_exe("[Service]\nExecStart=/no/quotes\n"), None);
    }

    #[test]
    fn a_binary_newer_than_the_process_means_an_upgrade_not_picked_up() {
        assert_eq!(parse_etime("05:07"), Some(307));
        assert_eq!(parse_etime("1:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:00:00"), Some(2 * 86400 + 3600));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        // Installed an hour ago, running for a day: replaced since.
        assert!(changed_since_start(3600, 86400));
        // Installed, then started: not.
        assert!(!changed_since_start(86400, 3600));
        // Installed and started in the same second or two: not.
        assert!(!changed_since_start(100, 101));
    }

    /// The spec the 0.8.0 fixtures were written with.
    fn golden_spec(instance: Option<&str>) -> Spec {
        Spec {
            exe: "/usr/local/bin/ferrule".into(),
            workspace: "/srv/ws".into(),
            config: "/etc/ferrule/config.toml".into(),
            path_env: "/usr/local/bin:/usr/bin".into(),
            instance: instance.map(str::to_string),
        }
    }

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/units-0.8.0")
            .join(name);
        // A Windows checkout may turn the line ends into CRLF.
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .replace("\r\n", "\n")
    }

    #[test]
    fn the_default_instances_units_are_0_8_0s_byte_for_byte() {
        let s = golden_spec(None);
        let data = Path::new(SYSTEM_DATA);
        for (name, text) in [
            ("system_unit", system_unit(&s, data)),
            ("systemd_unit", systemd_unit(&s)),
            (
                "launchd_plist",
                launchd_plist(&s, Path::new("/Users/a/Library/Logs/ferrule/gateway.log")),
            ),
            ("system_update_unit", system_update_unit(&s, data)),
            ("user_update_unit", user_update_unit(&s)),
            ("update_timer", update_timer()),
            (
                "update_path_unit_sys",
                update_path_unit(
                    Path::new("/var/lib/ferrule/data/update/request"),
                    true,
                    None,
                ),
            ),
            (
                "update_path_unit_user",
                update_path_unit(
                    Path::new("/home/u/.local/share/ferrule/update/request"),
                    false,
                    None,
                ),
            ),
            (
                "launchd_update_plist",
                launchd_update_plist(
                    &s,
                    Path::new("/Users/a/Library/Application Support/ferrule/update/request"),
                    Path::new("/Users/a/Library/Logs/ferrule/update.log"),
                    3,
                    17,
                ),
            ),
        ] {
            assert_eq!(
                text,
                fixture(name),
                "{name} changed for the default instance"
            );
        }
    }

    #[test]
    fn the_default_instances_names_are_0_8_0s() {
        for scope in [Scope::User, Scope::System] {
            let svc = Svc::new(None, scope);
            assert_eq!(svc.short(), "ferrule");
            assert_eq!(svc.systemd_unit(), SYSTEMD_UNIT);
            assert_eq!(svc.launchd_label(), LAUNCHD_LABEL);
            assert_eq!(svc.update_unit("service"), UPDATE_SERVICE);
            assert_eq!(svc.update_unit("timer"), UPDATE_TIMER);
            assert_eq!(svc.update_unit("path"), UPDATE_PATH);
            assert_eq!(svc.update_label(), UPDATE_LABEL);
            assert_eq!(svc.system_user(), SYSTEM_USER);
            assert_eq!(svc.system_config(), Path::new(SYSTEM_CONFIG));
            assert_eq!(svc.system_home(), Path::new(SYSTEM_HOME));
            assert_eq!(svc.system_data(), Path::new(SYSTEM_DATA));
            assert_eq!(svc.system_workspace(), Path::new(SYSTEM_WORKSPACE));
            assert_eq!(svc.system_unit_path(), Path::new(SYSTEM_UNIT_PATH));
        }
        assert_eq!(
            Svc::new(None, Scope::System).restart_hint(),
            if cfg!(target_os = "macos") {
                "launchctl kickstart -k gui/$(id -u)/ai.ferrule.gateway"
            } else {
                "sudo systemctl restart ferrule"
            }
        );
    }

    #[test]
    fn a_named_instance_gets_its_own_names_everywhere() {
        let svc = Svc::new(Some("work"), Scope::System);
        assert_eq!(svc.short(), "ferrule@work");
        assert_eq!(svc.systemd_unit(), "ferrule@work.service");
        assert_eq!(svc.launchd_label(), "ai.ferrule.gateway.work");
        assert_eq!(svc.update_unit("timer"), "ferrule-update@work.timer");
        assert_eq!(svc.update_label(), "ai.ferrule.update.work");
        assert_eq!(svc.system_user(), "ferrule-work");
        assert_eq!(
            svc.system_config(),
            Path::new("/etc/ferrule-work/config.toml")
        );
        assert_eq!(svc.system_data(), Path::new("/var/lib/ferrule-work/data"));
        assert_eq!(
            svc.system_workspace(),
            Path::new("/var/lib/ferrule-work/workspace")
        );
        assert_eq!(
            svc.system_unit_path(),
            Path::new("/etc/systemd/system/ferrule@work.service")
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(svc.logs_hint(), "journalctl -u ferrule@work -f");
            assert_eq!(svc.restart_hint(), "sudo systemctl restart ferrule@work");
        }
    }

    #[test]
    fn a_named_instances_units_carry_its_name() {
        let s = golden_spec(Some("work"));
        let data = Path::new("/var/lib/ferrule-work/data");
        let env = "Environment=\"FERRULE_INSTANCE=work\"\n";
        let unit = system_unit(&s, data);
        assert!(
            unit.contains("Description=ferrule gateway (work)\n"),
            "{unit}"
        );
        assert!(
            unit.contains("User=ferrule-work\nGroup=ferrule-work\n"),
            "{unit}"
        );
        assert!(unit.contains(env), "{unit}");
        // Everything else is the default's.
        let without = unit
            .replace(env, "")
            .replace(" (work)", "")
            .replace("ferrule-work", "ferrule");
        assert_eq!(without, fixture("system_unit"));
        for text in [
            systemd_unit(&s),
            system_update_unit(&s, data),
            user_update_unit(&s),
        ] {
            assert!(text.contains(env), "{text}");
            assert!(text.contains(" (work)"), "{text}");
        }
        assert_eq!(
            parse_unit(&systemd_unit(&s)),
            Some((s.config.clone(), s.workspace.clone()))
        );
        let path = update_path_unit(Path::new("/r"), true, Some("work"));
        assert!(
            path.contains("Unit=ferrule-update@work.service\n"),
            "{path}"
        );
        let plist = launchd_plist(&s, Path::new("/l"));
        assert!(plist.contains("<key>Label</key><string>ai.ferrule.gateway.work</string>"));
        assert!(plist.contains("<key>FERRULE_INSTANCE</key><string>work</string>"));
        assert_eq!(
            parse_unit(&plist),
            Some((s.config.clone(), s.workspace.clone()))
        );
        let plist = launchd_update_plist(&s, Path::new("/r"), Path::new("/l"), 3, 17);
        assert!(plist.contains("<string>ai.ferrule.update.work</string>"));
        assert!(plist.contains("<key>FERRULE_INSTANCE</key><string>work</string>"));
    }

    #[test]
    fn named_units_are_found_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "ferrule.service",
            "ferrule@work.service",
            "ferrule@home.service",
            "ferrule@Bad.service",
            "ferrule-update@work.service",
            "ferrule@x.timer",
            "ai.ferrule.gateway.mac.plist",
            "ai.ferrule.gateway.plist",
        ] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        assert_eq!(names_in_units(dir.path(), false), ["home", "work"]);
        assert_eq!(names_in_units(dir.path(), true), ["mac"]);
    }
}
