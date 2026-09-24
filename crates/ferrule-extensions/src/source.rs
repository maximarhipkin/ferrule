//! From "install this" to something that can run: names, the exact-pin
//! launch lines for npm and PyPI, and the rule that a git server's command
//! lives inside its own checkout.

use crate::error::{refused, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The model's `mcp_add`, kept as asked in a pending request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRequest {
    pub name: String,
    /// `npm:pkg@1.2.3`, `pypi:pkg==1.2.3`, `git:https://…[@rev]`, `url:https://…`.
    pub source: String,
    /// git only: the file (or interpreter) to run, relative to the checkout.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub replace: bool,
}

/// The model's `skill_install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRequest {
    /// `git:https://…[@rev]`.
    pub source: String,
    /// The skill's directory inside the repo; the root when absent.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub replace: bool,
}

/// Server and skill names: they become tool-name prefixes and directory
/// names, so lowercase ASCII, digits, `-` and `_`, no `__` (the tool-name
/// separator), 1–40 characters.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && !name.contains("__");
    if ok {
        Ok(())
    } else {
        Err(refused(format!(
            "invalid name `{name}`: use 1-40 of a-z, 0-9, - and _ (no `__`), starting with a letter or digit"
        )))
    }
}

/// `npx -y pkg@1.2.3 …` — `-y` because there is no one to answer npx's
/// prompt; the exact version is what keeps it from resolving anew.
pub fn npm_launch(package: &str, version: &str, args: &[String]) -> (String, Vec<String>) {
    let mut a = vec!["-y".to_string(), format!("{package}@{version}")];
    a.extend(args.iter().cloned());
    ("npx".into(), a)
}

/// `uvx pkg==1.2.3 …`.
pub fn pypi_launch(package: &str, version: &str, args: &[String]) -> (String, Vec<String>) {
    let mut a = vec![format!("{package}=={version}")];
    a.extend(args.iter().cloned());
    ("uvx".into(), a)
}

/// Interpreters a git server may be run with, as long as the script they
/// run is inside the checkout.
pub const INTERPRETERS: &[&str] = &["node", "python3", "python", "deno", "bun"];

/// What may come between the interpreter and the script. Nothing that
/// evaluates code (`-c`, `-e`, `--eval=`) or loads a file from elsewhere
/// (`--require=`, `-m`).
fn harmless_preamble(interpreter: &str, arg: &str) -> bool {
    match interpreter {
        "python3" | "python" => matches!(arg, "-u" | "-B" | "-I" | "-E" | "-s"),
        "node" => matches!(arg, "--enable-source-maps" | "--no-warnings"),
        "deno" => {
            arg == "run"
                || arg == "-A"
                || (arg.starts_with("--allow-") && !arg.starts_with("--allow-run"))
        }
        "bun" => arg == "run",
        _ => false,
    }
}

/// A git server's launch line with every path made absolute: `command` is
/// either a file inside the checkout, or an interpreter from
/// [`INTERPRETERS`] running a script inside it. Anything else would make
/// `mcp_add` a way to run an arbitrary command.
pub fn git_launch(
    checkout: &Path,
    command: &str,
    args: &[String],
) -> Result<(String, Vec<String>)> {
    if INTERPRETERS.contains(&command) {
        let mut args = args.to_vec();
        let i = args
            .iter()
            .position(|a| !harmless_preamble(command, a))
            .ok_or_else(|| refused(format!("`{command}` needs a script inside the repo")))?;
        args[i] = inside(checkout, &args[i])?.to_string_lossy().into_owned();
        return Ok((command.to_string(), args));
    }
    let cmd = inside(checkout, command)?;
    Ok((cmd.to_string_lossy().into_owned(), args.to_vec()))
}

/// `rel` resolved against `checkout`, symlinks followed, and still inside
/// it, and a file.
fn inside(checkout: &Path, rel: &str) -> Result<PathBuf> {
    let outside = || {
        refused(format!(
            "`{rel}` must be a file inside the repo (or use one of: {})",
            INTERPRETERS.join(", ")
        ))
    };
    if Path::new(rel).is_absolute() {
        return Err(outside());
    }
    let root = dunce::canonicalize(checkout)?;
    let p = dunce::canonicalize(checkout.join(rel)).map_err(|_| outside())?;
    if !p.starts_with(&root) || !p.is_file() || p.components().any(|c| c.as_os_str() == ".git") {
        return Err(outside());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn names() {
        for ok in ["fetch", "gh-issues", "a1_b"] {
            validate_name(ok).unwrap();
        }
        for bad in [
            "",
            "Fetch",
            "a__b",
            "-x",
            "_x",
            "a/b",
            "a.b",
            &"x".repeat(41),
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registry_launch_lines_carry_the_exact_pin() {
        let (c, a) = npm_launch("@scope/srv", "1.2.3", &["--port".into(), "0".into()]);
        assert_eq!(
            (c.as_str(), a),
            (
                "npx",
                vec![
                    "-y".into(),
                    "@scope/srv@1.2.3".into(),
                    "--port".into(),
                    "0".into()
                ]
            )
        );
        let (c, a) = pypi_launch("mcp-server-fetch", "0.6.2", &[]);
        assert_eq!(
            (c.as_str(), a),
            ("uvx", vec!["mcp-server-fetch==0.6.2".to_string()])
        );
    }

    #[test]
    fn git_commands_must_live_in_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let co = tmp.path().join("co");
        fs::create_dir_all(co.join("bin")).unwrap();
        fs::write(co.join("server.py"), "").unwrap();
        fs::write(co.join("bin/run"), "").unwrap();
        fs::write(tmp.path().join("outside.py"), "").unwrap();
        let root = dunce::canonicalize(&co).unwrap();

        let (c, a) = git_launch(
            &co,
            "python3",
            &["-u".into(), "server.py".into(), "--x".into()],
        )
        .unwrap();
        assert_eq!(c, "python3");
        assert_eq!(a[1], root.join("server.py").to_string_lossy());
        assert_eq!(a[2], "--x");
        let (c, _) = git_launch(&co, "bin/run", &[]).unwrap();
        assert_eq!(c, root.join("bin/run").to_string_lossy());

        for (cmd, args) in [
            ("bash", vec!["-c".to_string(), "curl evil".into()]),
            ("/bin/sh", vec![]),
            ("python3", vec!["-c".into(), "print(1)".into()]),
            ("python3", vec!["../outside.py".into()]),
            ("node", vec!["/etc/passwd".into()]),
            (
                "node",
                vec!["--require=/tmp/x.js".into(), "server.py".into()],
            ),
            ("node", vec!["--eval=1".into(), "server.py".into()]),
            ("python3", vec!["-m".into(), "server".into()]),
            ("uvx", vec!["server.py".into()]),
            ("bin", vec![]),
            ("missing.py", vec![]),
            ("../outside.py", vec![]),
        ] {
            assert!(git_launch(&co, cmd, &args).is_err(), "{cmd} {args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_checkout_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let co = tmp.path().join("co");
        fs::create_dir_all(&co).unwrap();
        fs::write(tmp.path().join("outside.py"), "").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.py"), co.join("s.py")).unwrap();
        assert!(git_launch(&co, "python3", &["s.py".into()]).is_err());
    }
}
