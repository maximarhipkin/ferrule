//! Finding a Chrome or Chromium that is already installed, for `ferrule
//! doctor` now and the browser tool later. Detection only: nothing here
//! downloads a browser.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Start `exe` headless and wait up to `timeout` for it to print a page.
/// `Err` says why it didn't, with the last line Chrome wrote to stderr.
pub fn launch_test(exe: &Path, no_sandbox: bool, timeout: Duration) -> Result<(), String> {
    let profile = std::env::temp_dir().join(format!("ferrule-chrome-check-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&profile);
    let result = run_headless(exe, &profile, no_sandbox, timeout);
    let _ = std::fs::remove_dir_all(&profile);
    result
}

fn run_headless(
    exe: &Path,
    profile: &Path,
    no_sandbox: bool,
    timeout: Duration,
) -> Result<(), String> {
    let mut child = Command::new(exe)
        .args(headless_args(profile, no_sandbox))
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
        let err = launch_test(&fake, false, Duration::from_secs(1)).unwrap_err();
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
        assert_eq!(launch_test(&good, false, Duration::from_secs(10)), Ok(()));
        let err = launch_test(&bad, false, Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("No usable sandbox!"), "{err}");
    }
}
