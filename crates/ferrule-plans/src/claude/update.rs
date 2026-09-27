//! M36 §5: keeping `claude` current through its own updater or the package
//! manager that installed it. Ferrule never writes, patches or moves the
//! `claude` binary or its files: it finds out how claude was installed and
//! runs that install's update command, as the owner of the files.
//!
//! The methods and their commands follow Anthropic's docs
//! (<https://code.claude.com/docs/en/setup>,
//! <https://code.claude.com/docs/en/troubleshoot-install>).

use super::cli;
use anyhow::{anyhow, bail, Context, Result};
use ferrule_providers::codex::version::newer;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The npm package every install method ships.
pub const PACKAGE: &str = "@anthropic-ai/claude-code";
/// Where the latest version is read: npm's dist-tags, `latest` (the
/// channel native installs default to).
pub const NPM_URL: &str =
    "https://registry.npmjs.org/-/package/@anthropic-ai/claude-code/dist-tags";
/// How long an update command may run (§8: it may want a password or a TTY
/// it will never get).
pub const LIMIT: Duration = Duration::from_secs(300);
/// A shim bigger than this isn't read for its target.
const SHIM_MAX: u64 = 64 * 1024;

/// How `claude` was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// The native installer: `~/.local/share/claude/versions/`.
    Native,
    /// A Homebrew cask (`claude-code`, or `claude-code@latest`).
    Homebrew(String),
    Winget,
    Npm,
    Pnpm,
    /// A system package (apt, dnf, apk): only the package manager updates it.
    System,
    Unknown,
}

impl Method {
    pub fn name(&self) -> &'static str {
        match self {
            Method::Native => "native installer",
            Method::Homebrew(_) => "Homebrew",
            Method::Winget => "WinGet",
            Method::Npm => "npm",
            Method::Pnpm => "pnpm",
            Method::System => "system package",
            Method::Unknown => "unknown install",
        }
    }

    /// The update command, program first; `None` when ferrule can't run one.
    pub fn argv(&self, claude: &Path) -> Option<Vec<OsString>> {
        let words = |w: &[&str]| w.iter().map(OsString::from).collect::<Vec<_>>();
        let latest = format!("{PACKAGE}@latest");
        Some(match self {
            Method::Native => vec![claude.as_os_str().to_owned(), "update".into()],
            Method::Homebrew(cask) => words(&["brew", "upgrade", "--cask", cask]),
            Method::Winget => words(&[
                "winget",
                "upgrade",
                "--id",
                "Anthropic.ClaudeCode",
                "--exact",
                "--silent",
                "--disable-interactivity",
            ]),
            // `claude update` calls npm even for a pnpm install.
            Method::Npm => words(&["npm", "install", "-g", &latest]),
            Method::Pnpm => words(&["pnpm", "add", "-g", &latest]),
            Method::System | Method::Unknown => return None,
        })
    }

    /// What the owner runs by hand.
    pub fn manual(&self, claude: &Path) -> String {
        match self {
            Method::System => {
                "the system package manager that installed it (apt, dnf or apk) updates it".into()
            }
            Method::Unknown => format!(
                "it wasn't installed in a way ferrule knows, so update it the way you installed it \
                 (the official installer: https://code.claude.com/docs/en/setup), then check \
                 `{} --version`",
                claude.display()
            ),
            m => {
                let argv = m.argv(claude).unwrap_or_default();
                format!(
                    "run `{}`",
                    argv.iter()
                        .map(|a| a.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        }
    }
}

/// Which method a path looks like, from the path alone (and a shim's
/// text). Slashes are normalised, so Windows paths match too.
pub fn method_of(path: &Path, shim: &str) -> Method {
    let p = path.to_string_lossy().replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    let shim = shim.replace('\\', "/");
    if lower.contains("/.local/share/claude/versions/") {
        return Method::Native;
    }
    if let Some(rest) = p.split("/Caskroom/").nth(1) {
        let cask = rest.split('/').next().unwrap_or("claude-code");
        return Method::Homebrew(cask.to_string());
    }
    if lower.contains("/microsoft/winget/packages/") || lower.contains("/microsoft/winget/links/") {
        return Method::Winget;
    }
    let package = format!("node_modules/{PACKAGE}");
    let pnpm = |s: &str| s.contains("/.pnpm/") || s.contains("/pnpm/global/");
    if p.contains(&package) {
        return if pnpm(&p) { Method::Pnpm } else { Method::Npm };
    }
    if shim.contains(&package) {
        return if pnpm(&shim) {
            Method::Pnpm
        } else {
            Method::Npm
        };
    }
    if lower.starts_with("/usr/bin/") || lower.starts_with("/usr/lib/") {
        return Method::System;
    }
    Method::Unknown
}

/// The configured `claude`, as found and followed.
#[derive(Debug, Clone)]
pub struct Install {
    /// What `PATH` (or the config) names.
    pub found: PathBuf,
    /// Links followed.
    pub real: PathBuf,
    pub method: Method,
    /// The files' owner: the update runs as them.
    #[cfg(unix)]
    pub owner: (u32, u32),
}

/// Finds `binary` and how it was installed.
pub fn detect(binary: &Path) -> Result<Install> {
    let found = cli::find(binary)
        .ok_or_else(|| anyhow!("claude isn't installed ({})", binary.display()))?;
    let real = std::fs::canonicalize(&found).unwrap_or_else(|_| found.clone());
    // npm and pnpm put a small script where a link would be (pnpm always,
    // npm on Windows): its text names the package.
    let shim = std::fs::metadata(&real)
        .ok()
        .filter(|m| m.len() <= SHIM_MAX)
        .and_then(|_| std::fs::read(&real).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let mut method = method_of(&real, &shim);
    if method == Method::Unknown {
        method = method_of(&found, &shim);
    }
    #[cfg(unix)]
    let owner = {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&real)?;
        (meta.uid(), meta.gid())
    };
    Ok(Install {
        found,
        real,
        method,
        #[cfg(unix)]
        owner,
    })
}

/// The latest `claude` on npm (`dist-tags.latest`).
pub async fn latest(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "ferrule")
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| {
            anyhow!(
                "asking npm for claude's latest version: {}",
                e.without_url()
            )
        })?;
    if !resp.status().is_success() {
        bail!(
            "asking npm for claude's latest version: HTTP {}",
            resp.status()
        );
    }
    let body: serde_json::Value = resp.json().await.context("npm's answer")?;
    body["latest"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("npm's answer has no latest version"))
}

/// `a` is newer than `b`.
pub fn is_newer(a: &str, b: &str) -> bool {
    newer(a, b)
}

/// Why this process can't run the update itself: the files belong to
/// someone else and it isn't root. `None`: it can.
pub fn blocked(install: &Install) -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        if me != 0 && me != install.owner.0 {
            return Some(format!(
                "{} belongs to another user (uid {})",
                install.real.display(),
                install.owner.0
            ));
        }
    }
    let _ = install;
    None
}

/// The update, as it went.
#[derive(Debug, Clone, PartialEq)]
pub struct Updated {
    pub from: String,
    pub to: String,
}

/// Runs the install's update command as the files' owner, with no input,
/// for at most `limit`, then asks claude its version again. `config_dir` is
/// only for `--version`.
pub fn update(install: &Install, config_dir: &Path, limit: Duration) -> Result<Updated> {
    if std::env::var("DISABLE_UPDATES").is_ok_and(|v| !v.is_empty() && v != "0") {
        bail!(
            "DISABLE_UPDATES is set for ferrule, which blocks every claude update; \
             unset it, or {}",
            install.method.manual(&install.found)
        );
    }
    let argv = install.method.argv(&install.found).ok_or_else(|| {
        anyhow!(
            "claude is a {}: {}",
            install.method.name(),
            install.method.manual(&install.found)
        )
    })?;
    if let Some(why) = blocked(install) {
        bail!("{why}; {}", install.method.manual(&install.found));
    }
    let from = cli::version(&install.found, config_dir)?;
    let mut cmd = Command::new(program(&argv[0], &install.found));
    cmd.args(&argv[1..])
        .env_remove("DISABLE_AUTOUPDATER")
        .env_remove("NOTIFY_SOCKET")
        .env_remove("WATCHDOG_USEC")
        .env_remove("WATCHDOG_PID");
    as_owner(&mut cmd, install)?;
    let out = run(cmd, limit).with_context(|| format!("running {}", shown(&argv)))?;
    if !out.status.success() {
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        bail!("{} failed: {}", shown(&argv), tail(text.trim(), 300));
    }
    let to = cli::version(&install.found, config_dir)?;
    Ok(Updated { from, to })
}

/// A package manager next to the claude it installed wins over `PATH`'s.
fn program(name: &std::ffi::OsStr, claude: &Path) -> PathBuf {
    let name = Path::new(name);
    if name.components().count() > 1 {
        return name.to_path_buf();
    }
    claude
        .parent()
        .and_then(|dir| cli::find(&dir.join(name)))
        .unwrap_or_else(|| name.to_path_buf())
}

/// As root, the update runs as the files' owner with their home.
#[cfg(unix)]
fn as_owner(cmd: &mut Command, install: &Install) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 || install.owner.0 == 0 {
        return Ok(());
    }
    let (uid, gid) = install.owner;
    cmd.uid(uid).gid(gid);
    if let Some((name, home)) = user(uid) {
        cmd.env("HOME", home)
            .env("USER", &name)
            .env("LOGNAME", &name);
    }
    Ok(())
}

#[cfg(not(unix))]
fn as_owner(_: &mut Command, _: &Install) -> Result<()> {
    Ok(())
}

/// A uid's name and home, from the password database.
#[cfg(unix)]
fn user(uid: u32) -> Option<(String, PathBuf)> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero passwd is a valid out-parameter.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and buf's length is right.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so both are NUL-terminated strings in buf.
    let (name, home) = unsafe { (CStr::from_ptr(pw.pw_name), CStr::from_ptr(pw.pw_dir)) };
    Some((
        name.to_string_lossy().into_owned(),
        PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())),
    ))
}

fn run(mut cmd: Command, limit: Duration) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Read as it comes, so a chatty updater can't fill the pipe and hang.
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("took more than {} min", limit.as_secs() / 60);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let join = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    Ok(std::process::Output {
        status,
        stdout: join(stdout),
        stderr: join(stderr),
    })
}

fn drain(mut r: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = r.read_to_end(&mut out);
        out
    })
}

fn shown(argv: &[OsString]) -> String {
    let first = Path::new(&argv[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::iter::once(first)
        .chain(argv[1..].iter().map(|a| a.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The last `max` characters.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!("…{}", s.chars().skip(n - max).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_install_method_is_told_by_its_path() {
        let p = |s: &str| method_of(Path::new(s), "");
        assert_eq!(
            p("/home/max/.local/share/claude/versions/2.1.283"),
            Method::Native
        );
        assert_eq!(
            p(r"C:\Users\max\.local\share\claude\versions\2.1.283.exe"),
            Method::Native
        );
        assert_eq!(
            p("/opt/homebrew/Caskroom/claude-code/2.1.283/claude"),
            Method::Homebrew("claude-code".into())
        );
        assert_eq!(
            p("/usr/local/Caskroom/claude-code@latest/2.1.283/claude"),
            Method::Homebrew("claude-code@latest".into())
        );
        assert_eq!(
            p(
                r"C:\Users\max\AppData\Local\Microsoft\WinGet\Packages\Anthropic.ClaudeCode_x\claude.exe"
            ),
            Method::Winget
        );
        assert_eq!(
            p(r"C:\Users\max\AppData\Local\Microsoft\WinGet\Links\claude.exe"),
            Method::Winget
        );
        assert_eq!(
            p("/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
            Method::Npm
        );
        assert_eq!(
            p("/home/max/.local/share/pnpm/global/5/.pnpm/@anthropic-ai+claude-code@2.1.283/node_modules/@anthropic-ai/claude-code/bin/claude.exe"),
            Method::Pnpm
        );
        assert_eq!(p("/usr/bin/claude"), Method::System);
        assert_eq!(p("/opt/tools/claude"), Method::Unknown);
    }

    #[test]
    fn a_shim_is_read_for_the_package_it_starts() {
        let pnpm = "#!/bin/sh\n\"$basedir/global/5/.pnpm/@anthropic-ai+claude-code@2.1.283/node_modules/@anthropic-ai/claude-code/bin/claude.exe\" \"$@\"\n";
        assert_eq!(method_of(Path::new("/pnpm/claude"), pnpm), Method::Pnpm);
        let npm_cmd =
            "@ECHO off\r\n\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";
        assert_eq!(
            method_of(
                Path::new(r"C:\Users\max\AppData\Roaming\npm\claude.cmd"),
                npm_cmd
            ),
            Method::Npm
        );
    }

    #[test]
    fn each_method_has_its_own_update_command_or_says_what_to_run() {
        let claude = Path::new("/home/max/.local/bin/claude");
        let cmd = |m: Method| {
            m.argv(claude).map(|a| {
                a.iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        };
        assert_eq!(
            cmd(Method::Native).as_deref(),
            Some("/home/max/.local/bin/claude update")
        );
        assert_eq!(
            cmd(Method::Homebrew("claude-code".into())).as_deref(),
            Some("brew upgrade --cask claude-code")
        );
        assert_eq!(
            cmd(Method::Npm).as_deref(),
            Some("npm install -g @anthropic-ai/claude-code@latest")
        );
        assert_eq!(
            cmd(Method::Pnpm).as_deref(),
            Some("pnpm add -g @anthropic-ai/claude-code@latest")
        );
        assert!(cmd(Method::Winget)
            .unwrap()
            .starts_with("winget upgrade --id Anthropic.ClaudeCode --exact"));
        assert_eq!(cmd(Method::System), None);
        assert!(Method::System.manual(claude).contains("apt, dnf or apk"));
        assert!(Method::Unknown
            .manual(claude)
            .contains("code.claude.com/docs/en/setup"));
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(tail("abc", 3), "abc");
    }
}
