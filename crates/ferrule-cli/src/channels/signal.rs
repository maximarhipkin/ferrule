//! M39 §6: Signal on the CLI side: finding signal-cli (and Java, which the
//! JVM build needs), the adapter's settings from `[gateway.signal]`, and the
//! dashboard card. Ferrule never downloads or bundles signal-cli.

use super::card::{Field, Kind, Settings, Spec, Step};
use super::settings::Signal;
use anyhow::{bail, Result};
use ferrule_gateway::channels::files::Inbox;
use ferrule_gateway::channels::signal::{self as sg, Daemon, SignalConfig};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

/// Where the daemon ferrule starts writes its output.
pub fn state_dir() -> Option<PathBuf> {
    crate::config::data_dir()
        .ok()
        .map(|d| d.join("gateway").join("signal"))
}

/// The daemon's address: the one you run, else the one ferrule starts.
pub fn url(s: &Signal) -> String {
    s.url.as_deref().map_or_else(
        || format!("http://127.0.0.1:{}", s.port),
        |u| u.trim().trim_end_matches('/').to_string(),
    )
}

/// `name` on PATH (with Windows' `.exe`, `.bat` and `.cmd`).
pub fn on_path(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".bat", ".cmd"]
    } else {
        &[""]
    };
    let dirs = std::env::var_os("PATH")?;
    std::env::split_paths(&dirs).find_map(|dir| {
        exts.iter()
            .map(|e| dir.join(format!("{name}{e}")))
            .find(|p| p.is_file())
    })
}

/// The signal-cli to run: `signal_cli` (a path, or a name on PATH), else
/// `signal-cli` on PATH.
pub fn program(s: &Signal) -> Option<PathBuf> {
    match s
        .signal_cli
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(p) if Path::new(p).is_file() => Some(PathBuf::from(p)),
        Some(p) => on_path(p),
        None => on_path("signal-cli"),
    }
}

/// The adapter's settings. `workspace`: the gateway, which saves what
/// people send and starts the daemon when no `url` is set; `None`
/// (`ferrule tasks run-now`): only sends, through the gateway's daemon.
pub fn config(s: &Signal, workspace: Option<&Path>) -> Result<SignalConfig> {
    if let Err(e) = number(&s.account) {
        bail!("[gateway.signal] account: {e}");
    }
    let daemon = match (&s.url, workspace) {
        (None, Some(_)) => Some(Daemon {
            // A missing program is the channel's problem on the dashboard
            // and in doctor, not a gateway that won't start.
            program: program(s)
                .unwrap_or_else(|| PathBuf::from(s.signal_cli.as_deref().unwrap_or("signal-cli"))),
            port: s.port,
            log: state_dir().map(|d| d.join("daemon.log")),
        }),
        _ => None,
    };
    Ok(SignalConfig {
        account: s.account.trim().to_string(),
        url: url(s),
        daemon,
        inbox: workspace.map(|ws| Inbox::new(ws, s.max_file_mb)),
    })
}

/// A number in international form, `+` and digits.
pub fn number(n: &str) -> Result<(), String> {
    let n = n.trim();
    match n.strip_prefix('+') {
        Some(d) if (6..=15).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit()) => Ok(()),
        _ => Err(format!(
            "`{n}` isn't a number in international form like +972501234567"
        )),
    }
}

/// Who may DM it: a number, or an ACI uuid.
pub fn user_ok(u: &str) -> bool {
    number(u).is_ok() || uuid::Uuid::parse_str(u.trim()).is_ok()
}

/// What was found on this machine.
#[derive(Debug, Clone, Default)]
pub struct Found {
    /// signal-cli's path and version ("0.13.9").
    pub signal_cli: Option<(PathBuf, String)>,
    /// Why it couldn't be run, when it was found but failed.
    pub broken: Option<String>,
    /// Java's major version (the JVM build needs 21 or newer).
    pub java: Option<u32>,
}

impl Found {
    /// One line: what there is, and what's missing.
    pub fn summary(&self) -> String {
        match (&self.signal_cli, &self.broken) {
            (Some((p, v)), _) => format!("signal-cli {v} ({})", p.display()),
            (None, Some(why)) => why.clone(),
            (None, None) => "signal-cli isn't installed (or isn't on PATH)".into(),
        }
    }
}

/// Runs `program args` with a deadline (the JVM starts slowly): its
/// stdout and stderr together.
async fn output(program: &Path, args: &[&str], secs: u64) -> Result<String, String> {
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(secs), run).await {
        Err(_) => Err(format!(
            "`{} {}` took over {secs} s",
            program.display(),
            args.join(" ")
        )),
        Ok(Err(e)) => Err(format!("couldn't run {}: {e}", program.display())),
        Ok(Ok(o)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if o.status.success() {
                Ok(text)
            } else {
                Err(format!(
                    "`{} {}` failed ({}): {}",
                    program.display(),
                    args.join(" "),
                    o.status,
                    ferrule_gateway::health::clip(text.trim(), 300)
                ))
            }
        }
    }
}

/// `java -version`'s major: `"21.0.2"` is 21, `"1.8.0_392"` is 8.
pub fn java_major(text: &str) -> Option<u32> {
    let v = text.split('"').nth(1)?;
    let mut parts = v.split(['.', '_', '-', '+']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// `signal-cli --version`'s version: "signal-cli 0.13.9" is "0.13.9".
pub fn cli_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("signal-cli "))
        .map(|v| v.trim().to_string())
}

/// Looks for signal-cli (as `s` names it) and Java.
pub async fn detect(s: Option<&Signal>) -> Found {
    let mut found = Found::default();
    if let Some(java) = on_path("java") {
        if let Ok(t) = output(&java, &["-version"], 20).await {
            found.java = java_major(&t);
        }
    }
    let prog = match s {
        Some(s) => program(s),
        None => on_path("signal-cli"),
    };
    if let Some(p) = prog {
        match output(&p, &["--version"], 60).await {
            Ok(t) => {
                let v = cli_version(&t).unwrap_or_else(|| "(version unknown)".into());
                found.signal_cli = Some((p, v));
            }
            Err(why) => {
                let java = match found.java {
                    None => "; its JVM build needs Java 21 or newer, and none was found (https://adoptium.net)",
                    Some(v) if v < 21 => "; its JVM build needs Java 21 or newer",
                    _ => "",
                };
                found.broken = Some(format!("{why}{java}"));
            }
        }
    }
    found
}

/// The accounts signal-cli holds on this machine (`listAccounts`).
pub async fn accounts(program: &Path) -> Result<Vec<String>, String> {
    let t = output(program, &["listAccounts"], 60).await?;
    Ok(t.split_whitespace()
        .filter(|w| number(w).is_ok())
        .map(str::to_string)
        .collect())
}

/// Test: the daemon's version and groups; when ferrule starts the daemon
/// and it isn't up yet, that signal-cli is installed and holds the account.
pub async fn test(s: &Signal) -> Result<String, String> {
    number(&s.account)?;
    let u = url(s);
    match sg::probe(&u, &s.account).await {
        Ok(p) => {
            let mut said = p.summary();
            if s.allowed_users.is_empty() {
                said.push_str(" · no one is allowed yet: add your number, or pair with `ferrule setup` → Signal");
            }
            Ok(said)
        }
        Err(why) if s.url.is_some() => Err(why),
        Err(_) => {
            let found = detect(Some(s)).await;
            let Some((prog, v)) = &found.signal_cli else {
                return Err(format!(
                    "{}: install it (https://github.com/AsamK/signal-cli/releases), or set signal_cli to its path",
                    found.summary()
                ));
            };
            let accs = accounts(prog).await?;
            if !accs.iter().any(|a| a == s.account.trim()) {
                return Err(format!(
                    "signal-cli {v} has no account {}: link it (`signal-cli link -n ferrule`) or register it first{}",
                    s.account.trim(),
                    if accs.is_empty() {
                        String::new()
                    } else {
                        format!(" (it has {})", accs.join(", "))
                    }
                ));
            }
            Ok(format!(
                "signal-cli {v} holds {}; the gateway starts its daemon on port {}",
                s.account.trim(),
                s.port
            ))
        }
    }
}

fn probe(s: Settings) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        let sig: Signal = s.read()?;
        test(&sig).await
    })
}

fn check(t: &toml::Table) -> Result<(), String> {
    let s: Signal = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    number(&s.account)?;
    if let Some(u) = &s.url {
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err(format!(
                "the daemon's address is a URL like http://127.0.0.1:7583, not `{u}`"
            ));
        }
    }
    for u in &s.allowed_users {
        if !user_ok(u) {
            return Err(format!(
                "`{u}` isn't a number like +972501234567 (or a Signal uuid)"
            ));
        }
    }
    for g in &s.allowed_groups {
        if g.trim().is_empty() || g.contains(char::is_whitespace) || g.starts_with('+') {
            return Err(format!(
                "`{g}` isn't a group id: `ferrule setup` → Signal lists your groups with theirs"
            ));
        }
    }
    Ok(())
}

pub const SPEC: Spec = Spec {
    name: "signal",
    fields: &[
        Field {
            key: "account",
            label: "Number",
            hint: "the Signal number signal-cli is registered or linked on, +972501234567",
            kind: Kind::Text,
            optional: false,
        },
        Field {
            key: "url",
            label: "Daemon URL",
            hint: "only if you run signal-cli's daemon yourself (http://127.0.0.1:7583); empty: ferrule starts it",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "signal_cli",
            label: "signal-cli path",
            hint: "empty: the one on PATH",
            kind: Kind::Text,
            optional: true,
        },
        Field {
            key: "allowed_users",
            label: "Allowed numbers",
            hint: "+972501234567 — whose messages reach the agent; your own number here allows Note to Self",
            kind: Kind::List,
            optional: true,
        },
        Field {
            key: "allowed_groups",
            label: "Allowed groups",
            hint: "group ids where a mention reaches it (`ferrule setup` → Signal lists them)",
            kind: Kind::List,
            optional: true,
        },
    ],
    guide: &[
        Step {
            text: "Install signal-cli (the native Linux build, or the JVM build with Java 21+); ferrule doesn't bundle it",
            url: Some("https://github.com/AsamK/signal-cli/releases"),
        },
        Step {
            text: "Java 21 or newer, for the JVM build",
            url: Some("https://adoptium.net/temurin/releases/"),
        },
        Step {
            text: "Best: a separate number for the agent, registered with signal-cli (a captcha from signalcaptchas.org, then an SMS code)",
            url: Some("https://github.com/AsamK/signal-cli/wiki/Registration-with-captcha"),
        },
        Step {
            text: "Or link it to your phone like Signal Desktop: `signal-cli link -n ferrule`, then scan the code in Signal → Settings → Linked devices (`ferrule setup` → Signal shows it)",
            url: Some("https://github.com/AsamK/signal-cli/wiki/Linking-other-devices-(Provisioning)"),
        },
        Step {
            text: "Save and Test here: the gateway starts `signal-cli daemon` on 127.0.0.1 for you",
            url: None,
        },
    ],
    probe,
    check,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, toml::Value)]) -> toml::Table {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn the_card_checks_numbers_urls_and_groups() {
        let ok = table(&[("account", "+15550000001".into())]);
        assert!(check(&ok).is_ok());
        let bad = table(&[("account", "0501234567".into())]);
        assert!(check(&bad).unwrap_err().contains("international form"));
        let mut u = ok.clone();
        u.insert("url".into(), "127.0.0.1:7583".into());
        assert!(check(&u).unwrap_err().contains("a URL"));
        let mut who = ok.clone();
        who.insert(
            "allowed_users".into(),
            vec!["+15550000002", "0a0a0a0a-0000-4000-8000-000000000002"].into(),
        );
        assert!(check(&who).is_ok());
        who.insert("allowed_users".into(), vec!["max"].into());
        assert!(check(&who).unwrap_err().contains("isn't a number"));
        let mut g = ok.clone();
        g.insert("allowed_groups".into(), vec!["+1555"].into());
        assert!(check(&g).unwrap_err().contains("group id"));
    }

    #[test]
    fn versions_are_read_from_what_the_tools_print() {
        assert_eq!(
            java_major("openjdk version \"21.0.2\" 2024-01-16"),
            Some(21)
        );
        assert_eq!(java_major("java version \"1.8.0_392\""), Some(8));
        assert_eq!(java_major("nothing"), None);
        assert_eq!(
            cli_version("signal-cli 0.13.9\n").as_deref(),
            Some("0.13.9")
        );
        assert_eq!(
            cli_version("WARN something\nsignal-cli 0.12.8"),
            Some("0.12.8".into())
        );
    }

    #[test]
    fn ferrule_starts_the_daemon_only_in_the_gateway_and_only_without_a_url() {
        let s: Signal = toml::from_str("account = \"+15550000001\"").unwrap();
        assert_eq!(url(&s), "http://127.0.0.1:7583");
        let gw = config(&s, Some(Path::new("/ws"))).unwrap();
        assert_eq!(gw.daemon.unwrap().port, 7583);
        assert!(gw.inbox.is_some());
        assert!(config(&s, None).unwrap().daemon.is_none());
        let own: Signal =
            toml::from_str("account = \"+15550000001\"\nurl = \"http://10.0.0.2:8080/\"").unwrap();
        let c = config(&own, Some(Path::new("/ws"))).unwrap();
        assert!(c.daemon.is_none());
        assert_eq!(c.url, "http://10.0.0.2:8080");
        let bad: Signal = toml::from_str("account = \"0501\"").unwrap();
        assert!(config(&bad, None).is_err());
    }
}
