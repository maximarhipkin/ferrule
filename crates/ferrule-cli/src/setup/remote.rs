//! The "Remote workspace (SSH)" step of `ferrule setup` (M34,
//! docs/ssh.md): where the host is, who logs in and how, the host key's
//! fingerprint confirmed by the owner, and a test run before anything is
//! saved. ferrule never reads the private key: it only passes the path to
//! ssh, which asks for any passphrase itself.

use super::{info, ok, put, table, warn, Target};
use crate::config;
use crate::remote::{self, Mark};
use anyhow::{bail, Result};
use ferrule_ssh::HostConfig;
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, InquireError, Select, Text};
use std::path::PathBuf;

pub(super) fn summary(cfg: &config::Config) -> String {
    match (cfg.ssh.len(), cfg.workspace.as_deref()) {
        (0, _) => "none".into(),
        (n, Some(w)) if ferrule_ssh::is_remote(w) => format!("{n} · default {w}"),
        (1, _) => "1 host".into(),
        (n, _) => format!("{n} hosts"),
    }
}

pub(super) async fn step(t: &mut Target) -> Result<()> {
    info("The agent can work in a directory on another machine: its shell and file tools run there, over your own ssh.");
    info(format!("Keep in mind: {}", remote::BOUNDARY));
    let cfg = t.config()?;
    let names: Vec<String> = cfg.ssh.keys().cloned().collect();
    let pick = if names.is_empty() {
        None
    } else {
        let mut items: Vec<String> = names.iter().map(|n| format!("ssh:{n}")).collect();
        items.push("Add another".into());
        let i = Select::new("Which one?", items).raw_prompt()?.index;
        names.get(i).cloned()
    };
    match pick {
        Some(name) => edit(t, &name).await,
        None => add(t).await,
    }
}

async fn edit(t: &mut Target, name: &str) -> Result<()> {
    let cfg = t.config()?;
    let spec = format!("ssh:{name}");
    let is_default = cfg.workspace.as_deref() == Some(spec.as_str());
    let mut items = vec!["Test it", "Trust its host key"];
    items.push(if is_default {
        "Stop using it as the default workspace"
    } else {
        "Make it the default workspace"
    });
    items.push("Remove it");
    let choice = Select::new(&spec, items).prompt()?;
    let target = ferrule_ssh::Target::parse(&spec, &cfg.ssh).map_err(anyhow::Error::msg)?;
    match choice {
        "Test it" => {
            test(&target, &cfg).await?;
        }
        "Trust its host key" => {
            info(remote::trust_target(&target, None, confirm).await?);
        }
        "Make it the default workspace" => {
            put(t.root(), "workspace", spec.as_str());
            t.save()?;
            ok(format!("`ferrule run`/`chat` and the gateway now work in {spec}; `--workspace .` overrides it"));
        }
        "Stop using it as the default workspace" => {
            t.root().remove("workspace");
            t.save()?;
            ok("the default workspace is the current directory again");
        }
        _ => {
            if !Confirm::new(&format!(
                "Remove [ssh.{name}]? (its host key stays in the known_hosts file)"
            ))
            .with_default(false)
            .prompt()?
            {
                return Ok(());
            }
            table(t.root(), &["ssh"])?.remove(name);
            if table(t.root(), &["ssh"])?.is_empty() {
                t.root().remove("ssh");
            }
            if is_default {
                t.root().remove("workspace");
            }
            t.save()?;
            ok(format!("{spec} removed"));
        }
    }
    Ok(())
}

async fn add(t: &mut Target) -> Result<()> {
    let cfg = t.config()?;
    let taken: Vec<String> = cfg.ssh.keys().cloned().collect();
    let name = Text::new("A short name for it (used as ssh:<name>)")
        .with_default(if taken.is_empty() { "app" } else { "" })
        .with_validator(move |s: &str| -> Result<Validation, CustomUserError> {
            Ok(if !valid_name(s) {
                Validation::Invalid("letters, digits, - and _ only".into())
            } else if taken.iter().any(|n| n == s) {
                Validation::Invalid("that name is taken; pick it from the list instead".into())
            } else {
                Validation::Valid
            })
        })
        .prompt()?;
    let host = Text::new("Host (a name, an address, or a Host alias from ~/.ssh/config)")
        .with_validator(|s: &str| -> Result<Validation, CustomUserError> {
            Ok(if s.trim().is_empty() || s.contains(char::is_whitespace) {
                Validation::Invalid("one host name, no spaces".into())
            } else {
                Validation::Valid
            })
        })
        .prompt()?
        .trim()
        .to_string();
    let user = Text::new("User on it (empty: what ssh would use)")
        .with_help_message("best a dedicated one, owning only the workspace")
        .prompt()?
        .trim()
        .to_string();
    let port = Text::new("Port")
        .with_default("22")
        .with_validator(|s: &str| -> Result<Validation, CustomUserError> {
            Ok(match s.trim().parse::<u16>() {
                Ok(p) if p > 0 => Validation::Valid,
                _ => Validation::Invalid("a port number".into()),
            })
        })
        .prompt()?
        .trim()
        .parse::<u16>()?;
    let path = Text::new("The workspace directory on it (absolute, or ~/…)")
        .with_validator(|s: &str| -> Result<Validation, CustomUserError> {
            let s = s.trim();
            Ok(if s.starts_with('/') || s == "~" || s.starts_with("~/") {
                Validation::Valid
            } else {
                Validation::Invalid("absolute (/srv/app) or under the home (~/app)".into())
            })
        })
        .prompt()?
        .trim()
        .to_string();
    let how = Select::new(
        "How does ssh log in?",
        vec![
            "ssh-agent or my ssh config, as a plain `ssh` would",
            "a key file (ferrule passes its path to ssh; it never reads the key)",
        ],
    )
    .raw_prompt()?
    .index;
    let identity_file = if how == 1 {
        let default = dirs::home_dir()
            .map(|h| h.join(".ssh").join("id_ed25519"))
            .unwrap_or_default();
        let p = Text::new("Path to the private key")
            .with_default(&super::tilde(&default))
            .prompt()?;
        Some(expand(p.trim()))
    } else {
        None
    };
    let hc = HostConfig {
        host,
        user: (!user.is_empty()).then_some(user),
        port: (port != 22).then_some(port),
        path,
        identity_file,
        ..HostConfig::default()
    };
    let target = ferrule_ssh::Target::from_config(&name, &hc).map_err(anyhow::Error::msg)?;
    if target.is_loopback() {
        warn("that's this machine: over SSH, ferrule's local sandbox is bypassed rather than extended");
    }
    match remote::trust_target(&target, None, confirm).await {
        Ok(msg) => ok(msg),
        Err(e) => {
            warn(format!("{e:#}"));
            if !Confirm::new("Save it anyway, to trust it later with `ferrule ssh trust`?")
                .with_default(false)
                .prompt()?
            {
                bail!("nothing was saved");
            }
        }
    }
    let passed = test(&target, &cfg).await?;
    if !passed
        && !Confirm::new("The test failed. Save it anyway?")
            .with_default(false)
            .prompt()?
    {
        bail!("nothing was saved");
    }
    write(t, &name, &hc)?;
    ok(format!("saved as [ssh.{name}]"));
    let spec = format!("ssh:{name}");
    if Confirm::new(&format!(
        "Make {spec} the default workspace (for run, chat and the gateway)?"
    ))
    .with_default(false)
    .prompt()?
    {
        put(t.root(), "workspace", spec.as_str());
        t.save()?;
        ok(format!("default workspace: {spec}"));
    } else {
        info(format!("Use it with `--workspace {spec}`."));
    }
    Ok(())
}

/// A trust question on the terminal; Esc is a no.
fn confirm(question: &str) -> Option<bool> {
    match Confirm::new(question).with_default(false).prompt() {
        Ok(yes) => Some(yes),
        Err(InquireError::OperationCanceled) => Some(false),
        Err(_) => None,
    }
}

/// `ferrule ssh test`'s checks, printed the way setup prints; whether
/// nothing failed.
async fn test(target: &ferrule_ssh::Target, cfg: &config::Config) -> Result<bool> {
    info(format!("Testing {} …", target.describe()));
    let mut passed = true;
    for c in remote::check(target, cfg).await? {
        if c.text == remote::BOUNDARY {
            continue;
        }
        match c.mark {
            Mark::Ok => ok(&c.text),
            Mark::Note => info(&c.text),
            Mark::Warn => warn(&c.text),
            Mark::Fail => {
                passed = false;
                println!("  ✗ {}", c.text);
            }
        }
        if let Some(h) = c.hint {
            info(format!("  → {h}"));
        }
    }
    Ok(passed)
}

fn write(t: &mut Target, name: &str, hc: &HostConfig) -> Result<()> {
    let tbl = table(t.root(), &["ssh", name])?;
    put(tbl, "host", hc.host.as_str());
    if let Some(u) = &hc.user {
        put(tbl, "user", u.as_str());
    }
    if let Some(p) = hc.port {
        put(tbl, "port", i64::from(p));
    }
    put(tbl, "path", hc.path.as_str());
    if let Some(k) = &hc.identity_file {
        put(tbl, "identity_file", k.to_string_lossy().as_ref());
    }
    t.save()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_block_reads_back_as_the_same_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_provider = \"x\"\n").unwrap();
        let mut t = Target::load(path).unwrap();
        let hc = HostConfig {
            host: "build.example.com".into(),
            user: Some("ferrule".into()),
            port: Some(2222),
            path: "/srv/app".into(),
            identity_file: Some(PathBuf::from("/home/me/.ssh/id_ed25519")),
            ..HostConfig::default()
        };
        write(&mut t, "app", &hc).unwrap();
        put(t.root(), "workspace", "ssh:app");
        t.save().unwrap();
        let cfg = t.config().unwrap();
        assert_eq!(cfg.ssh.get("app"), Some(&hc));
        assert_eq!(cfg.workspace.as_deref(), Some("ssh:app"));
        assert_eq!(summary(&cfg), "1 · default ssh:app");
        assert!(valid_name("app-2_b") && !valid_name("a b") && !valid_name(""));
    }
}
