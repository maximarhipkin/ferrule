//! The secrets file: `NAME=value` lines in `<data dir>/private/secrets.env`,
//! written by `ferrule setup` so nobody has to export API keys by hand.
//!
//! It's loaded into the process environment at startup, before any thread
//! exists, and a variable already set in the real environment wins — so
//! everything downstream keeps reading env vars, and `export X=…` still
//! overrides the file. The file is 0600 in a 0700 directory, and the
//! sandbox hides the directory from the shell tool's commands.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Everything sandboxed commands must never read: the secrets file's
/// directory. Kept apart from the rest of the data dir so the memory
/// database and sessions stay ordinary files.
pub fn private_dir() -> Result<PathBuf> {
    Ok(crate::config::data_dir()?.join("private"))
}

pub fn path() -> Result<PathBuf> {
    Ok(private_dir()?.join("secrets.env"))
}

/// Where a variable's value comes from, for `ferrule doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    File,
    Missing,
}

static LOADED: OnceLock<Vec<String>> = OnceLock::new();

pub fn source(name: &str) -> Source {
    if std::env::var_os(name).is_none_or(|v| v.is_empty()) {
        Source::Missing
    } else if LOADED.get().is_some_and(|l| l.iter().any(|n| n == name)) {
        Source::File
    } else {
        Source::Env
    }
}

/// Load the secrets file into the environment. Must run before the tokio
/// runtime (or any other thread) starts: `set_var` isn't thread-safe.
/// Problems are warnings — a broken secrets file shouldn't stop `ferrule
/// doctor` from saying so.
pub fn load_into_env() {
    let mut loaded = Vec::new();
    match path() {
        Ok(path) => match read(&path) {
            Ok(entries) => {
                if !entries.is_empty() {
                    warn_if_loose(&path);
                }
                for (name, value) in entries {
                    if std::env::var_os(&name).is_none() {
                        std::env::set_var(&name, value);
                        loaded.push(name);
                    }
                }
            }
            Err(e) => eprintln!("ferrule: can't read {}: {e:#}", path.display()),
        },
        Err(e) => eprintln!("ferrule: no data dir for the secrets file: {e:#}"),
    }
    let _ = LOADED.set(loaded);
}

#[cfg(unix)]
fn warn_if_loose(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "ferrule: {} is readable by other users; run `chmod 600` on it",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_loose(_path: &Path) {}

/// A missing file is an empty one.
pub fn read(path: &Path) -> Result<Vec<(String, String)>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// `NAME=value` per line; blank lines, `#` comments and an `export ` prefix
/// are allowed, and one pair of matching quotes around the value is
/// dropped. No escapes, no interpolation. Lines that don't parse are
/// skipped.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (name, value) = line.split_once('=')?;
    let name = name.trim();
    if !valid_name(name) {
        return None;
    }
    let value = value.trim();
    let unquoted = [('"', '"'), ('\'', '\'')].iter().find_map(|(open, close)| {
        value
            .strip_prefix(*open)
            .and_then(|v| v.strip_suffix(*close))
    });
    Some((name.to_string(), unquoted.unwrap_or(value).to_string()))
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Set `name` in the file, replacing its line in place or appending one;
/// comments and other lines are kept. Written to a temp file and renamed,
/// with the directory at 0700 and the file at 0600 from the start.
pub fn set(path: &Path, name: &str, value: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("`{name}` isn't a valid variable name");
    }
    if value.contains(['\n', '\r']) {
        bail!("the value for {name} spans lines");
    }
    let dir = path.parent().context("secrets path has no parent")?;
    create_private_dir(dir)?;
    let old = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let entry = format!("{name}={value}");
    let mut replaced = false;
    let mut lines: Vec<String> = old
        .lines()
        .filter_map(|line| match parse_line(line) {
            Some((n, _)) if n == name => {
                if replaced {
                    None // a duplicate would win on read; keep one
                } else {
                    replaced = true;
                    Some(entry.clone())
                }
            }
            _ => Some(line.to_string()),
        })
        .collect();
    if !replaced {
        if lines.is_empty() {
            lines.push(
                "# ferrule secrets — written by `ferrule setup`. Keep this file private.".into(),
            );
        }
        lines.push(entry);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    write_private(path, &text)
}

/// Drop `name`'s lines from the file; other lines and comments are kept.
/// A missing file, or a name that isn't in it, is fine.
pub fn remove(path: &Path, name: &str) -> Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let kept: Vec<&str> = old
        .lines()
        .filter(|line| parse_line(line).is_none_or(|(n, _)| n != name))
        .collect();
    if kept.len() == old.lines().count() {
        return Ok(());
    }
    let mut text = kept.join("\n");
    text.push('\n');
    write_private(path, &text)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let _ = std::fs::remove_file(&tmp);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_people_write() {
        let text = "# comment\n\nA=1\nexport B = two \nC=\"quoted value\"\nD='x'\n\
                    E=has=equals\nnot a line\n9BAD=x\nF=\"unbalanced\n";
        assert_eq!(
            parse(text),
            [
                ("A", "1"),
                ("B", "two"),
                ("C", "quoted value"),
                ("D", "x"),
                ("E", "has=equals"),
                ("F", "\"unbalanced"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    #[test]
    fn set_replaces_in_place_keeps_comments_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/secrets.env");
        set(&path, "OPENAI_API_KEY", "sk-1").unwrap();
        set(&path, "TELEGRAM_BOT_TOKEN", "123:abc").unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("# mine\nOPENAI_API_KEY=dup\n");
        std::fs::write(&path, text).unwrap();
        set(&path, "OPENAI_API_KEY", "sk-2").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# ferrule secrets"));
        assert!(text.contains("# mine\n"));
        assert_eq!(
            read(&path).unwrap(),
            [
                ("OPENAI_API_KEY".to_string(), "sk-2".to_string()),
                ("TELEGRAM_BOT_TOKEN".to_string(), "123:abc".to_string()),
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        remove(&path, "OPENAI_API_KEY").unwrap();
        remove(&path, "NOT_THERE").unwrap();
        assert_eq!(
            read(&path).unwrap(),
            [("TELEGRAM_BOT_TOKEN".to_string(), "123:abc".to_string())]
        );
        assert!(std::fs::read_to_string(&path).unwrap().contains("# mine\n"));
        assert!(set(&path, "BAD NAME", "x").is_err());
        assert!(set(&path, "X", "two\nlines").is_err());
        assert!(read(&dir.path().join("missing")).unwrap().is_empty());
    }
}
