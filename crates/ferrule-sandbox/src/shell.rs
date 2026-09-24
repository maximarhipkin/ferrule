//! The shell a command string runs in.
//!
//! Unix has `sh`. Windows has nothing models know as well, so there it's
//! Git for Windows' bash when it's installed — the same `sh` syntax — and
//! PowerShell otherwise.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// `sh -c`, or Git Bash's `bash -c` on Windows.
    Posix,
    /// `-EncodedCommand`, so no quoting rule can split the script.
    PowerShell,
}

#[derive(Debug, Clone)]
pub struct Shell {
    pub program: PathBuf,
    pub kind: ShellKind,
    /// What to call it when telling the model or the user.
    pub name: &'static str,
}

impl Shell {
    /// Found once per process.
    pub fn get() -> &'static Shell {
        static SHELL: OnceLock<Shell> = OnceLock::new();
        SHELL.get_or_init(Shell::detect)
    }

    fn detect() -> Shell {
        if !cfg!(windows) {
            return Shell {
                program: "sh".into(),
                kind: ShellKind::Posix,
                name: "sh",
            };
        }
        if let Some(bash) = git_bash() {
            return Shell {
                program: bash,
                kind: ShellKind::Posix,
                name: "Git Bash",
            };
        }
        match on_path("pwsh.exe") {
            Some(pwsh) => Shell {
                program: pwsh,
                kind: ShellKind::PowerShell,
                name: "PowerShell 7",
            },
            None => Shell {
                program: "powershell.exe".into(),
                kind: ShellKind::PowerShell,
                name: "Windows PowerShell",
            },
        }
    }

    /// The arguments that run `script`.
    pub fn args(&self, script: &str) -> Vec<OsString> {
        match self.kind {
            ShellKind::Posix => vec!["-c".into(), script.into()],
            ShellKind::PowerShell => {
                // Windows PowerShell writes redirected output in the OEM code
                // page; ask for UTF-8 so non-ASCII text survives.
                let script = format!("[Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
                [
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                ]
                .into_iter()
                .map(OsString::from)
                .chain([encode(&script).into()])
                .collect()
            }
        }
    }

    /// A sentence for the shell tool's description, where it isn't plain `sh`.
    pub fn model_note(&self) -> Option<String> {
        match (cfg!(windows), self.kind) {
            (false, _) => None,
            (true, ShellKind::Posix) => Some(format!(
                "This is Windows: commands run in {} (bash syntax, Windows paths also work as /c/…).",
                self.name
            )),
            (true, ShellKind::PowerShell) => Some(format!(
                "This is Windows: commands run in {}, so use PowerShell syntax.",
                self.name
            )),
        }
    }
}

/// `bash.exe` from Git for Windows — never `System32\bash.exe`, which is
/// WSL's launcher and runs in another filesystem.
fn git_bash() -> Option<PathBuf> {
    let from_git = on_path("git.exe")
        .and_then(|git| Some(git.parent()?.parent()?.join("bin").join("bash.exe")));
    let installs = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|dir| PathBuf::from(dir).join("Git"))
        .chain(
            std::env::var_os("LOCALAPPDATA")
                .map(|dir| PathBuf::from(dir).join("Programs").join("Git")),
        )
        .map(|git| git.join("bin").join("bash.exe"));
    from_git
        .into_iter()
        .chain(installs)
        .find(|bash| bash.is_file())
}

fn on_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(exe))
        .find(|path| path.is_file())
}

/// Base64 of the UTF-16LE bytes — what `-EncodedCommand` takes.
fn encode(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_utf16le_base64() {
        // PowerShell's own: [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(…))
        assert_eq!(encode("dir"), "ZABpAHIA");
        assert_eq!(encode("echo 'שלום'"), "ZQBjAGgAbwAgACcA6QXcBdUF3QUnAA==");
        assert_eq!(encode("abcd"), "YQBiAGMAZAA=");
        assert_eq!(encode(""), "");
    }

    #[test]
    fn powershell_script_is_encoded_not_quoted() {
        let shell = Shell {
            program: "powershell.exe".into(),
            kind: ShellKind::PowerShell,
            name: "Windows PowerShell",
        };
        let args = shell.args("echo \"a b\" | Out-File x.txt");
        assert_eq!(
            args[..4],
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand"
            ]
        );
        assert!(args[4]
            .to_str()
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
    }

    #[cfg(unix)]
    #[test]
    fn unix_is_plain_sh() {
        let shell = Shell::get();
        assert_eq!(
            (shell.program.as_path(), shell.kind),
            (std::path::Path::new("sh"), ShellKind::Posix)
        );
        assert_eq!(shell.args("true"), ["-c", "true"]);
        assert!(shell.model_note().is_none());
    }
}
