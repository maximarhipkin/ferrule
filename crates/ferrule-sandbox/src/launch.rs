//! Windows: the launcher between ferrule and a sandboxed program.
//!
//! `std::process::Command` can't take a token, so on Windows
//! [`Sandbox::command`](crate::Sandbox::command) returns a `Command` that
//! runs `<launcher> __sandbox-launch <program> <args…>` with the policy in
//! [`SPEC_VAR`]. The launcher (ferrule itself, or the
//! `ferrule-sandbox-launch` bin) starts the program under a restricted
//! token inside a job and exits with its exit code. The environment,
//! working directory and stdio are the ones `Command` gave the launcher.
//!
//! What's here compiles everywhere, so the spec, the quoting and the
//! lookups are tested on every OS; the Win32 side is in `windows.rs`.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The first argument that makes ferrule act as the launcher.
pub const LAUNCH_ARG: &str = "__sandbox-launch";
/// The policy, as JSON. Removed before the program starts.
pub const SPEC_VAR: &str = "FERRULE_SANDBOX_SPEC";
/// Overrides where the launcher is looked for.
pub const LAUNCHER_VAR: &str = "FERRULE_SANDBOX_LAUNCHER";
/// Overrides where the capability SIDs are kept.
pub const STATE_VAR: &str = "FERRULE_SANDBOX_STATE";

/// What the launcher applies. The program and its arguments travel in
/// argv, so they never need encoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// Canonical dirs the program may write. Each gets a capability SID.
    pub write_roots: Vec<PathBuf>,
    /// Canonical paths ferrule owns and the program may not read: they
    /// get the protected DACL (see `windows::protect`).
    pub protect: Vec<PathBuf>,
    /// `WRITE_RESTRICTED` with the capability SIDs. `false` is hide-only
    /// (an unconfined MCP server): writes as open as the user's, the
    /// protected paths and ferrule's process still shut.
    pub confine_writes: bool,
    /// The job's process limit, 0 for none.
    pub process_limit: u32,
    /// The job's memory limit in MB.
    pub memory_mb: Option<u64>,
    /// Where the capability SIDs are kept (`<data>/sandbox/windows-caps.json`).
    pub state_file: Option<PathBuf>,
}

impl Spec {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a spec always serializes")
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("bad {SPEC_VAR}: {e}"))
    }
}

/// Where the launcher is: `FERRULE_SANDBOX_LAUNCHER`, the current exe if
/// it's ferrule, or `ferrule`/`ferrule-sandbox-launch` next to the current
/// exe or one dir up (test binaries run from `deps/`). Looked up once.
pub fn launcher() -> Result<&'static Path, String> {
    static FOUND: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            locate(
                std::env::var_os(LAUNCHER_VAR).filter(|v| !v.is_empty()),
                std::env::current_exe().ok(),
                |p| p.is_file(),
            )
        })
        .as_deref()
        .map_err(Clone::clone)
}

fn locate(
    from_env: Option<OsString>,
    exe: Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(path) = from_env {
        let path = PathBuf::from(path);
        return if exists(&path) {
            Ok(path)
        } else {
            Err(format!(
                "{LAUNCHER_VAR} points at {}, which doesn't exist",
                path.display()
            ))
        };
    }
    let exe = exe.ok_or("can't tell where the current executable is")?;
    if exe.file_stem().is_some_and(|s| s == "ferrule") {
        return Ok(exe);
    }
    let suffix = std::env::consts::EXE_SUFFIX;
    let names = [
        format!("ferrule{suffix}"),
        format!("ferrule-sandbox-launch{suffix}"),
    ];
    let dir = exe.parent();
    dir.into_iter()
        .chain(dir.and_then(Path::parent))
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| exists(p))
        .ok_or_else(|| {
            format!(
                "no sandbox launcher (ferrule{suffix} or ferrule-sandbox-launch{suffix}) next to {}; set {LAUNCHER_VAR}",
                exe.display()
            )
        })
}

/// Appends `arg` to a command line the way the MSVC runtime (and Rust's
/// `std::env::args`) splits it back: quoted when it has a space, a tab or
/// a quote, or is empty; backslashes doubled only before a quote.
pub fn quote_arg(arg: &[u16], out: &mut Vec<u16>) {
    const BS: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;
    let plain = !arg.is_empty()
        && !arg.iter().any(|&c| {
            c == b' ' as u16 || c == b'\t' as u16 || c == b'\n' as u16 || c == 0x0b || c == QUOTE
        });
    if plain {
        out.extend_from_slice(arg);
        return;
    }
    out.push(QUOTE);
    let mut backslashes = 0;
    for &c in arg {
        if c == BS {
            backslashes += 1;
            continue;
        }
        let n = if c == QUOTE {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        out.extend(std::iter::repeat_n(BS, n));
        backslashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n(BS, backslashes * 2));
    out.push(QUOTE);
}

/// `args` joined with [`quote_arg`], space-separated.
pub fn command_line<'a>(args: impl IntoIterator<Item = &'a [u16]>) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, arg) in args.into_iter().enumerate() {
        if i > 0 {
            out.push(b' ' as u16);
        }
        quote_arg(arg, &mut out);
    }
    out
}

/// A batch file's arguments pass through `cmd.exe`, whose own parsing no
/// quoting makes safe for `"`, `%`, `!`, or a line break. Those are refused.
pub fn batch_safe(arg: &str) -> bool {
    !arg.contains(['"', '%', '!', '\n', '\r'])
}

/// Whether `program` names a batch file, which `CreateProcess` can't run
/// by itself.
pub fn is_batch(program: &Path) -> bool {
    program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// `program` as `CreateProcess` wants it: a full path. A bare name is
/// looked up along `path`, trying each `pathext` extension when it has
/// none; a path is taken as it is, with the extensions tried if it
/// doesn't exist.
pub fn resolve_program(
    program: &Path,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let exts: Vec<String> = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let candidates = |base: PathBuf| {
        let mut out = Vec::new();
        if base.extension().is_some() {
            out.push(base.clone());
        }
        for ext in &exts {
            let mut p = base.clone().into_os_string();
            p.push(ext);
            out.push(PathBuf::from(p));
        }
        out
    };
    let has_dir = program.components().count() > 1 || program.is_absolute();
    if has_dir {
        if exists(program) {
            return Some(program.to_path_buf());
        }
        return candidates(program.to_path_buf())
            .into_iter()
            .find(|p| exists(p));
    }
    std::env::split_paths(path?)
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|d| candidates(d.join(program)))
        .find(|p| exists(p))
}

/// The DACL ferrule's secret paths get on Windows, as SDDL: SYSTEM, and
/// the user only while Authenticated Users is enabled in their token —
/// which it is in every normal logon and isn't in the sandbox token
/// (`Member_of` ignores deny-only groups in an allow ACE). The owner keeps
/// `READ_CONTROL` only, so the implicit owner rights can't rewrite it.
pub fn protected_sddl(user_sid: &str, dir: bool) -> String {
    let inherit = if dir { "OICI" } else { "" };
    format!(
        "D:PAI(A;{inherit};FA;;;SY)(XA;{inherit};FA;;;{user_sid};(Member_of {{SID(AU)}}))(A;{inherit};RC;;;OW)"
    )
}

/// The same condition for ferrule's own process and threads.
pub fn process_sddl(user_sid: &str) -> String {
    format!("D:(A;;GA;;;SY)(XA;;GA;;;{user_sid};(Member_of {{SID(AU)}}))(A;;RC;;;OW)")
}

/// The launcher's `main`: `argv` is everything after [`LAUNCH_ARG`]. Never
/// returns. Exits with the program's exit code, or [`LAUNCH_FAILED`] after
/// saying why on stderr.
pub fn run(argv: Vec<OsString>) -> ! {
    let code = match launch(argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ferrule sandbox launcher: {e}");
            LAUNCH_FAILED
        }
    };
    std::process::exit(code)
}

/// What the launcher exits with when it couldn't start the program.
pub const LAUNCH_FAILED: i32 = 125;

#[cfg(windows)]
fn launch(argv: Vec<OsString>) -> Result<i32, String> {
    let spec = std::env::var(SPEC_VAR).map_err(|_| format!("{SPEC_VAR} is not set"))?;
    std::env::remove_var(SPEC_VAR);
    let spec = Spec::from_json(&spec)?;
    let (program, args) = argv.split_first().ok_or("no program to run")?;
    crate::windows::launch(&spec, Path::new(program), args)
}

#[cfg(not(windows))]
fn launch(_argv: Vec<OsString>) -> Result<i32, String> {
    Err("the sandbox launcher only runs on Windows".into())
}

/// Call first thing in a `main` that doubles as the launcher: runs it and
/// never returns when the first argument is [`LAUNCH_ARG`].
pub fn intercept() {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == LAUNCH_ARG) {
        run(args.collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(args: &[&str]) -> String {
        let wide: Vec<Vec<u16>> = args.iter().map(|a| a.encode_utf16().collect()).collect();
        String::from_utf16(&command_line(wide.iter().map(Vec::as_slice))).unwrap()
    }

    #[test]
    fn quoting_follows_the_msvc_rules() {
        assert_eq!(quoted(&["a", "b c", ""]), r#"a "b c" """#);
        assert_eq!(quoted(&[r"C:\dir\x.exe"]), r"C:\dir\x.exe");
        assert_eq!(quoted(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(quoted(&[r"C:\with space\"]), r#""C:\with space\\""#);
        assert_eq!(quoted(&[r#"a\"b"#]), r#""a\\\"b""#);
        assert_eq!(quoted(&[r"a\\b c"]), r#""a\\b c""#);
        assert_eq!(quoted(&["tab\there"]), "\"tab\there\"");
    }

    #[test]
    fn protected_dacls_key_on_authenticated_users() {
        let user = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            protected_sddl(user, true),
            "D:PAI(A;OICI;FA;;;SY)(XA;OICI;FA;;;S-1-5-21-1-2-3-1001;(Member_of {SID(AU)}))(A;OICI;RC;;;OW)"
        );
        assert!(!protected_sddl(user, false).contains("OICI"));
        assert!(process_sddl(user).contains("(XA;;GA;;;S-1-5-21-1-2-3-1001;(Member_of {SID(AU)}))"));
    }

    #[test]
    fn spec_round_trips() {
        let spec = Spec {
            write_roots: vec![PathBuf::from("C:\\ws")],
            protect: vec![PathBuf::from("C:\\data\\private")],
            confine_writes: true,
            process_limit: 256,
            memory_mb: Some(2048),
            state_file: Some(PathBuf::from("C:\\data\\sandbox\\windows-caps.json")),
        };
        assert_eq!(Spec::from_json(&spec.to_json()).unwrap(), spec);
        assert!(Spec::from_json("{").is_err());
    }

    #[test]
    fn launcher_lookup_order() {
        let yes = |_: &Path| true;
        let only = |want: PathBuf| move |p: &Path| p == want;
        // The variable wins, and must point at something.
        assert_eq!(
            locate(Some("/x/l".into()), Some("/bin/ferrule".into()), yes).unwrap(),
            PathBuf::from("/x/l")
        );
        assert!(locate(Some("/x/l".into()), None, |_| false).is_err());
        // ferrule is its own launcher.
        let exe = format!("/bin/ferrule{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(
            locate(None, Some(exe.clone().into()), |_| false).unwrap(),
            PathBuf::from(&exe)
        );
        // A test binary in deps/ finds the one a dir up.
        let bin = PathBuf::from(format!(
            "/t/debug/ferrule-sandbox-launch{}",
            std::env::consts::EXE_SUFFIX
        ));
        assert_eq!(
            locate(None, Some("/t/debug/deps/it-123".into()), only(bin.clone())).unwrap(),
            bin
        );
        assert!(locate(None, Some("/t/debug/deps/it-123".into()), |_| false).is_err());
    }

    #[test]
    fn programs_resolve_through_path_and_pathext() {
        let path = std::env::join_paths(["/a", "/b"]).unwrap();
        let exists = |p: &Path| {
            ["/b/bash.exe", "/a/npm.cmd", "/b/npm.cmd", "/c/tool.exe"]
                .iter()
                .any(|x| Path::new(x) == p)
        };
        let resolve =
            |p: &str| resolve_program(Path::new(p), Some(&path), Some(".EXE;.CMD"), exists);
        assert_eq!(resolve("bash"), Some(PathBuf::from("/b/bash.exe")));
        assert_eq!(resolve("bash.exe"), Some(PathBuf::from("/b/bash.exe")));
        assert_eq!(resolve("npm"), Some(PathBuf::from("/a/npm.cmd")));
        assert_eq!(resolve("/c/tool"), Some(PathBuf::from("/c/tool.exe")));
        assert_eq!(resolve("/c/tool.exe"), Some(PathBuf::from("/c/tool.exe")));
        assert_eq!(resolve("nope"), None);
        assert!(is_batch(Path::new("/a/npm.cmd")) && is_batch(Path::new("x.BAT")));
        assert!(!is_batch(Path::new("/b/bash.exe")));
        assert!(batch_safe("install --save-dev x") && !batch_safe("%PATH%"));
    }
}
