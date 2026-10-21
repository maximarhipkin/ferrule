//! M36 §6.3: a gateway whose config stopped reading doesn't exit into its
//! service's five-second restart loop. It runs on the last copy that read
//! (written after every good start) and tells the owner; with none, it
//! logs once and waits for the file to read again.

use crate::config::{self, Config};
use crate::service::{Scope, Svc};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(30);

pub fn path(data: &Path) -> PathBuf {
    data.join("gateway").join("config.last-good.toml")
}

/// The gateway's config: the file, or the last good copy when the file
/// doesn't read.
pub enum Loaded {
    Fresh(Config),
    /// The file (`path`) doesn't read, for `why`; the last good copy runs.
    LastGood {
        cfg: Config,
        path: PathBuf,
        why: String,
    },
}

impl Loaded {
    pub fn config(&self) -> &Config {
        match self {
            Loaded::Fresh(cfg) | Loaded::LastGood { cfg, .. } => cfg,
        }
    }
}

/// Reads the config; a good one is copied to [`path`]. No config at all is
/// still an error (`ferrule setup` first); a broken one without a copy
/// waits, checking every `poll`.
pub async fn load(data: &Path) -> Result<Loaded> {
    let loaded = load_from(data, config::config_path, POLL).await?;
    if let Loaded::LastGood { .. } = &loaded {
        let _ = ON_COPY.set(path(data));
    }
    Ok(loaded)
}

/// Set when this gateway started on the copy: every later
/// [`Config::load`] in the process reads it while the file doesn't.
static ON_COPY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The copy this process runs on, if it does.
pub fn on_copy() -> Option<&'static Path> {
    ON_COPY.get().map(PathBuf::as_path)
}

/// [`load`], finding the file with `find`.
async fn load_from(
    data: &Path,
    find: impl Fn() -> Result<Option<PathBuf>>,
    poll: Duration,
) -> Result<Loaded> {
    let mut logged = false;
    loop {
        let Some(file) = find()? else {
            return Err(crate::config::no_config());
        };
        match Config::from_file(&file) {
            Ok(cfg) => {
                keep(data, &file);
                return Ok(Loaded::Fresh(cfg));
            }
            Err(e) => {
                let why = format!("{e:#}");
                let copy = path(data);
                if copy.exists() {
                    match Config::from_file(&copy) {
                        Ok(cfg) => {
                            tracing::error!(
                                "{why}; running on the last good config ({})",
                                copy.display()
                            );
                            return Ok(Loaded::LastGood {
                                cfg,
                                path: file,
                                why,
                            });
                        }
                        Err(e) => tracing::debug!("the last good config doesn't read: {e:#}"),
                    }
                }
                if !logged {
                    tracing::error!(
                        "{why}. Ferrule waits for the file to read, checking every {} s.",
                        poll.as_secs().max(1)
                    );
                    logged = true;
                }
                tokio::time::sleep(poll).await;
            }
        }
    }
}

fn keep(data: &Path, file: &Path) {
    let copy = path(data);
    let done = std::fs::read_to_string(file)
        .map_err(anyhow::Error::from)
        .and_then(|text| {
            std::fs::create_dir_all(copy.parent().unwrap_or(data))?;
            crate::secrets::write_private(&copy, &text)
        });
    if let Err(e) = done {
        tracing::debug!("last good config not kept: {e:#}");
    }
}

/// What the owner hears when the gateway runs on the copy.
pub fn owner_line(path: &Path, why: &str) -> String {
    format!(
        "Your config {} doesn't read, so I'm running on the last one that did. \
         What's wrong: {}. When it reads again I switch to it by myself.",
        path.display(),
        ferrule_gateway::health::clip(why, 400)
    )
}

/// Under a service manager that restarts on exit.
pub fn supervised() -> bool {
    supervised_by(
        // M44: a managed bot starts itself again (`lifecycle::reexec`).
        crate::managed::on(),
        std::env::var("XPC_SERVICE_NAME").ok().as_deref(),
        std::env::var_os("INVOCATION_ID").is_some(),
        crate::instance::current().as_deref(),
        own_unit,
    )
}

/// Only this instance's own job counts: `XPC_SERVICE_NAME` and
/// `INVOCATION_ID` are passed on to whatever a job starts (VS Code's
/// terminal, a desktop session's), and an exit there would end a gateway
/// run by hand for good. launchd names the job it started; under systemd
/// the unit's main process must be this one.
fn supervised_by(
    managed: bool,
    xpc_service: Option<&str>,
    invocation: bool,
    instance: Option<&str>,
    own_unit: impl FnOnce(Option<&str>) -> bool,
) -> bool {
    let label = Svc::new(instance, Scope::User).launchd_label();
    managed || xpc_service == Some(label.as_str()) || (invocation && own_unit(instance))
}

/// The instance's systemd unit, the user's or the system's, runs this
/// process. A system service's gateway runs as its own user, not root, so
/// both are asked.
fn own_unit(instance: Option<&str>) -> bool {
    let me = std::process::id();
    cfg!(target_os = "linux")
        && [Scope::User, Scope::System]
            .into_iter()
            .any(|scope| Svc::new(instance, scope).main_pid() == Some(me))
}

/// While on the copy: once the file reads again, a supervised gateway
/// exits so its service starts it on the fixed file; one in a terminal
/// tells the owner to restart it.
pub fn watch(file: PathBuf, tell: impl Fn(String) + Send + 'static) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            if Config::from_file(&file).is_ok() {
                if supervised() {
                    tracing::info!("{} reads again; restarting onto it", file.display());
                    tell(format!(
                        "{} reads again; restarting onto it.",
                        file.display()
                    ));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if crate::managed::on() {
                        crate::lifecycle::request_restart();
                        return;
                    }
                    std::process::exit(0);
                }
                tell(format!(
                    "{} reads again. Restart ferrule to use it.",
                    file.display()
                ));
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_instances_own_job_or_unit_counts_as_supervised() {
        let none = |_: Option<&str>| false;
        // What a terminal passes on: VS Code's XPC name, a desktop
        // session's INVOCATION_ID, Terminal's `0`.
        let vscode = Some("application.com.microsoft.VSCode.81665391.81665397");
        assert!(!supervised_by(false, vscode, false, None, none));
        assert!(!supervised_by(false, Some("0"), true, None, none));
        // launchd names the job it started.
        assert!(supervised_by(
            false,
            Some("ai.ferrule.gateway"),
            false,
            None,
            none
        ));
        assert!(supervised_by(
            false,
            Some("ai.ferrule.gateway.work"),
            false,
            Some("work"),
            none
        ));
        assert!(!supervised_by(
            false,
            Some("ai.ferrule.gateway"),
            false,
            Some("work"),
            none
        ));
        // systemd: the unit's main process is this one.
        assert!(supervised_by(false, None, true, Some("work"), |i| i == Some("work")));
        // A managed bot always.
        assert!(supervised_by(true, None, false, None, none));
    }

    const GOOD: &str = "default_provider = \"a\"\n\n[providers.a]\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"K\"\nmodel = \"m\"\n";

    #[tokio::test]
    async fn a_broken_config_runs_on_the_last_good_copy_and_without_one_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("ferrule.toml");
        let data = tmp.path().join("data");
        let found = {
            let file = file.clone();
            move || Ok(Some(file.clone()))
        };
        let poll = Duration::from_millis(20);

        // No copy yet, and a broken file: it waits, then reads the fix.
        std::fs::write(&file, "this is [not toml").unwrap();
        let fix = {
            let file = file.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                std::fs::write(&file, GOOD).unwrap();
            })
        };
        let loaded = tokio::time::timeout(Duration::from_secs(20), load_from(&data, &found, poll))
            .await
            .expect("the fixed file read")
            .unwrap();
        fix.await.unwrap();
        assert!(matches!(loaded, Loaded::Fresh(_)));
        assert_eq!(std::fs::read_to_string(path(&data)).unwrap(), GOOD);

        std::fs::write(&file, "default_provider = [\"not\", \"a\", \"name\"]\n").unwrap();
        match load_from(&data, &found, poll).await.unwrap() {
            Loaded::LastGood { cfg, path, why } => {
                assert_eq!(cfg.default_provider.as_deref(), Some("a"));
                assert_eq!(path, file);
                assert!(why.contains("parsing"), "{why}");
                let line = owner_line(&path, &why);
                assert!(line.contains("running on the last one that did"), "{line}");
            }
            Loaded::Fresh(_) => panic!("the broken file read"),
        }
    }
}
