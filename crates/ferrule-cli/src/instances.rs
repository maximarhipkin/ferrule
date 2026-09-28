//! `ferrule instances`: every agent on this machine, a new one, removing
//! one; and the collision checks between them (docs/m38-instances.md §5,
//! §6).

use crate::config::Config;
use crate::instance::{self, Roots};
use crate::service::{self, Scope, Svc};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum InstancesCmd {
    /// Every instance on this machine: config, data, service, dashboard port, bot
    List {
        /// One JSON array, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Set up another agent beside this one: its own config, data, bot and service
    New {
        name: String,
        /// Linux: a system service run as its own `ferrule-<name>` user
        #[arg(long)]
        system: bool,
        /// A service run as you, even as root
        #[arg(long, conflicts_with = "system")]
        user: bool,
        /// Only create its config; `ferrule --instance <name> setup` later
        #[arg(long)]
        no_setup: bool,
    },
    /// Stop and remove an instance's service; its config and data stay
    /// unless --purge
    Remove {
        name: String,
        /// Also delete its config and data dirs (never a workspace)
        #[arg(long)]
        purge: bool,
        /// Don't ask before --purge deletes
        #[arg(long)]
        yes: bool,
        /// The system instance of that name (when there's a user one too)
        #[arg(long)]
        system: bool,
        /// The user instance of that name (when there's a system one too)
        #[arg(long, conflicts_with = "system")]
        user: bool,
    },
}

/// An instance found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: Option<String>,
    pub scope: Scope,
    pub config: PathBuf,
    pub data: PathBuf,
    /// Its gateway unit, when one is installed.
    pub unit: Option<PathBuf>,
}

impl Found {
    pub fn svc(&self) -> Svc {
        Svc::new(self.name.as_deref(), self.scope)
    }

    pub fn label(&self) -> &str {
        instance::label(self.name.as_deref())
    }
}

/// Where to look. [`Places::here`] in the program; tests point every
/// field at a temp dir.
#[derive(Debug, Clone)]
pub struct Places {
    pub roots: Roots,
    /// The user's gateway units: `~/.config/systemd/user` or
    /// `~/Library/LaunchAgents`.
    pub user_units: Option<PathBuf>,
    pub launchd: bool,
    /// `/etc` and `/etc/systemd/system`, on Linux.
    pub system: Option<(PathBuf, PathBuf)>,
}

impl Places {
    pub fn here() -> Result<Self> {
        let roots = Roots::get().ok_or_else(|| anyhow!("no config dir on this system"))?;
        let user_units = Svc::new(None, Scope::User)
            .unit_path()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        Ok(Places {
            roots,
            user_units,
            launchd: cfg!(target_os = "macos"),
            system: cfg!(target_os = "linux")
                .then(|| ("/etc".into(), service::SYSTEM_UNIT_DIR.into())),
        })
    }
}

/// Every instance under `places`, the default first, then by name; user
/// ones before system ones.
pub fn discover(places: &Places) -> Vec<Found> {
    let mut found = Vec::new();
    // Users'.
    let unit_of = |name: Option<&str>| {
        let dir = places.user_units.as_ref()?;
        let svc = Svc::new(name, Scope::User);
        let file = if places.launchd {
            format!("{}.plist", svc.launchd_label())
        } else {
            svc.systemd_unit()
        };
        Some(dir.join(file)).filter(|p| p.is_file())
    };
    let mut names: Vec<Option<String>> = vec![None];
    names.extend(places.roots.named().into_iter().map(Some));
    if let Some(dir) = &places.user_units {
        names.extend(
            service::names_in_units(dir, places.launchd)
                .into_iter()
                .map(Some),
        );
    }
    names.sort();
    names.dedup();
    for name in names {
        let unit = unit_of(name.as_deref());
        let own = places.roots.config_file(name.as_deref());
        if unit.is_none() && !own.is_file() {
            continue;
        }
        // An installed unit pins its config.
        let config = unit
            .as_deref()
            .and_then(service::installed_at)
            .map_or(own, |(config, _)| config);
        found.push(Found {
            data: places.roots.data_dir(name.as_deref()),
            name,
            scope: Scope::User,
            config,
            unit,
        });
    }
    // The system's (Linux).
    if let Some((etc, unit_dir)) = &places.system {
        let mut names: Vec<Option<String>> = vec![None];
        names.extend(
            instance::named_in(etc, |d| d.join("config.toml").exists())
                .into_iter()
                .map(Some),
        );
        names.extend(
            service::names_in_units(unit_dir, false)
                .into_iter()
                .map(Some),
        );
        names.sort();
        names.dedup();
        for name in names {
            let svc = Svc::new(name.as_deref(), Scope::System);
            let unit = Some(unit_dir.join(svc.systemd_unit())).filter(|p| p.is_file());
            let config = etc
                .join(instance::dir_name(name.as_deref()))
                .join("config.toml");
            if unit.is_none() && !config.exists() {
                continue;
            }
            let pinned = unit.as_deref().and_then(service::installed_at);
            found.push(Found {
                data: unit
                    .as_deref()
                    .and_then(service::pinned_data)
                    .unwrap_or_else(|| svc.system_data()),
                config: pinned.map_or(config, |(config, _)| config),
                name,
                scope: Scope::System,
                unit,
            });
        }
    }
    found
}

/// What the collision checks compare, for one instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub name: Option<String>,
    /// The Telegram bot id: the token's part before `:`, which isn't secret.
    pub bot: Option<String>,
    /// A fixed dashboard port, when the dashboard is on.
    pub port: Option<u16>,
    /// The local workspace, canonical where it exists.
    pub workspace: Option<PathBuf>,
    /// A remote workspace as `host:port/path`.
    pub ssh: Option<String>,
    pub relay: Option<String>,
    /// Only compared, never shown.
    pub relay_key: Option<String>,
    /// M39: each other channel's account, `(channel, id)`: the WhatsApp
    /// number's id and the like, never a secret.
    pub accounts: Vec<(&'static str, String)>,
    /// Its config or secrets couldn't be read, so some of the above is
    /// missing.
    pub unreadable: Option<String>,
}

/// The token's bot id, if it looks like a bot token.
pub fn bot_id(token: &str) -> Option<String> {
    let (id, rest) = token.trim().split_once(':')?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !rest.is_empty())
        .then(|| id.to_string())
}

impl Facts {
    /// From a config, the secrets that go with it and the workspace its
    /// service pins.
    pub fn from_config(
        name: Option<&str>,
        cfg: &Config,
        secret: impl Fn(&str) -> Option<String>,
        pinned_workspace: Option<&Path>,
    ) -> Self {
        let bot = cfg
            .gateway
            .telegram_token_env
            .as_deref()
            .and_then(&secret)
            .and_then(|t| bot_id(&t));
        let port = (cfg.dashboard.enabled && cfg.dashboard.port != 0).then_some(cfg.dashboard.port);
        let mut facts = Facts {
            name: name.map(str::to_string),
            bot,
            port,
            ..Facts::default()
        };
        match cfg.workspace.as_deref() {
            Some(spec) if ferrule_ssh::is_remote(spec) => {
                if let Ok(t) = ferrule_ssh::Target::parse(spec, &cfg.ssh) {
                    facts.ssh = Some(format!(
                        "{}:{}/{}",
                        t.host.to_ascii_lowercase(),
                        t.port.unwrap_or(22),
                        t.path.trim_matches('/')
                    ));
                }
            }
            Some(local) if pinned_workspace.is_none() => {
                facts.workspace = Some(canonical(&crate::import::expand_tilde(
                    local,
                    dirs::home_dir().as_deref(),
                )));
            }
            _ => {}
        }
        if facts.ssh.is_none() {
            if let Some(ws) = pinned_workspace {
                facts.workspace = Some(canonical(ws));
            }
        }
        if let Some(url) = &cfg.connections.relay_url {
            facts.relay = Some(url.trim().trim_end_matches('/').to_ascii_lowercase());
            facts.relay_key = secret(ferrule_connections::relay::RELAY_KEY_ENV);
        }
        facts.accounts = crate::channels::CHANNELS
            .iter()
            .filter_map(|c| Some((c.name, crate::channels::account(cfg, c.name, &secret)?)))
            .collect();
        // Two daemons ferrule starts can't share a port: the second one's
        // gateway would talk to the first's account.
        if let Some(s) = cfg.gateway.signal.as_ref().filter(|s| s.url.is_none()) {
            facts.accounts.push(("signal-port", s.port.to_string()));
        }
        facts
    }
}

fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| std::path::absolute(path).unwrap_or(path.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Fail,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    pub severity: Severity,
    /// `telegram`, `dashboard`, `workspace`, `ssh`, `relay` or `channel`.
    pub what: &'static str,
    pub other: String,
    pub message: String,
}

/// What `me` shares with each of `others` that it mustn't.
pub fn clashes(me: &Facts, others: &[Facts]) -> Vec<Clash> {
    let mut out = Vec::new();
    for o in others.iter().filter(|o| o.name != me.name) {
        let other = instance::label(o.name.as_deref()).to_string();
        let mut clash = |severity, what, message: String| {
            out.push(Clash {
                severity,
                what,
                other: other.clone(),
                message,
            })
        };
        if let (Some(a), Some(b)) = (&me.bot, &o.bot) {
            if a == b {
                clash(
                    Severity::Fail,
                    "telegram",
                    format!("the same Telegram bot (id {a}) as the instance `{other}`: Telegram lets one of them poll it, so each misses messages (409 Conflict). Give one of them a bot of its own (@BotFather)"),
                );
            }
        }
        if let (Some(a), Some(b)) = (me.port, o.port) {
            if a == b {
                clash(
                    Severity::Fail,
                    "dashboard",
                    format!("dashboard port {a} is the instance `{other}`'s too; the second gateway can't listen on it. Set another `[dashboard] port`, or 0 for any free one"),
                );
            }
        }
        if let (Some(a), Some(b)) = (&me.workspace, &o.workspace) {
            if a == b {
                clash(
                    Severity::Fail,
                    "workspace",
                    format!("the workspace {} is the instance `{other}`'s too: two agents would edit one tree", a.display()),
                );
            }
        }
        if let (Some(a), Some(b)) = (&me.ssh, &o.ssh) {
            if a == b {
                clash(
                    Severity::Fail,
                    "ssh",
                    format!("the remote workspace {a} is the instance `{other}`'s too: two agents would edit one tree"),
                );
            }
        }
        for (channel, id) in &me.accounts {
            if o.accounts.iter().any(|(c, i)| c == channel && i == id) {
                clash(
                    Severity::Fail,
                    "channel",
                    crate::channels::account_clash(channel, id, &other),
                );
            }
        }
        if let (Some(a), Some(b)) = (&me.relay, &o.relay) {
            if a == b {
                match (&me.relay_key, &o.relay_key) {
                    (Some(x), Some(y)) if x != y => clash(
                        Severity::Fail,
                        "relay",
                        format!("the relay {a} is the instance `{other}`'s too, with another key: a relay has one key, so one of them has its logins refused. Deploy a relay of its own (`ferrule connections relay deploy`)"),
                    ),
                    _ => clash(
                        Severity::Note,
                        "relay",
                        format!("shares the relay {a} with the instance `{other}`; redeploying it from one re-keys it and breaks the other"),
                    ),
                }
            }
        }
    }
    out
}

/// The secrets file of an instance's data dir, read if this user can.
fn secrets_of(data: &Path) -> std::result::Result<Vec<(String, String)>, String> {
    let path = data.join("private").join("secrets.env");
    match crate::secrets::read(&path) {
        Ok(entries) => Ok(entries),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(Vec::new())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Another instance's facts, from its files only (never this process's
/// environment, which is this instance's).
pub fn facts_of(found: &Found) -> Facts {
    let pinned = found
        .unit
        .as_deref()
        .and_then(service::installed_at)
        .map(|(_, ws)| ws);
    let cfg = match Config::from_file(&found.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            return Facts {
                name: found.name.clone(),
                workspace: pinned.as_deref().map(canonical),
                unreadable: Some(format!("{e:#}")),
                ..Facts::default()
            }
        }
    };
    let (secrets, unreadable) = match secrets_of(&found.data) {
        Ok(s) => (s, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let secret = |name: &str| {
        secrets
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let mut facts = Facts::from_config(found.name.as_deref(), &cfg, secret, pinned.as_deref());
    facts.unreadable = unreadable;
    facts
}

/// This process's own instance's facts: its loaded config, and secrets
/// from its environment (the secrets file is loaded into it).
pub fn my_facts(cfg: &Config) -> Facts {
    let pinned = service::installed().map(|(_, ws)| ws);
    Facts::from_config(
        instance::current().as_deref(),
        cfg,
        crate::config_follow::secret_value,
        pinned.as_deref(),
    )
}

/// The other instances this one clashes with, for doctor and setup. Empty
/// when there are no other instances.
pub fn my_clashes(cfg: &Config) -> (Vec<Clash>, Vec<String>) {
    let Ok(places) = Places::here() else {
        return (Vec::new(), Vec::new());
    };
    let me = instance::current();
    let others: Vec<Found> = discover(&places)
        .into_iter()
        .filter(|f| f.name != me)
        .collect();
    let facts: Vec<Facts> = others.iter().map(facts_of).collect();
    let unreadable = facts
        .iter()
        .filter(|f| f.unreadable.is_some())
        .map(|f| instance::label(f.name.as_deref()).to_string())
        .collect();
    (clashes(&my_facts(cfg), &facts), unreadable)
}

/// One row of `instances list`.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub name: String,
    pub scope: &'static str,
    pub config: PathBuf,
    pub data: PathBuf,
    pub service: String,
    pub dashboard_port: Option<u16>,
    pub bot: Option<String>,
    pub current: bool,
}

fn row(found: &Found, current: bool, probe_service: bool) -> Row {
    let service = if !(cfg!(target_os = "linux") || cfg!(target_os = "macos")) {
        "none on this OS".into()
    } else if found.unit.is_none() {
        "not installed".into()
    } else if !probe_service {
        "installed".into()
    } else {
        match found.svc().status() {
            service::Status::Installed { running: true, .. } => "running".into(),
            service::Status::Installed { running: false, .. } => "stopped".into(),
            service::Status::NotInstalled => "not installed".into(),
            service::Status::Unsupported(_) => "installed".into(),
        }
    };
    let cfg = Config::from_file(&found.config).ok();
    let marker = std::fs::read(found.data.join("gateway").join("dashboard.json"))
        .ok()
        .and_then(|t| serde_json::from_slice::<crate::dashboard::cli::Marker>(&t).ok())
        .filter(|m| crate::health::pid_alive(m.pid) != Some(false))
        .map(|m| m.port);
    let dashboard_port = marker.or_else(|| {
        cfg.as_ref()
            .filter(|c| c.dashboard.enabled && c.dashboard.port != 0)
            .map(|c| c.dashboard.port)
    });
    let facts = if current {
        cfg.as_ref().map(my_facts)
    } else {
        Some(facts_of(found))
    };
    Row {
        name: found.label().to_string(),
        scope: match found.scope {
            Scope::User => "user",
            Scope::System => "system",
        },
        config: found.config.clone(),
        data: found.data.clone(),
        service,
        dashboard_port,
        bot: facts.and_then(|f| f.bot).map(|id| format!("bot id {id}")),
        current,
    }
}

pub fn rows(places: &Places, probe_service: bool) -> Vec<Row> {
    let me = instance::current();
    let my_scope = service::scope();
    discover(places)
        .iter()
        .map(|f| {
            let current = f.name == me && (f.scope == my_scope || !cfg!(target_os = "linux"));
            row(f, current, probe_service)
        })
        .collect()
}

pub async fn run(op: InstancesCmd) -> Result<()> {
    match op {
        InstancesCmd::List { json } => list(&Places::here()?, json, true),
        InstancesCmd::New {
            name,
            system,
            user,
            no_setup,
        } => new(&Places::here()?, &name, system, user, no_setup),
        InstancesCmd::Remove {
            name,
            purge,
            yes,
            system,
            user,
        } => remove(&Places::here()?, &name, purge, yes, system, user),
    }
}

pub fn list(places: &Places, json: bool, probe_service: bool) -> Result<()> {
    let rows = rows(places, probe_service);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no instances yet: `ferrule setup` sets up the default one");
        return Ok(());
    }
    for r in &rows {
        println!(
            "{}{} ({})",
            if r.current { "* " } else { "  " },
            r.name,
            r.scope
        );
        println!("    config     {}", r.config.display());
        println!("    data       {}", r.data.display());
        println!("    service    {}", r.service);
        println!(
            "    dashboard  {}",
            r.dashboard_port
                .map_or("any free port".into(), |p| format!("port {p}"))
        );
        println!("    bot        {}", r.bot.as_deref().unwrap_or("none"));
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        println!(
            "(no background service on this OS: run `ferrule --instance <name> gateway` yourself)"
        );
    }
    Ok(())
}

fn scope_for(system: bool, user: bool) -> Result<Scope> {
    service::decide_scope(cfg!(target_os = "linux"), service::is_root(), system, user)
        .map_err(|e| anyhow!(e))
}

pub fn new(places: &Places, name: &str, system: bool, user: bool, no_setup: bool) -> Result<()> {
    instance::validate(name).map_err(|e| anyhow!(e))?;
    let scope = scope_for(system, user)?;
    if let Some(f) = discover(places)
        .into_iter()
        .find(|f| f.name.as_deref() == Some(name) && f.scope == scope)
    {
        bail!(
            "there is an instance `{name}` already (config {}); `ferrule --instance {name} setup` changes it",
            f.config.display()
        );
    }
    let config = match scope {
        Scope::User => places.roots.config_file(Some(name)),
        Scope::System => Svc::new(Some(name), Scope::System).system_config(),
    };
    let dir = config.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    if !config.exists() {
        std::fs::write(&config, crate::setup::HEADER)
            .with_context(|| format!("writing {}", config.display()))?;
    }
    println!("instance `{name}`: config {}", config.display());
    let scope_flag = match (system, user) {
        (true, _) => Some("--system"),
        (_, true) => Some("--user"),
        _ => None,
    };
    let next = format!(
        "ferrule --instance {name} setup{}",
        scope_flag.map(|f| format!(" {f}")).unwrap_or_default()
    );
    if no_setup {
        println!("Next: `{next}` asks for its provider, bot and service.");
        return Ok(());
    }
    if !crate::setup::has_terminal() {
        println!("No terminal to ask on. Next, in a terminal: `{next}`.");
        return Ok(());
    }
    let exe = std::env::current_exe().context("finding this binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--instance", name, "setup"]).args(scope_flag);
    instance::child_for(&mut cmd, Some(name));
    let status = cmd
        .status()
        .context("starting setup for the new instance")?;
    if !status.success() {
        bail!("setup for `{name}` stopped; `{next}` picks up from there");
    }
    Ok(())
}

pub fn remove(
    places: &Places,
    name: &str,
    purge: bool,
    yes: bool,
    system: bool,
    user: bool,
) -> Result<()> {
    if name == "default" {
        bail!("`instances remove` is for named instances; the default's service goes with `ferrule setup` → Background service → Remove it");
    }
    instance::validate(name).map_err(|e| anyhow!(e))?;
    let mut matching: Vec<Found> = discover(places)
        .into_iter()
        .filter(|f| f.name.as_deref() == Some(name))
        .filter(|f| match (system, user) {
            (true, _) => f.scope == Scope::System,
            (_, true) => f.scope == Scope::User,
            _ => true,
        })
        .collect();
    let found = match matching.len() {
        0 => bail!("there's no instance `{name}`; `ferrule instances list` shows them"),
        1 => matching.remove(0),
        _ => bail!("`{name}` is both a user and a system instance; say which: --user or --system"),
    };
    if found.scope == Scope::System && !service::is_root() {
        bail!("`{name}` is a system instance; remove it as root: sudo ferrule instances remove {name}");
    }
    let pinned_ws = found
        .unit
        .as_deref()
        .and_then(service::installed_at)
        .map(|(_, ws)| ws);
    if found.unit.is_some() {
        found
            .svc()
            .uninstall()
            .with_context(|| format!("removing `{name}`'s service"))?;
        println!("removed `{name}`'s service and its update units");
    }
    let config_dir = found
        .config
        .parent()
        .context("config path has no parent")?
        .to_path_buf();
    let own_config_dir = match found.scope {
        Scope::User => places.roots.config.join(instance::dir_name(Some(name))),
        Scope::System => Path::new("/etc").join(instance::dir_name(Some(name))),
    };
    if !purge {
        println!(
            "kept its config ({}) and data ({})",
            found.config.display(),
            found.data.display()
        );
        println!("  delete them too: ferrule instances remove {name} --purge");
        return Ok(());
    }
    if let Some(ws) = &pinned_ws {
        if ws.starts_with(&found.data) || ws.starts_with(&config_dir) {
            bail!(
                "its workspace {} is inside what --purge deletes; move it first (a workspace is yours, never deleted)",
                ws.display()
            );
        }
    }
    if !yes {
        if !crate::setup::has_terminal() {
            bail!(
                "--purge deletes {} and {}; answer with --yes when there's no terminal to ask in",
                config_dir.display(),
                found.data.display()
            );
        }
        let typed = inquire::Text::new(&format!(
            "This deletes {} and {}. Type `{name}` to go on:",
            config_dir.display(),
            found.data.display()
        ))
        .prompt()
        .unwrap_or_default();
        if typed.trim() != name {
            bail!("nothing deleted");
        }
    }
    // Only the instance's own dirs, whatever a unit pinned.
    let mut gone = Vec::new();
    if config_dir == own_config_dir && config_dir.exists() {
        std::fs::remove_dir_all(&config_dir)
            .with_context(|| format!("deleting {}", config_dir.display()))?;
        gone.push(config_dir);
    } else if config_dir != own_config_dir {
        println!(
            "kept {}: it isn't `{name}`'s own config dir",
            found.config.display()
        );
    }
    let own_data = match found.scope {
        Scope::User => places.roots.data_dir(Some(name)),
        Scope::System => Svc::new(Some(name), Scope::System).system_data(),
    };
    if found.data == own_data && found.data.exists() {
        std::fs::remove_dir_all(&found.data)
            .with_context(|| format!("deleting {}", found.data.display()))?;
        gone.push(found.data.clone());
    }
    if found.scope == Scope::System {
        let state = Svc::new(Some(name), Scope::System)
            .system_home()
            .join("update");
        if state.exists() {
            std::fs::remove_dir_all(&state)?;
            gone.push(state);
        }
    }
    for dir in &gone {
        println!("deleted {}", dir.display());
    }
    if let Some(ws) = pinned_ws {
        println!("kept its workspace {}", ws.display());
    }
    if found.scope == Scope::System {
        println!("its system user stays; to delete it: userdel ferrule-{name}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: Option<&str>) -> Facts {
        Facts {
            name: name.map(str::to_string),
            ..Facts::default()
        }
    }

    #[test]
    fn a_bot_id_is_the_tokens_number() {
        assert_eq!(bot_id("123456:AAE-xyz"), Some("123456".into()));
        assert_eq!(bot_id(" 42:x \n"), Some("42".into()));
        for bad in ["", "abc:def", "123", "123:", ":abc"] {
            assert_eq!(bot_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn each_shared_thing_is_a_clash_naming_the_other() {
        let mut me = facts(Some("work"));
        me.bot = Some("111".into());
        me.port = Some(8765);
        me.workspace = Some("/srv/ws".into());
        me.ssh = Some("box:22/app".into());
        me.relay = Some("https://r.example".into());
        me.relay_key = Some("k1".into());
        me.accounts = vec![("whatsapp", "1110001".into())];
        let mut other = me.clone();
        other.name = None;
        other.relay_key = Some("k2".into());
        let found = clashes(&me, std::slice::from_ref(&other));
        let whats: Vec<_> = found.iter().map(|c| (c.what, c.severity)).collect();
        assert_eq!(
            whats,
            [
                ("telegram", Severity::Fail),
                ("dashboard", Severity::Fail),
                ("workspace", Severity::Fail),
                ("ssh", Severity::Fail),
                ("channel", Severity::Fail),
                ("relay", Severity::Fail),
            ]
        );
        assert!(found[4]
            .message
            .contains("WhatsApp number (phone number id 1110001)"));
        assert!(found.iter().all(|c| c.other == "default"));
        assert!(found[0].message.contains("id 111") && found[0].message.contains("`default`"));
        // Keys are compared, never shown.
        assert!(found.iter().all(|c| !c.message.contains("k1")));
        // The same relay with the same key only notes it.
        other.relay_key = me.relay_key.clone();
        let relay: Vec<_> = clashes(&me, &[other])
            .into_iter()
            .filter(|c| c.what == "relay")
            .collect();
        assert_eq!(relay.len(), 1);
        assert_eq!(relay[0].severity, Severity::Note);
    }

    #[test]
    fn different_things_and_unknowns_dont_clash() {
        let mut me = facts(None);
        me.bot = Some("1".into());
        me.port = Some(8765);
        me.workspace = Some("/a".into());
        me.ssh = Some("box:22/a".into());
        me.relay = Some("https://r1".into());
        me.accounts = vec![("whatsapp", "1".into())];
        let mut other = facts(Some("b"));
        other.accounts = vec![("whatsapp", "2".into()), ("matrix", "1".into())];
        other.bot = Some("2".into());
        other.port = Some(8766);
        other.workspace = Some("/b".into());
        other.ssh = Some("box:2222/a".into());
        other.relay = Some("https://r2".into());
        assert_eq!(clashes(&me, &[other]), []);
        // Nothing known about the other: nothing to say.
        assert_eq!(clashes(&me, &[facts(Some("c"))]), []);
        // Itself isn't another instance.
        assert_eq!(clashes(&me, std::slice::from_ref(&me)), []);
    }

    fn cfg(text: &str) -> Config {
        toml::from_str::<Config>(text).unwrap().finish().unwrap()
    }

    #[test]
    fn facts_come_from_the_config_and_that_instances_secrets() {
        let c = cfg(r#"
workspace = "ssh://Me@Box.example:2222/srv/app/"
[gateway]
telegram_token_env = "TG"
[dashboard]
port = 9000
[connections]
relay_url = "https://Relay.example/"
"#);
        let secret = |n: &str| match n {
            "TG" => Some("555:secret".to_string()),
            "FERRULE_RELAY_KEY" => Some("key".to_string()),
            _ => None,
        };
        let f = Facts::from_config(Some("w"), &c, secret, None);
        assert_eq!(f.bot.as_deref(), Some("555"));
        assert_eq!(f.port, Some(9000));
        assert_eq!(f.ssh.as_deref(), Some("box.example:2222/srv/app"));
        assert_eq!(f.workspace, None);
        assert_eq!(f.relay.as_deref(), Some("https://relay.example"));
        assert_eq!(f.relay_key.as_deref(), Some("key"));
        // A dashboard that's off, or on any free port, holds no port.
        let off = cfg("[dashboard]\nenabled = false\nport = 9000\n");
        assert_eq!(Facts::from_config(None, &off, |_| None, None).port, None);
        assert_eq!(
            Facts::from_config(None, &cfg(""), |_| None, None).port,
            None
        );
        // The service's pinned workspace, canonical.
        let dir = tempfile::tempdir().unwrap();
        let f = Facts::from_config(None, &cfg(""), |_| None, Some(dir.path()));
        assert_eq!(f.workspace, Some(dunce::canonicalize(dir.path()).unwrap()));
    }

    fn places(root: &Path) -> Places {
        Places {
            roots: Roots {
                config: root.join("config"),
                data: root.join("data"),
            },
            user_units: Some(root.join("units")),
            launchd: false,
            system: Some((root.join("etc"), root.join("sysunits"))),
        }
    }

    #[test]
    fn instances_are_found_by_config_and_by_unit() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        assert_eq!(discover(&p), []);
        for d in ["ferrule", "ferrule-work", "ferrule-Bad"] {
            std::fs::create_dir_all(p.roots.config.join(d)).unwrap();
            std::fs::write(p.roots.config.join(d).join("config.toml"), "").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("units")).unwrap();
        let pinned = dir.path().join("elsewhere.toml");
        let spec = service::Spec {
            exe: "/bin/ferrule".into(),
            workspace: "/ws".into(),
            config: pinned.clone(),
            path_env: "/bin".into(),
            instance: Some("unitonly".into()),
        };
        std::fs::write(
            dir.path().join("units/ferrule@unitonly.service"),
            service::systemd_unit(&spec),
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("etc/ferrule-sys")).unwrap();
        std::fs::write(dir.path().join("etc/ferrule-sys/config.toml"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("sysunits")).unwrap();
        let found = discover(&p);
        let names: Vec<_> = found
            .iter()
            .map(|f| (f.label().to_string(), f.scope))
            .collect();
        assert_eq!(
            names,
            [
                ("default".to_string(), Scope::User),
                ("unitonly".to_string(), Scope::User),
                ("work".to_string(), Scope::User),
                ("sys".to_string(), Scope::System),
            ]
        );
        assert_eq!(found[1].config, pinned);
        assert!(found[1].unit.is_some());
        assert_eq!(
            found[2].config,
            p.roots.config.join("ferrule-work/config.toml")
        );
        assert_eq!(found[2].data, p.roots.data.join("ferrule-work"));
        assert_eq!(found[3].data, Path::new("/var/lib/ferrule-sys/data"));
    }
}
