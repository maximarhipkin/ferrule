//! M34: a remote workspace over SSH (docs/ssh.md). `--workspace ssh:app`
//! (or `workspace = "ssh:app"` in the config) makes the shell and file
//! tools run on another machine; everything else stays local, anchored in
//! `<data>/ssh/<name>/local`.
//!
//! One link per process, shared by every agent it builds: the root, its
//! sub-agents and the gateway's tasks all follow the same remote.

use crate::config::{self, Config};
use anyhow::{anyhow, bail, Result};
use ferrule_ssh::{trust, DenySpec, Failure, Forward, Link, LinkOptions, Target};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// This process's remote workspace, once `workspace()` chose one.
static REMOTE: OnceLock<Remote> = OnceLock::new();

pub struct Remote {
    pub link: Arc<Link>,
    /// The workspace's AGENTS.md (or the like), read once at connect.
    pub baseline: Option<(String, String)>,
}

/// The remote workspace, if this process has one.
pub fn current() -> Option<&'static Remote> {
    REMOTE.get()
}

/// What the owner asked for: `--workspace`, else the config's
/// `workspace`, else `.`.
fn spec(arg: Option<String>, cfg: Option<&Config>) -> String {
    arg.or_else(|| cfg.and_then(|c| c.workspace.clone()))
        .unwrap_or_else(|| ".".into())
}

/// The local directory the agent is built on. For a remote workspace it
/// is the local anchor; the link is connected first. `lenient` (the
/// gateway) starts anyway when the host is only unreachable: the link
/// reconnects on the next call, and `/status` says it's down.
pub async fn workspace(arg: Option<String>, lenient: bool) -> Result<PathBuf> {
    let loaded = Config::load().ok().map(|(c, _)| c);
    let spec = spec(arg, loaded.as_ref());
    if !ferrule_ssh::is_remote(&spec) {
        return Ok(PathBuf::from(spec));
    }
    let cfg = match loaded {
        Some(c) => c,
        None => Config::load()?.0,
    };
    let target = Target::parse(&spec, &cfg.ssh).map_err(|e| anyhow!(e))?;
    let anchor = anchor(&target)?;
    if target.is_loopback() {
        eprintln!(
            "ferrule: warning: {} is this machine. Over SSH, ferrule's local sandbox doesn't apply; \
             the remote account is the boundary.",
            target.label
        );
    }
    let link = Link::new(target, options(&cfg, crate::shared_broker(&cfg)?)?);
    let baseline = match link.connect().await {
        Ok(_) => baseline(&link).await,
        Err(e) if lenient && !stops(&link) => {
            eprintln!("ferrule: {e}\nferrule: starting anyway; the link retries on each call.");
            None
        }
        Err(e) => bail!("{e}"),
    };
    for note in notes(&cfg) {
        eprintln!("ferrule: {}: {note}", link.target().label);
    }
    let _ = REMOTE.set(Remote { link, baseline });
    Ok(anchor)
}

/// Whether the link failed for a reason retrying can't fix: an unknown or
/// changed host key, or a refused login.
fn stops(link: &Link) -> bool {
    matches!(
        link.last_failure(),
        Some(Failure::UnknownHost | Failure::HostKeyChanged { .. } | Failure::Auth)
    )
}

/// `<data>/ssh/<name>/local`: todos, the diary and hook trust for a
/// remote workspace.
pub fn anchor(target: &Target) -> Result<PathBuf> {
    let dir = config::data_dir()?
        .join("ssh")
        .join(&target.name)
        .join("local");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// ferrule's own known_hosts, which `ferrule ssh trust` writes.
pub fn known_hosts() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("ssh").join("known_hosts"))
}

/// The link's settings from the config: both known_hosts, the sandbox's
/// read denies (resolved on the remote), and the credential proxy's
/// forward when `[secrets]` has entries.
pub fn options(
    cfg: &Config,
    broker: Option<&'static ferrule_proxy::Broker>,
) -> Result<LinkOptions> {
    let path = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
    let forward = broker.map(|broker| Forward {
        port: broker.addr().port(),
        env: Arc::new(move || broker.child_env()),
    });
    Ok(LinkOptions {
        known_hosts: Some(known_hosts()?),
        user_known_hosts: ferrule_ssh::default_user_known_hosts(),
        deny: DenySpec {
            default: cfg.sandbox.deny_default_reads,
            deny_read: cfg.sandbox.deny_read.iter().map(path).collect(),
            allow_read: cfg.sandbox.allow_read.iter().map(path).collect(),
        },
        forward,
        extra_args: Vec::new(),
    })
}

/// The first non-empty context baseline in the remote workspace, capped
/// as locally.
async fn baseline(link: &Link) -> Option<(String, String)> {
    for name in ferrule_core::baseline::BASELINE_FILES {
        // Not through a symlink: that could point into a read deny.
        let cmd = format!("[ -f '{name}' ] && [ ! -L '{name}' ] && cat -- '{name}' 2>/dev/null");
        let Ok((0, text)) =
            ferrule_ssh::tools::run_remote(link, &cmd, Duration::from_secs(30)).await
        else {
            continue;
        };
        let text = text
            .chars()
            .take(ferrule_core::baseline::BASELINE_MAX_CHARS)
            .collect::<String>();
        if !text.trim().is_empty() {
            return Some((name.to_string(), text));
        }
    }
    None
}

/// What a remote workspace turns off, for the owner at startup and the
/// model in the prompt (docs/m34-ssh-local.md §6).
pub fn notes(cfg: &Config) -> Vec<String> {
    let mut out = vec!["code_search and the repo map are off (they index local files)".to_string()];
    if cfg.agent.lint == config::LintMode::Auto {
        out.push("per-edit lint is off (the linters run locally)".into());
    }
    if cfg.agent.auto_commit {
        out.push("auto-commit is off (it commits a local tree)".into());
    }
    out
}

/// The system prompt's line about where the workspace is.
pub fn prompt_note(link: &Link, cfg: &Config) -> String {
    let t = link.target();
    let mut s = format!(
        "Workspace: {} — the directory {} on the remote host {}. The shell, read_file, write_file, \
         edit_file and list_dir tools act there, over SSH; paths are relative to it. \
         MCP servers and other tools run on the local machine.",
        t.label,
        t.path,
        t.host
    );
    for note in notes(cfg) {
        s.push_str(&format!(" Note: {note}."));
    }
    s
}

/// `/status` and the dashboard: one line from the link's last state.
pub fn status_lines() -> Vec<String> {
    current()
        .map(|r| vec![r.link.status_line()])
        .unwrap_or_default()
}

/// The gateway is degraded while the link is down.
pub fn probe() -> Option<String> {
    let r = current()?;
    r.link.last_error().map(|e| {
        format!(
            "the remote workspace {} is down: {e}",
            r.link.target().label
        )
    })
}

/// `ferrule ssh …`
#[derive(clap::Subcommand)]
pub enum SshCmd {
    /// The configured remote workspaces ([ssh.<name>]) and whether their
    /// host keys are known
    List,
    /// First contact: fetch the host's keys, show their fingerprints, and
    /// trust them on yes (or when one matches --fingerprint)
    Trust {
        /// `name` of an [ssh.<name>] block, `ssh:name`, or `ssh://…`
        target: String,
        /// Trust only a key with this fingerprint (`SHA256:…`), without
        /// asking: for scripted installs
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Connect and check everything: host key, login, the shell, the
    /// workspace, the credential proxy's forward
    Test {
        /// `name` of an [ssh.<name>] block, `ssh:name`, or `ssh://…`
        target: String,
    },
}

pub async fn run(op: SshCmd) -> Result<()> {
    let (cfg, _) = Config::load()?;
    match op {
        SshCmd::List => {
            if cfg.ssh.is_empty() {
                println!(
                    "no [ssh.<name>] blocks (add one with `ferrule setup` → Remote workspace)"
                );
            }
            for (name, host) in &cfg.ssh {
                let t = match Target::from_config(name, host) {
                    Ok(t) => t,
                    Err(e) => {
                        println!("ssh:{name}  ✗ {e}");
                        continue;
                    }
                };
                let default = cfg.workspace.as_deref() == Some(t.label.as_str());
                let key = match trust::resolve(&t).await {
                    Ok(r) if trust::is_known(&t, &r.known_as, &known_files()?).await => {
                        "host key known"
                    }
                    Ok(_) => "host key NOT known: `ferrule ssh trust` it",
                    Err(_) => "can't resolve (ssh -G failed)",
                };
                println!(
                    "{}{}  {}  · {key}",
                    t.label,
                    if default { " (default workspace)" } else { "" },
                    t.describe()
                );
            }
        }
        SshCmd::Trust {
            target,
            fingerprint,
        } => {
            let t = target_arg(&target, &cfg)?;
            let msg = trust_target(&t, fingerprint.as_deref(), ask_at_terminal).await?;
            println!("{msg}");
        }
        SshCmd::Test { target } => {
            let t = target_arg(&target, &cfg)?;
            let mut failed = false;
            for c in check(&t, &cfg).await? {
                failed |= c.mark == Mark::Fail;
                println!("{} {}", c.mark.symbol(), c.text);
                if let Some(h) = c.hint {
                    println!("    → {h}");
                }
            }
            if failed {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// `app`, `ssh:app` or `ssh://…`.
fn target_arg(arg: &str, cfg: &Config) -> Result<Target> {
    let spec = if ferrule_ssh::is_remote(arg) {
        arg.to_string()
    } else {
        format!("ssh:{arg}")
    };
    Target::parse(&spec, &cfg.ssh).map_err(|e| anyhow!(e))
}

/// Every known_hosts ssh reads for ferrule: the owner's, then ferrule's.
fn known_files() -> Result<Vec<PathBuf>> {
    let mut files = ferrule_ssh::default_user_known_hosts();
    files.push(known_hosts()?);
    Ok(files)
}

fn ask_at_terminal(question: &str) -> Option<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// First contact (docs/m34-ssh-local.md §4): the scanned keys are shown and
/// written to ferrule's known_hosts only on a yes, or when one matches
/// `fingerprint`. A host already known is left alone; one whose keys no
/// longer match is a hard stop. `ask` gets the question, and `None` when
/// nobody can answer it.
pub async fn trust_target(
    t: &Target,
    fingerprint: Option<&str>,
    ask: impl Fn(&str) -> Option<bool>,
) -> Result<String> {
    let (resolved, keys) = trust::scan(t).await.map_err(|e| anyhow!(e))?;
    let files = known_files()?;
    let known = trust::known_keys(t, &resolved.known_as, &files).await;
    if !known.is_empty() {
        let matched = known
            .iter()
            .find(|(_, kind, blob)| keys.iter().any(|k| trust::same_key(k, kind, blob)));
        return match matched {
            Some((file, kind, _)) => Ok(format!(
                "{} is already known ({kind} in {}); nothing to do.",
                resolved.known_as,
                crate::setup::tilde(file)
            )),
            None => {
                let now: Vec<String> = keys
                    .iter()
                    .map(|k| format!("{} {}", k.key_type, k.fingerprint))
                    .collect();
                bail!(
                    "STOP: {} has keys on file ({}) that don't match any it presents now ({}). \
                     This can be a machine-in-the-middle attack, or the server was reinstalled. \
                     ferrule never replaces a key: confirm the new one with the server's admin \
                     (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` there), then remove the old entry \
                     with `ssh-keygen -R '{}' -f <that file>` and run this again.",
                    resolved.known_as,
                    known
                        .iter()
                        .map(|(f, _, _)| crate::setup::tilde(f))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", "),
                    now.join(", "),
                    resolved.known_as
                );
            }
        };
    }
    let chosen = match fingerprint {
        Some(fp) => {
            let m = trust::matching(&keys, fp);
            if m.is_empty() {
                bail!(
                    "{}:{} presents no key with fingerprint {fp}; it has: {}. Nothing was trusted.",
                    resolved.hostname,
                    resolved.port,
                    keys.iter()
                        .map(|k| format!("{} {}", k.key_type, k.fingerprint))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            m
        }
        None => {
            println!(
                "{}:{} ({}) presents these host keys:",
                resolved.hostname, resolved.port, t.label
            );
            for k in &keys {
                println!("  {:<22} {}", k.key_type, k.fingerprint);
            }
            println!(
                "Compare them with the server's own, run there:\n  ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub"
            );
            match ask("Do they match, and trust this host?") {
                Some(true) => keys,
                Some(false) => bail!("not trusted; nothing was written"),
                None => bail!(
                    "no terminal to confirm the fingerprint on: run `ferrule ssh trust {} --fingerprint SHA256:…` \
                     with the fingerprint the server's admin gave you",
                    t.label
                ),
            }
        }
    };
    let file = known_hosts()?;
    trust::add(&file, &chosen)?;
    Ok(format!(
        "trusted {} ({}) in {}",
        resolved.known_as,
        chosen
            .iter()
            .map(|k| k.fingerprint.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        crate::setup::tilde(&file)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Ok,
    Note,
    Warn,
    Fail,
}

impl Mark {
    fn symbol(self) -> &'static str {
        match self {
            Mark::Ok => "✓",
            Mark::Note => "·",
            Mark::Warn => "!",
            Mark::Fail => "✗",
        }
    }
}

pub struct Check {
    pub mark: Mark,
    pub text: String,
    pub hint: Option<String>,
}

fn line(mark: Mark, text: impl Into<String>, hint: Option<String>) -> Check {
    Check {
        mark,
        text: text.into(),
        hint,
    }
}

/// The boundary, said wherever a remote workspace is set up or checked.
pub const BOUNDARY: &str =
    "the remote ACCOUNT is the boundary: ferrule's sandbox doesn't reach there, \
     a command can do whatever that user can. Use a dedicated low-privilege user that owns only \
     the workspace (no sudo, no keys to other hosts).";

/// Doctor's and `ferrule ssh test`'s checks for one target (§9): host
/// key, login, the shell, the workspace, the proxy's forward. A fresh
/// link, so nothing is cached from a running agent.
pub async fn check(t: &Target, cfg: &Config) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    if t.is_loopback() {
        out.push(line(
            Mark::Warn,
            format!(
                "{} is this machine: over SSH ferrule's local sandbox is bypassed, not extended",
                t.label
            ),
            Some("use a local workspace, or a remote host".into()),
        ));
    }
    let resolved = match trust::resolve(t).await {
        Ok(r) => r,
        Err(e) => {
            out.push(line(
                Mark::Fail,
                e,
                Some(
                    "is `ssh` installed? (FERRULE_SSH or [ssh.<name>] ssh = … names another)"
                        .into(),
                ),
            ));
            return Ok(out);
        }
    };
    // A throwaway listener stands in for the proxy: only whether the
    // server accepts the forward is tested.
    let stand_in = if cfg.secrets.is_empty() {
        None
    } else {
        std::net::TcpListener::bind("127.0.0.1:0").ok()
    };
    let mut opts = options(cfg, None)?;
    if let Some(l) = &stand_in {
        opts.forward = Some(Forward {
            port: l.local_addr()?.port(),
            env: Arc::new(Vec::new),
        });
    }
    let link = Link::new(t.clone(), opts);
    let remote = match link.connect().await {
        Ok(r) => r,
        Err(e) => {
            let hint = match link.last_failure() {
                Some(Failure::UnknownHost) => Some(format!("`ferrule ssh trust {}`", t.label)),
                Some(Failure::Auth) => {
                    Some("`ssh-add` the key, or set identity_file; then `ferrule ssh test`".into())
                }
                Some(Failure::Unreachable(_)) => Some(format!(
                    "is {}:{} up and reachable from here?",
                    resolved.hostname, resolved.port
                )),
                _ => None,
            };
            out.push(line(Mark::Fail, e, hint));
            return Ok(out);
        }
    };
    out.push(line(
        Mark::Ok,
        format!(
            "{}: reachable at {}:{}, host key known, login ok",
            t.label, resolved.hostname, resolved.port
        ),
        None,
    ));
    match ferrule_ssh::tools::run_remote(&link, "uname -sr", Duration::from_secs(30)).await {
        Ok((0, text)) => out.push(line(
            Mark::Ok,
            format!("shell: sh works ({})", text.trim()),
            None,
        )),
        Ok((code, text)) => out.push(line(
            Mark::Fail,
            format!("shell: `uname -sr` exited {code}: {}", text.trim()),
            Some("the account's shell must run POSIX sh".into()),
        )),
        Err(e) => out.push(line(Mark::Fail, format!("shell: {e}"), None)),
    }
    if remote.writable {
        out.push(line(
            Mark::Ok,
            format!("workspace: {} (writable)", remote.workspace),
            None,
        ));
    } else {
        out.push(line(
            Mark::Warn,
            format!(
                "workspace: {} is not writable for this account",
                remote.workspace
            ),
            Some("the write tools will fail; fine for a read-only look".into()),
        ));
    }
    if stand_in.is_some() {
        match link.forward_note() {
            None => out.push(line(
                Mark::Ok,
                "credential proxy: the server accepts the forward",
                None,
            )),
            Some(n) => out.push(line(
                Mark::Warn,
                format!("credential proxy: {n}"),
                Some("`AllowTcpForwarding remote` (or yes) in the server's sshd_config".into()),
            )),
        }
    } else {
        out.push(line(
            Mark::Note,
            "credential proxy: off (no [secrets])",
            None,
        ));
    }
    out.push(line(Mark::Note, BOUNDARY, None));
    link.disconnect().await;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_wins_then_the_config_then_dot() {
        let cfg: Config = toml::from_str("workspace = \"ssh:app\"").unwrap();
        assert_eq!(spec(Some("/w".into()), Some(&cfg)), "/w");
        assert_eq!(spec(None, Some(&cfg)), "ssh:app");
        assert_eq!(spec(None, None), ".");
    }

    #[test]
    fn notes_follow_what_the_config_turned_on() {
        let mut cfg: Config = toml::from_str("").unwrap();
        cfg.agent.lint = config::LintMode::Off;
        cfg.agent.auto_commit = false;
        assert_eq!(notes(&cfg).len(), 1);
        cfg.agent.lint = config::LintMode::Auto;
        cfg.agent.auto_commit = true;
        let n = notes(&cfg).join("\n");
        assert!(n.contains("lint") && n.contains("auto-commit"), "{n}");
    }

    #[test]
    fn the_prompt_names_the_remote_and_never_the_key() {
        let cfg: Config = toml::from_str(
            "[ssh.app]\nhost = \"app.example.com\"\nuser = \"ferrule\"\npath = \"/srv/app\"\nidentity_file = \"/keys/secret_ed25519\"\n",
        )
        .unwrap();
        let t = Target::parse("ssh:app", &cfg.ssh).unwrap();
        let link = Link::new(t, LinkOptions::default());
        let note = prompt_note(&link, &cfg);
        assert!(
            note.contains("ssh:app") && note.contains("/srv/app"),
            "{note}"
        );
        assert!(note.contains("app.example.com"), "{note}");
        assert!(!note.contains("secret_ed25519"), "{note}");
    }

    #[test]
    fn the_sandbox_denies_reach_the_link() {
        let cfg: Config = toml::from_str(
            "[sandbox]\ndeny_read = [\"~/prod.env\"]\nallow_read = [\"~/.kube\"]\ndeny_default_reads = true\n",
        )
        .unwrap();
        let o = options(&cfg, None).unwrap();
        assert!(o.deny.default);
        assert_eq!(o.deny.deny_read, ["~/prod.env"]);
        assert_eq!(o.deny.allow_read, ["~/.kube"]);
        assert!(o.known_hosts.unwrap().ends_with("ssh/known_hosts"));
        assert!(o.forward.is_none());
    }
}
