//! M38: named instances (docs/m38-instances.md). Several agents on one
//! machine, each with its own config, data, bot and service. No name is the
//! default instance, whose paths are those of 0.8.0 byte for byte; a named
//! one swaps every `ferrule` directory for `ferrule-<name>` (§2).

use std::path::{Path, PathBuf};

/// What `--instance` sets and every process (and unit) reads.
pub const ENV: &str = "FERRULE_INSTANCE";
/// `ferrule-<name>` is the system user, and Linux caps user names at 32.
pub const MAX_LEN: usize = 24;

/// A name: 1–24 of `[a-z0-9-]`, a letter or digit at both ends, and not
/// `default` (the default has no name).
pub fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("an instance name can't be empty".into());
    }
    if name == "default" {
        return Err(
            "`default` is the instance with no name: leave --instance out to use it".into(),
        );
    }
    if name.len() > MAX_LEN {
        return Err(format!(
            "`{name}` is {} characters; an instance name has {MAX_LEN} at most",
            name.len()
        ));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(format!(
            "`{name}` has {c:?}; an instance name is lower-case letters, digits and `-`"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(format!(
            "`{name}` starts or ends with `-`; a name starts and ends with a letter or digit"
        ));
    }
    Ok(())
}

/// `FERRULE_INSTANCE`, checked: `Ok(None)` for the default (unset or
/// empty).
pub fn from_env() -> Result<Option<String>, String> {
    match std::env::var(ENV) {
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) => validate(&v).map(|()| Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{ENV} isn't valid text")),
    }
}

/// This process's instance. `main` refuses an invalid name before anything
/// runs, so here one only reads as the default in a unit test.
pub fn current() -> Option<String> {
    from_env().ok().flatten()
}

/// How an instance reads in messages: its name, or `default`.
pub fn label(instance: Option<&str>) -> &str {
    instance.unwrap_or("default")
}

/// `ferrule`, or `ferrule-<name>`: every directory an instance owns.
pub fn dir_name(instance: Option<&str>) -> String {
    match instance {
        None => "ferrule".into(),
        Some(name) => format!("ferrule-{name}"),
    }
}

/// `--instance <name>` for a command line shown to the user, with the
/// space after it; empty for the default.
pub fn flag(instance: Option<&str>) -> String {
    instance
        .map(|n| format!("--instance {n} "))
        .unwrap_or_default()
}

/// Where every instance's config and data dirs sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Roots {
    /// `$FERRULE_ROOT/{config,data}` when set (tests: `dirs` ignores
    /// `APPDATA` on Windows), else the platform's dirs.
    pub fn get() -> Option<Self> {
        if let Some(root) = std::env::var_os("FERRULE_ROOT").filter(|r| !r.is_empty()) {
            let root = PathBuf::from(root);
            return Some(Roots {
                config: root.join("config"),
                data: root.join("data"),
            });
        }
        Some(Roots {
            config: dirs::config_dir()?,
            // Windows: the local AppData, so saved keys don't roam with a
            // profile.
            data: if cfg!(windows) {
                dirs::data_local_dir()
            } else {
                dirs::data_dir()
            }?,
        })
    }

    pub fn config_file(&self, instance: Option<&str>) -> PathBuf {
        self.config.join(dir_name(instance)).join("config.toml")
    }

    pub fn data_dir(&self, instance: Option<&str>) -> PathBuf {
        self.data.join(dir_name(instance))
    }

    /// The named instances with a config here: `ferrule-<name>/config.toml`
    /// for a valid name, sorted.
    pub fn named(&self) -> Vec<String> {
        named_in(&self.config, |dir| dir.join("config.toml").is_file())
    }
}

/// The valid names `ferrule-<name>` under `dir` that `keep` accepts.
pub fn named_in(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name();
            let name = file.to_str()?.strip_prefix("ferrule-")?;
            (validate(name).is_ok() && keep(&e.path())).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// A child `ferrule` for another instance: its name, and none of this
/// process's config, data dir or the secrets it loaded into the
/// environment, which belong to this instance (§9).
pub fn child_for(cmd: &mut std::process::Command, instance: Option<&str>) {
    cmd.env_remove("FERRULE_CONFIG")
        .env_remove("FERRULE_DATA_DIR");
    for name in crate::secrets::loaded_names() {
        cmd.env_remove(name);
    }
    match instance {
        Some(name) => cmd.env(ENV, name),
        None => cmd.env_remove(ENV),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_short_lower_case_and_not_default() {
        for good in ["work", "a", "bot-2", "x1", &"a".repeat(24)] {
            assert_eq!(validate(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "default",
            "Work",
            "a_b",
            "a.b",
            "a/b",
            "..",
            "-a",
            "a-",
            "-",
            "a b",
            "é",
            "a%i",
            &"a".repeat(25),
        ] {
            assert!(validate(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate("default")
            .unwrap_err()
            .contains("leave --instance out"));
        // The system user fits Linux's 32.
        assert!(format!("ferrule-{}", "a".repeat(MAX_LEN)).len() <= 32);
    }

    #[test]
    fn the_default_keeps_its_0_8_0_paths_and_a_name_gets_siblings() {
        let roots = Roots {
            config: "/home/u/.config".into(),
            data: "/home/u/.local/share".into(),
        };
        assert_eq!(
            roots.config_file(None),
            Path::new("/home/u/.config/ferrule/config.toml")
        );
        assert_eq!(
            roots.data_dir(None),
            Path::new("/home/u/.local/share/ferrule")
        );
        assert_eq!(
            roots.config_file(Some("work")),
            Path::new("/home/u/.config/ferrule-work/config.toml")
        );
        assert_eq!(
            roots.data_dir(Some("work")),
            Path::new("/home/u/.local/share/ferrule-work")
        );
        assert_eq!(flag(None), "");
        assert_eq!(flag(Some("work")), "--instance work ");
        assert_eq!(label(None), "default");
    }

    #[test]
    fn named_instances_are_found_by_their_config() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots {
            config: dir.path().into(),
            data: dir.path().join("data"),
        };
        for (d, config) in [
            ("ferrule", true),
            ("ferrule-work", true),
            ("ferrule-home", true),
            ("ferrule-empty", false),
            ("ferrule-Bad", true),
            ("ferrule-default", true),
            ("other", true),
        ] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
            if config {
                std::fs::write(dir.path().join(d).join("config.toml"), "").unwrap();
            }
        }
        assert_eq!(roots.named(), ["home", "work"]);
    }
}
