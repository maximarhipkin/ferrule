//! `update_check` and `update` from the chat: the same check and the same
//! signed, verified install `ferrule update` does, started by the owner's
//! tap instead of a terminal.

use super::promise::{self, Kind};
use crate::update::apply::{self, Apply, Outcome, Want};
use crate::update::release::{Release, Source};
use crate::update::state::{self, Request, State};
use crate::update::{others_running, someone_elses, state_dir};
use ferrule_trust::ChatRef;
use std::path::PathBuf;
use std::time::Duration;

/// How long an install waits for running turns.
const IDLE: Duration = Duration::from_secs(600);

/// What an install did.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The new binary is in place: restart into `exe` when the asking turn
    /// is over.
    Restarting {
        from: String,
        to: String,
        exe: PathBuf,
    },
    /// Nothing to restart; this is what to tell the owner.
    Said(String),
}

/// The real thing, from the config: this binary, this data dir.
pub fn real_apply(data: PathBuf) -> Result<Apply<'static>, String> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| dunce::canonicalize(e).ok())
        .ok_or("I can't tell which file runs me")?;
    let mut apply = Apply::new_defaults(Source::github(), exe, data.clone(), state_dir(&data));
    if let Ok((cfg, _)) = crate::config::Config::load() {
        apply.channel = cfg.update.channel;
    }
    apply.idle_for = IDLE;
    Ok(apply)
}

pub async fn check(apply: &Apply<'_>) -> Result<Option<Release>, String> {
    apply
        .check(&Want::default())
        .await
        .map_err(|e| format!("{e:#}"))
}

pub fn newest(current: &semver::Version) -> String {
    format!("Ferrule {current} is the newest; nothing to do.")
}

/// The one-line answer to "is there an update?".
pub fn check_text(current: &semver::Version, found: Option<&Release>) -> String {
    match found {
        None => newest(current),
        Some(r) => {
            let headline = r.headline();
            format!(
                "Ferrule {} is out (this is {current}){}.",
                r.tag,
                if headline.is_empty() {
                    String::new()
                } else {
                    format!(": {headline}")
                }
            )
        }
    }
}

fn waited(d: Duration) -> String {
    match d.as_secs() {
        s if s >= 120 => format!("{} minutes", s / 60),
        1 => "1 second".into(),
        s => format!("{s} seconds"),
    }
}

/// Installs the newest release, as `ferrule update` would. With the update
/// units, the updater does it (it restarts the service and rolls back if
/// the new one doesn't come up); without them it is done here, and the
/// caller restarts into the new binary.
pub async fn install(apply: Apply<'_>, units: bool, chat: &ChatRef) -> Applied {
    let from = apply.current.to_string();
    let found = match check(&apply).await {
        Ok(Some(r)) => r,
        Ok(None) => return Applied::Said(newest(&apply.current)),
        Err(e) => {
            return Applied::Said(format!(
                "Couldn't check for a new Ferrule: {e}. {from} keeps running."
            ))
        }
    };
    let to = found.version.to_string();
    let after = State::load(&apply.state_dir)
        .events
        .last()
        .map_or(0, |e| e.id);
    if units {
        let request = Request {
            ferrule: true,
            ..Request::default()
        };
        if let Err(e) = state::write_request(&apply.data, &request) {
            return Applied::Said(format!(
                "Couldn't hand the update to the updater: {e:#}. {from} keeps running."
            ));
        }
        let _ = promise::make(&apply.data, chat, Kind::Update { from, after });
        return Applied::Said(format!(
            "Ferrule {to} is being installed by the updater; I tell you here when it's done."
        ));
    }
    if cfg!(windows) {
        return Applied::Said(
            "Updating from the chat isn't available on Windows yet; nothing changed.".into(),
        );
    }
    let exe = apply.exe.clone();
    let others = others_running(&exe);
    if !others.is_empty() {
        let names: Vec<&str> = others.iter().map(|o| o.name.as_str()).collect();
        return Applied::Said(format!(
            "Other Ferrule instances run this same file ({}), and they would be left on the old \
             version. Nothing changed; update from a terminal on the machine, where it can \
             restart them together.",
            names.join(", ")
        ));
    }
    if let Some(why) = someone_elses(&exe) {
        return Applied::Said(format!("Nothing changed: {why}."));
    }
    if let Err(e) = apply::writable(&exe) {
        return Applied::Said(format!(
            "I can't replace {}: {e:#}. Nothing changed; {from} keeps running.",
            exe.display()
        ));
    }
    let idle_for = apply.idle_for;
    match apply.run(&Want::default()).await {
        Ok(Outcome::UpToDate) => Applied::Said(newest(&apply.current)),
        Ok(Outcome::Busy) => Applied::Said(format!(
            "Ferrule {to} wasn't installed: turns were still running after I waited {}. \
             Nothing changed; ask me again later.",
            waited(idle_for)
        )),
        Ok(Outcome::Updated { from, to, .. }) => {
            let _ = promise::make(
                &apply.data,
                chat,
                Kind::Update {
                    from: from.clone(),
                    after,
                },
            );
            Applied::Restarting { from, to, exe }
        }
        Ok(Outcome::RolledBack { from, to }) => Applied::Said(format!(
            "Ferrule {to} didn't start properly, so I went back to {from} and won't try {to} again."
        )),
        Err(e) => Applied::Said(format!(
            "Ferrule {to} wasn't installed: {e:#}. {from} keeps running."
        )),
    }
}

/// The card's words for `update`, or why there's nothing to approve.
pub async fn card(apply: &Apply<'_>, units: bool) -> Result<String, String> {
    let found = check(apply)
        .await
        .map_err(|e| format!("Couldn't check for a new Ferrule: {e}"))?
        .ok_or_else(|| newest(&apply.current))?;
    let headline = found.headline();
    let how = if units {
        "The updater installs it when no turn is running, and goes back to the old one if the \
         new one doesn't start properly."
    } else {
        "It goes in when no turn is running, then I restart into it (about a minute offline) \
         and tell you here when I'm back. The old file is kept beside it."
    };
    Ok(format!(
        "Install Ferrule {} (this is {}){}. {how}",
        found.tag,
        apply.current,
        if headline.is_empty() {
            String::new()
        } else {
            format!(": {headline}")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::tests::{apply, mark, setup};

    fn chat() -> ChatRef {
        ChatRef::new("telegram", "42")
    }

    // The in-chat update says "not on Windows yet" there (see `apply`).
    #[cfg(unix)]
    #[tokio::test]
    async fn an_update_from_the_chat_installs_and_promises_the_chat() {
        let s = setup().await;
        let out = install(apply(&s, None), false, &chat()).await;
        match out {
            Applied::Restarting { from, to, exe } => {
                assert_eq!((from.as_str(), to.as_str()), ("0.5.1", "0.6.0"));
                assert_eq!(exe, s.exe);
            }
            other => panic!("{other:?}"),
        }
        let text = std::fs::read_to_string(&s.exe).unwrap();
        assert!(text.contains("0.6.0"), "{text}");
        let p = promise::take(&s.data).expect("a promise was made");
        assert_eq!(p.chat(), chat());
        assert_eq!(
            p.kind,
            Kind::Update {
                from: "0.5.1".into(),
                after: 0
            }
        );
    }

    // The in-chat update says "not on Windows yet" there (see `apply`).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_busy_bot_gives_up_after_the_wait_and_changes_nothing() {
        let s = setup().await;
        mark(&s.data, "0.5.1", true, 100);
        let mut a = apply(&s, None);
        a.idle_for = Duration::from_secs(1);
        let out = install(a, false, &chat()).await;
        let Applied::Said(said) = out else {
            panic!("{out:?}")
        };
        assert!(said.contains("waited 1 second"), "{said}");
        assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
        assert!(!promise::exists(&s.data));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_binary_is_refused_with_the_reason() {
        use std::os::unix::fs::PermissionsExt;
        let s = setup().await;
        let dir = s.exe.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let root = std::fs::File::create(dir.join("probe")).is_ok();
        let out = install(apply(&s, None), false, &chat()).await;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if root {
            return; // root writes anywhere; nothing to refuse
        }
        let Applied::Said(said) = out else {
            panic!("{out:?}")
        };
        assert!(said.contains("can't replace"), "{said}");
        assert!(said.contains("isn't writable"), "{said}");
        assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
        assert!(!promise::exists(&s.data));
    }

    #[tokio::test]
    async fn with_units_the_chat_writes_the_request_instead() {
        let s = setup().await;
        let out = install(apply(&s, None), true, &chat()).await;
        let Applied::Said(said) = out else {
            panic!("{out:?}")
        };
        assert!(said.contains("being installed by the updater"), "{said}");
        let request = state::take_request(&s.data).expect("a request was written");
        assert!(request.ferrule);
        assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
        assert!(promise::exists(&s.data));
    }

    #[tokio::test]
    async fn the_card_says_what_comes_in_and_up_to_date_has_none() {
        let s = setup().await;
        let card = card(&apply(&s, None), false).await.unwrap();
        assert!(
            card.starts_with("Install Ferrule v0.6.0 (this is 0.5.1)"),
            "{card}"
        );
        let mut a = apply(&s, None);
        a.current = semver::Version::parse("0.6.0").unwrap();
        assert_eq!(
            super::card(&a, false).await.unwrap_err(),
            "Ferrule 0.6.0 is the newest; nothing to do."
        );
    }
}
