//! The browser: a Chrome or Chromium that is already installed, driven by
//! agent-browser's MCP server (`agent-browser mcp`). Detection only —
//! nothing here downloads a browser. [`BrowserConfig::server_config`]
//! turns the `[browser]` config into an ordinary MCP server entry, with
//! everything that would loosen the owner's policy taken out of the
//! model's reach.

use crate::config::McpServerConfig;
use ferrule_sandbox::Sandbox;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The MCP server name, so its tools are `mcp__browser__agent_browser_*`.
pub const SERVER_NAME: &str = "browser";

/// Arguments every agent-browser tool takes that the model must not set:
/// they pick another browser session or profile, add Chrome flags, trust
/// another CA, change the domain list or the headless choice. Only
/// `timeoutMs` of the common ones stays.
pub const HIDDEN_ARGS: &[&str] = &[
    "allowedDomains",
    "caCert",
    "clearCaCert",
    "extraArgs",
    "headed",
    "idleTimeout",
    "namespace",
    "restore",
    "restoreCheckFn",
    "restoreCheckText",
    "restoreCheckUrl",
    "restoreSave",
    "session",
];

/// `[browser]` in `ferrule.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    /// Off unless the owner turns it on (`ferrule setup` offers to).
    pub enabled: bool,
    /// The agent-browser binary: a name on PATH or a path.
    pub command: String,
    /// The Chrome or Chromium to drive. Unset = the first one [`find`]
    /// finds.
    pub chrome: Option<PathBuf>,
    /// Hosts the browser may load, e.g. `["example.com", "*.example.org"]`.
    /// Empty = any. With a list, the browser starts with a fresh profile
    /// every time (agent-browser refuses a saved one), so logins don't
    /// stick.
    pub allowed_domains: Vec<String>,
    /// Keep Chrome's own sandbox. `false` runs Chrome with `--no-sandbox`
    /// (still inside ferrule's sandbox), for systems where Chrome's can't
    /// start: containers, root, or no user namespaces. See `docs/browser.md`.
    pub chrome_sandbox: bool,
    /// Show the window instead of running headless.
    pub headed: bool,
    /// agent-browser's tool set: `core` (navigation, snapshots, clicks,
    /// forms, tabs, screenshots, eval) or `all`.
    pub tools: String,
    /// Per-call timeout; a page load can be slow.
    pub timeout_secs: Option<u64>,
    /// Close the browser after this long without a call.
    pub idle_timeout_secs: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: "agent-browser".into(),
            chrome: None,
            allowed_domains: Vec::new(),
            chrome_sandbox: true,
            headed: false,
            tools: "core".into(),
            timeout_secs: Some(120),
            idle_timeout_secs: 600,
        }
    }
}

/// The credential proxy, as the browser uses it. Chrome only sends HTTPS
/// through it (the proxy speaks CONNECT only); it answers `407` so Chrome
/// sends the credentials, and Chrome accepts certificates the proxy mints
/// because their chain carries the key whose hash is `ca_spki_sha256`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProxy {
    /// `host:port`.
    pub addr: String,
    pub username: String,
    pub password: String,
    /// Base64 SHA-256 of the proxy CA's SubjectPublicKeyInfo.
    pub ca_spki_sha256: String,
}

impl BrowserConfig {
    /// The MCP server entry for this config, driving `chrome`, keeping its
    /// profile, caches and daemon socket under `state_dir` (the server's
    /// own, see `ServerHost::state_dir`). Writes an empty agent-browser
    /// config file next to `state_dir` (see [`config_file`]) so an
    /// `agent-browser.json` in the workspace, which the model can write, is
    /// never read.
    pub fn server_config(
        &self,
        chrome: &Path,
        state_dir: &Path,
        proxy: Option<&BrowserProxy>,
    ) -> std::io::Result<McpServerConfig> {
        std::fs::create_dir_all(state_dir)?;
        let own_config = config_file(state_dir);
        std::fs::write(&own_config, "{}\n")?;
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut env = HashMap::new();
        let mut set = |k: &str, v: String| {
            env.insert(k.to_string(), v);
        };
        set("AGENT_BROWSER_CONFIG", path(&own_config));
        set("AGENT_BROWSER_EXECUTABLE_PATH", path(chrome));
        set("AGENT_BROWSER_SOCKET_DIR", path(&state_dir.join("run")));
        set(
            "AGENT_BROWSER_SCREENSHOT_DIR",
            path(&state_dir.join("screenshots")),
        );
        set(
            "AGENT_BROWSER_DOWNLOAD_PATH",
            path(&state_dir.join("downloads")),
        );
        set(
            "AGENT_BROWSER_IDLE_TIMEOUT_MS",
            (self.idle_timeout_secs.max(1) * 1000).to_string(),
        );
        // Page text comes back fenced as untrusted content.
        set("AGENT_BROWSER_CONTENT_BOUNDARIES", "1".into());
        if self.headed {
            set("AGENT_BROWSER_HEADED", "1".into());
        }
        if self.allowed_domains.is_empty() {
            set("AGENT_BROWSER_PROFILE", path(&state_dir.join("profile")));
        } else {
            set(
                "AGENT_BROWSER_ALLOWED_DOMAINS",
                self.allowed_domains.join(","),
            );
        }
        let mut chrome_args = Vec::new();
        if !self.chrome_sandbox {
            chrome_args.push("--no-sandbox".to_string());
        }
        if let Some(proxy) = proxy {
            set("AGENT_BROWSER_PROXY", format!("https={}", proxy.addr));
            set("AGENT_BROWSER_PROXY_USERNAME", proxy.username.clone());
            set("AGENT_BROWSER_PROXY_PASSWORD", proxy.password.clone());
            chrome_args.push(format!(
                "--ignore-certificate-errors-spki-list={}",
                proxy.ca_spki_sha256
            ));
        }
        if !chrome_args.is_empty() {
            set("AGENT_BROWSER_ARGS", chrome_args.join(","));
        }
        Ok(McpServerConfig {
            name: SERVER_NAME.into(),
            command: self.command.clone(),
            args: vec!["mcp".into(), "--tools".into(), self.tools.clone()],
            env,
            timeout_secs: self.timeout_secs,
            sandbox: true,
            // `CI` (any value) makes agent-browser add `--no-sandbox`
            // silently; inherited `AGENT_BROWSER_*` could point it at a
            // running Chrome, extensions or another config.
            env_remove: vec!["CI".into(), "AGENT_BROWSER_*".into()],
            hide_args: HIDDEN_ARGS.iter().map(|s| s.to_string()).collect(),
            writable_roots: sandbox_roots(!self.chrome_sandbox),
            desktop_services: true,
            ..Default::default()
        })
    }
}

/// agent-browser's config file for the server whose state dir is
/// `state_dir`: beside it, not in it. The state dir is writable from inside
/// the sandbox — Chrome writes downloads there, and a page driven over CDP
/// can make it write whatever it likes — and a config file can name
/// `plugins` to run and Chrome flags to add, which the daemon would pick up
/// the next time it starts.
pub fn config_file(state_dir: &Path) -> PathBuf {
    let name = state_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| SERVER_NAME.into());
    state_dir.with_file_name(format!("{name}.agent-browser.json"))
}

/// What Chrome needs writable beyond a helper's usual dirs. On Linux its
/// own sandbox starts in a new user namespace and maps its uid by writing
/// `/proc/<pid>/uid_map`, which Landlock only allows with `/proc` writable.
/// That gives no hold on processes outside ferrule's sandbox: their
/// `/proc` files that matter (`mem`, `environ`) need ptrace access, which
/// Landlock refuses across its boundary.
fn sandbox_roots(no_sandbox: bool) -> Vec<PathBuf> {
    if cfg!(target_os = "linux") && !no_sandbox {
        vec![PathBuf::from("/proc")]
    } else {
        Vec::new()
    }
}

/// Why Chrome's own sandbox won't be used here even though it's asked for:
/// agent-browser adds `--no-sandbox` by itself as root and in a container
/// (it has no way to turn that off), and macOS refuses to start a second
/// sandbox inside Seatbelt, so under `seatbelt` (ferrule's sandbox is on,
/// on a Mac) Chrome's own can't start either. Ferrule refuses to start the
/// browser in these cases unless `chrome_sandbox = false` says the owner
/// accepts that.
pub fn chrome_sandbox_blocker(is_root: bool, seatbelt: bool) -> Option<&'static str> {
    blocker(is_root, seatbelt, &|p| p.exists(), &|p| {
        std::fs::read_to_string(p).ok()
    })
}

fn blocker(
    is_root: bool,
    seatbelt: bool,
    exists: &dyn Fn(&Path) -> bool,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<&'static str> {
    if seatbelt {
        return Some("macOS doesn't let Chrome start its own sandbox inside ferrule's (Seatbelt)");
    }
    if is_root {
        return Some("ferrule runs as root, where Chrome won't start with its own sandbox");
    }
    if exists(Path::new("/.dockerenv")) || exists(Path::new("/run/.containerenv")) {
        return Some("this is a container, where agent-browser turns Chrome's sandbox off");
    }
    let cgroup = read(Path::new("/proc/1/cgroup")).unwrap_or_default();
    if ["docker", "kubepods", "lxc"]
        .iter()
        .any(|c| cgroup.contains(c))
    {
        return Some("this is a container, where agent-browser turns Chrome's sandbox off");
    }
    None
}

/// Does this error from Chrome mean its own sandbox couldn't start?
pub fn is_sandbox_failure(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    [
        "no usable sandbox",
        "setuid sandbox",
        "namespace",
        "zygote",
        "sandbox_host",
    ]
    .iter()
    .any(|s| err.contains(s))
}

/// Where a browser may be, in the order they're tried: `$CHROME_PATH`
/// first, then the names distributions use on PATH, then the fixed
/// install locations for `os` (`std::env::consts::OS`).
pub fn candidates(os: &str, env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = env("CHROME_PATH").filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(path));
    }
    let path_dirs: Vec<PathBuf> = env("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let on_path = |out: &mut Vec<PathBuf>, names: &[&str]| {
        for name in names {
            for dir in &path_dirs {
                out.push(dir.join(name));
            }
        }
    };
    match os {
        "linux" => {
            on_path(
                &mut out,
                &[
                    "google-chrome-stable",
                    "google-chrome",
                    "chromium",
                    "chromium-browser",
                ],
            );
            for fixed in [
                "/opt/google/chrome/chrome",
                "/usr/bin/google-chrome-stable",
                "/usr/bin/google-chrome",
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/usr/lib/chromium/chromium",
                "/snap/bin/chromium",
            ] {
                out.push(PathBuf::from(fixed));
            }
        }
        "macos" => {
            let bundles = [
                "Google Chrome.app/Contents/MacOS/Google Chrome",
                "Chromium.app/Contents/MacOS/Chromium",
            ];
            for bundle in bundles {
                out.push(Path::new("/Applications").join(bundle));
            }
            if let Some(home) = env("HOME").filter(|h| !h.is_empty()) {
                for bundle in bundles {
                    out.push(Path::new(&home).join("Applications").join(bundle));
                }
            }
            on_path(&mut out, &["chromium", "google-chrome"]);
        }
        "windows" => {
            for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(root) = env(var).filter(|r| !r.is_empty()) {
                    let root = PathBuf::from(root);
                    out.push(root.join(r"Google\Chrome\Application\chrome.exe"));
                    out.push(root.join(r"Chromium\Application\chrome.exe"));
                }
            }
            on_path(&mut out, &["chrome.exe"]);
        }
        _ => on_path(&mut out, &["chromium", "google-chrome", "chrome"]),
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// The first candidate that is an executable file.
pub fn find() -> Option<PathBuf> {
    candidates(std::env::consts::OS, &|name| std::env::var_os(name))
        .into_iter()
        .find(|p| is_executable(p))
}

/// The program `command` names: itself when it has a directory part, else
/// the first match on PATH. On Windows `.exe` and `.cmd` (what `npm -g`
/// installs) are tried too, since spawning a bare name won't find a `.cmd`.
pub fn find_command(command: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let with_exts = |base: PathBuf| {
        exts.iter().map(move |ext| {
            let mut name = base.clone().into_os_string();
            name.push(ext);
            PathBuf::from(name)
        })
    };
    let given = Path::new(command);
    if given.components().count() > 1 || given.is_absolute() {
        return with_exts(given.to_path_buf()).find(|p| is_executable(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| with_exts(dir.join(command)))
        .find(|p| is_executable(p))
}

/// The oldest agent-browser this is built against: `mcp` and the tool
/// arguments in [`HIDDEN_ARGS`] are those of 0.38.
pub const MIN_AGENT_BROWSER: (u32, u32, u32) = (0, 38, 0);

/// The agent-browser `command` names, checked to be new enough. The error
/// says what's wrong in words an owner can act on.
pub fn find_agent_browser(command: &str) -> Result<PathBuf, String> {
    let path = find_command(command)
        .ok_or_else(|| format!("`{command}` isn't installed (npm install -g agent-browser)"))?;
    let out = Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("{} didn't run: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (a, b, c) = MIN_AGENT_BROWSER;
    match parse_version(&text) {
        Some(v) if v >= MIN_AGENT_BROWSER => Ok(path),
        Some((x, y, z)) => Err(format!(
            "{} is agent-browser {x}.{y}.{z}; the browser needs {a}.{b}.{c} or newer \
             (npm install -g agent-browser@latest)",
            path.display()
        )),
        None => Err(format!(
            "{} doesn't look like agent-browser (`--version` said {:?})",
            path.display(),
            text.trim()
        )),
    }
}

/// `agent-browser 0.38.1` → `(0, 38, 1)`.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().find(|w| w.contains('.'))?;
    let mut parts = word.trim_start_matches('v').splitn(3, '.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// The flags for a one-shot headless run that prints `about:blank`'s DOM
/// and exits, in a throwaway profile. `no_sandbox` only as root, where
/// Chrome refuses to start otherwise.
pub fn headless_args(profile: &Path, no_sandbox: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        // Without these macOS Chrome waits on the keychain and updaters.
        "--use-mock-keychain",
        "--password-store=basic",
        "--disable-background-networking",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let mut dir = OsString::from("--user-data-dir=");
    dir.push(profile);
    args.push(dir);
    if no_sandbox {
        args.push("--no-sandbox".into());
    }
    args.push("--dump-dom".into());
    args.push("about:blank".into());
    args
}

/// Where a launch test runs Chrome: under `sandbox` made a helper sandbox
/// for `state_dir`, in `workspace`, the way the browser server runs.
pub struct Confine<'a> {
    pub sandbox: &'a Sandbox,
    pub workspace: &'a Path,
    pub state_dir: &'a Path,
}

/// Start `exe` headless and wait up to `timeout` for it to print a page,
/// unconfined or under `confine`. `Err` says why it didn't, with the last
/// line Chrome wrote to stderr.
pub fn launch_test(
    exe: &Path,
    no_sandbox: bool,
    timeout: Duration,
    confine: Option<&Confine>,
) -> Result<(), String> {
    let name = format!("ferrule-chrome-check-{}", std::process::id());
    let profile = match confine {
        Some(c) => c.state_dir.join("tmp").join(name),
        None => std::env::temp_dir().join(name),
    };
    let _ = std::fs::create_dir_all(&profile);
    let result = run_headless(exe, &profile, no_sandbox, timeout, confine);
    let _ = std::fs::remove_dir_all(&profile);
    result
}

fn run_headless(
    exe: &Path,
    profile: &Path,
    no_sandbox: bool,
    timeout: Duration,
    confine: Option<&Confine>,
) -> Result<(), String> {
    let args = headless_args(profile, no_sandbox);
    let mut cmd = match confine {
        Some(c) => c
            .sandbox
            .for_helper(c.state_dir, &sandbox_roots(no_sandbox))
            .with_desktop_services()
            .command(exe, &args, c.workspace)
            .map_err(|e| format!("couldn't sandbox it: {e}"))?,
        None => {
            let mut cmd = Command::new(exe);
            cmd.args(&args);
            cmd
        }
    };
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't start it: {e}"))?;
    // Drain both pipes on threads so a chatty browser can't block on them.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no page after {}s", timeout.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() && stdout.contains("<html") {
        return Ok(());
    }
    let last = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no output");
    let last: String = last.chars().take(160).collect();
    Err(format!("{status}: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn chrome_path_wins_then_path_then_the_usual_places() {
        let path = std::env::join_paths(["/a", "/b"]).unwrap();
        let env = env_of(&[
            ("CHROME_PATH", "/x/my-chrome"),
            ("PATH", path.to_str().unwrap()),
        ]);
        let got = candidates("linux", &env);
        assert_eq!(got[0], PathBuf::from("/x/my-chrome"));
        assert_eq!(got[1], PathBuf::from("/a/google-chrome-stable"));
        assert_eq!(got[2], PathBuf::from("/b/google-chrome-stable"));
        for fixed in [
            "/opt/google/chrome/chrome",
            "/snap/bin/chromium",
            "/usr/bin/chromium",
        ] {
            assert!(
                got.contains(&PathBuf::from(fixed)),
                "{fixed} missing: {got:?}"
            );
        }
        let names: Vec<_> = got.iter().filter_map(|p| p.file_name()).collect();
        assert!(names.contains(&"chromium-browser".as_ref()));
    }

    #[test]
    fn an_empty_chrome_path_is_ignored_and_duplicates_dropped() {
        let env = env_of(&[("CHROME_PATH", ""), ("PATH", "/usr/bin")]);
        let got = candidates("linux", &env);
        assert_eq!(got[0], PathBuf::from("/usr/bin/google-chrome-stable"));
        let once = got
            .iter()
            .filter(|p| *p == Path::new("/usr/bin/chromium"))
            .count();
        assert_eq!(once, 1);
    }

    #[test]
    fn macos_looks_in_both_application_folders() {
        let env = env_of(&[("HOME", "/Users/max")]);
        let got = candidates("macos", &env);
        assert_eq!(
            got[0],
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        );
        assert!(got.contains(&PathBuf::from(
            "/Users/max/Applications/Chromium.app/Contents/MacOS/Chromium"
        )));
    }

    #[test]
    fn windows_looks_under_program_files_and_the_per_user_install() {
        let env = env_of(&[
            ("ProgramFiles", r"C:\Program Files"),
            ("LOCALAPPDATA", r"C:\Users\max\AppData\Local"),
        ]);
        let got = candidates("windows", &env);
        assert_eq!(
            got[0],
            Path::new(r"C:\Program Files").join(r"Google\Chrome\Application\chrome.exe")
        );
        assert!(got.contains(
            &Path::new(r"C:\Users\max\AppData\Local").join(r"Google\Chrome\Application\chrome.exe")
        ));
    }

    #[test]
    fn the_headless_run_uses_a_throwaway_profile_and_the_sandbox_unless_root() {
        let args = headless_args(Path::new("/tmp/p"), false);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert!(args.contains(&"--headless=new"));
        assert!(args.contains(&"--user-data-dir=/tmp/p"));
        assert!(!args.contains(&"--no-sandbox"));
        assert_eq!(args[args.len() - 2..], ["--dump-dom", "about:blank"]);
        assert!(headless_args(Path::new("/tmp/p"), true).contains(&"--no-sandbox".into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_browser_that_hangs_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("chrome");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let err = launch_test(&fake, false, Duration::from_secs(1), None).unwrap_err();
        assert!(err.contains("no page after 1s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn a_browser_that_prints_the_page_passes_and_one_that_fails_says_why() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        std::fs::write(
            &good,
            "#!/bin/sh\necho '<html><head></head><body></body></html>'\n",
        )
        .unwrap();
        let bad = dir.path().join("bad");
        std::fs::write(&bad, "#!/bin/sh\necho 'No usable sandbox!' >&2\nexit 1\n").unwrap();
        for f in [&good, &bad] {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            launch_test(&good, false, Duration::from_secs(10), None),
            Ok(())
        );
        let err = launch_test(&bad, false, Duration::from_secs(10), None).unwrap_err();
        assert!(err.contains("No usable sandbox!"), "{err}");
    }

    #[test]
    fn the_server_entry_keeps_state_in_its_dir_and_policy_out_of_reach() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("browser");
        let cfg = BrowserConfig {
            enabled: true,
            ..Default::default()
        };
        let got = cfg
            .server_config(Path::new("/opt/chrome"), &state, None)
            .unwrap();
        assert_eq!(got.name, "browser");
        assert_eq!(got.command, "agent-browser");
        assert_eq!(got.args, ["mcp", "--tools", "core"]);
        assert!(got.sandbox);
        let env = |k: &str| got.env.get(k).cloned();
        assert_eq!(
            env("AGENT_BROWSER_EXECUTABLE_PATH").as_deref(),
            Some("/opt/chrome")
        );
        // Beside the state dir, which the browser can write, not in it.
        let own = dir.path().join("browser.agent-browser.json");
        assert_eq!(config_file(&state), own);
        assert!(got.desktop_services);
        assert_eq!(
            env("AGENT_BROWSER_CONFIG"),
            Some(own.to_string_lossy().into())
        );
        assert_eq!(std::fs::read_to_string(&own).unwrap().trim(), "{}");
        assert_eq!(
            env("AGENT_BROWSER_PROFILE"),
            Some(state.join("profile").to_string_lossy().into())
        );
        assert_eq!(
            env("AGENT_BROWSER_IDLE_TIMEOUT_MS").as_deref(),
            Some("600000")
        );
        assert_eq!(
            env("AGENT_BROWSER_CONTENT_BOUNDARIES").as_deref(),
            Some("1")
        );
        // Chrome keeps its sandbox, and nothing proxy-related without a proxy.
        for unset in [
            "AGENT_BROWSER_ARGS",
            "AGENT_BROWSER_PROXY",
            "AGENT_BROWSER_HEADED",
            "AGENT_BROWSER_ALLOWED_DOMAINS",
        ] {
            assert_eq!(env(unset), None, "{unset}");
        }
        assert!(got.env_remove.contains(&"CI".to_string()));
        assert!(got.env_remove.contains(&"AGENT_BROWSER_*".to_string()));
        for arg in ["extraArgs", "caCert", "allowedDomains", "session", "headed"] {
            assert!(got.hide_args.contains(&arg.to_string()), "{arg}");
        }
        assert!(!got.hide_args.contains(&"timeoutMs".to_string()));
        // Chrome's own sandbox writes its uid map under /proc (Linux only).
        let proc_root: Vec<PathBuf> = if cfg!(target_os = "linux") {
            vec!["/proc".into()]
        } else {
            vec![]
        };
        assert_eq!(got.writable_roots, proc_root);
    }

    #[test]
    fn versions_are_read_from_what_agent_browser_prints() {
        assert_eq!(parse_version("agent-browser 0.38.1\n"), Some((0, 38, 1)));
        assert_eq!(parse_version("v1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse_version("Unknown command"), None);
        assert!(parse_version("agent-browser 0.27.1").unwrap() < MIN_AGENT_BROWSER);
    }

    #[test]
    fn a_domain_list_drops_the_saved_profile_and_the_proxy_is_trusted_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BrowserConfig {
            allowed_domains: vec!["example.com".into(), "*.example.org".into()],
            chrome_sandbox: false,
            headed: true,
            ..Default::default()
        };
        let proxy = BrowserProxy {
            addr: "127.0.0.1:4000".into(),
            username: "ferrule".into(),
            password: "tok".into(),
            ca_spki_sha256: "abc=".into(),
        };
        let got = cfg
            .server_config(Path::new("/c"), dir.path(), Some(&proxy))
            .unwrap();
        let env = |k: &str| got.env.get(k).map(String::as_str);
        assert!(got.writable_roots.is_empty(), "no own sandbox, no /proc");
        assert_eq!(env("AGENT_BROWSER_PROFILE"), None);
        assert_eq!(
            env("AGENT_BROWSER_ALLOWED_DOMAINS"),
            Some("example.com,*.example.org")
        );
        assert_eq!(env("AGENT_BROWSER_HEADED"), Some("1"));
        assert_eq!(env("AGENT_BROWSER_PROXY"), Some("https=127.0.0.1:4000"));
        assert_eq!(env("AGENT_BROWSER_PROXY_USERNAME"), Some("ferrule"));
        assert_eq!(env("AGENT_BROWSER_PROXY_PASSWORD"), Some("tok"));
        assert_eq!(
            env("AGENT_BROWSER_ARGS"),
            Some("--no-sandbox,--ignore-certificate-errors-spki-list=abc=")
        );
    }

    #[test]
    fn the_config_section_defaults_to_off_and_rejects_typos() {
        let cfg: BrowserConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.enabled && cfg.chrome_sandbox && !cfg.headed);
        assert_eq!(cfg.tools, "core");
        assert!(serde_json::from_str::<BrowserConfig>(r#"{"chrome_sandbx": false}"#).is_err());
    }

    #[test]
    fn root_containers_and_seatbelt_block_chromes_own_sandbox() {
        let none = |_: &Path| false;
        let no_file = |_: &Path| None;
        assert_eq!(blocker(false, false, &none, &no_file), None);
        assert!(blocker(true, false, &none, &no_file)
            .unwrap()
            .contains("root"));
        assert!(blocker(false, true, &none, &no_file)
            .unwrap()
            .contains("macOS"));
        let docker = |p: &Path| p == Path::new("/.dockerenv");
        assert!(blocker(false, false, &docker, &no_file)
            .unwrap()
            .contains("container"));
        let k8s = |_: &Path| Some("0::/kubepods/besteffort/pod1\n".to_string());
        assert!(blocker(false, false, &none, &k8s).is_some());
        let host = |_: &Path| Some("0::/init.scope\n".to_string());
        assert_eq!(blocker(false, false, &none, &host), None);
    }

    #[test]
    fn sandbox_failures_are_told_apart_from_other_crashes() {
        assert!(is_sandbox_failure(
            "exit status: 1: [0924:FATAL:zygote_host_impl_linux.cc(128)] No usable sandbox!"
        ));
        assert!(is_sandbox_failure(
            "Failed to move to new namespace: PID namespaces supported"
        ));
        assert!(!is_sandbox_failure("exit status: 1: cannot open display"));
    }
}
