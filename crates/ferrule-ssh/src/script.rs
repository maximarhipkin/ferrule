//! What runs on the remote: POSIX `sh` scripts sent over stdin, never on
//! the command line (docs/m34-ssh-local.md §5–§6).
//!
//! ssh runs one fixed command, [`BOOTSTRAP`], which reads a length and
//! that many bytes of script and evals them. The rest of stdin is the
//! nonce, the parameters (one per line, [`param`]-escaped) and then any
//! data (a file's new content). Nothing the model wrote is ever parsed by
//! a remote shell as code: parameters are read with `read -r`, and the
//! command for `shell` is passed to `sh -c` as one argument.
//!
//! Every script reports on stderr: `<nonce> start` once it runs,
//! `<nonce> exit <rc>` when it ends, `<nonce> fail <code>` for a refusal
//! (escape, off limits, missing …). A start with no end means the link
//! dropped mid-command: interrupted, never a success.

/// The one command ssh runs. Single-quoted as a whole for the remote login
/// shell, whatever it is (sh, bash, zsh, fish all pass it to `sh`).
pub const BOOTSTRAP: &str =
    "exec sh -c 'IFS= read -r n; s=$(dd bs=1 count=\"$n\" 2>/dev/null); eval \"$s\"'";

/// Shared by every script: reading parameters, markers, and the path
/// checks the file tools need.
const PRELUDE: &str = r#"set -f
IFS= read -r N
printf '%s start\n' "$N" >&2
ferrule_p() { IFS= read -r ferrule_l; ferrule_v=$(printf '%bx' "$ferrule_l"); ferrule_v=${ferrule_v%x}; }
ferrule_fail() { printf '%s fail %s\n' "$N" "$1" >&2; exit 3; }
ferrule_fold() { if [ "$ferrule_icase" = 1 ]; then printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; else printf '%s' "$1"; fi; }
ferrule_real() {
  ferrule_q=$1; ferrule_rest=; ferrule_hops=0
  while :; do
    if [ -L "$ferrule_q" ]; then
      ferrule_hops=$((ferrule_hops + 1)); [ "$ferrule_hops" -gt 40 ] && return 1
      ferrule_t=$(readlink -- "$ferrule_q") || return 1
      case $ferrule_t in /*) ;; *) ferrule_d=${ferrule_q%/*}; ferrule_t=${ferrule_d:-/}/$ferrule_t ;; esac
      ferrule_q=$ferrule_t; continue
    fi
    if [ -d "$ferrule_q" ]; then
      ferrule_d=$(cd -P -- "$ferrule_q" 2>/dev/null && pwd -P) || return 1
      ferrule_r=${ferrule_d%/}$ferrule_rest; ferrule_r=${ferrule_r:-/}; return 0
    fi
    if [ -e "$ferrule_q" ]; then
      ferrule_d=${ferrule_q%/*}
      ferrule_d=$(cd -P -- "${ferrule_d:-/}" 2>/dev/null && pwd -P) || return 1
      ferrule_r=${ferrule_d%/}/${ferrule_q##*/}$ferrule_rest; return 0
    fi
    case $ferrule_q in /|'') return 1 ;; esac
    case ${ferrule_q##*/} in .|..|'') return 1 ;; esac
    ferrule_rest=/${ferrule_q##*/}$ferrule_rest; ferrule_q=${ferrule_q%/*}; ferrule_q=${ferrule_q:-/}
  done
}
ferrule_common() {
  ferrule_p; ferrule_ws=$ferrule_v
  ferrule_p; ferrule_icase=$ferrule_v
  ferrule_p; ferrule_n=$ferrule_v; ferrule_denies=; ferrule_i=0
  while [ "$ferrule_i" -lt "$ferrule_n" ]; do
    ferrule_p; ferrule_denies="$ferrule_denies$(ferrule_fold "$ferrule_v")
"
    ferrule_i=$((ferrule_i + 1))
  done
}
ferrule_check() {
  ferrule_real "$1" || ferrule_fail escape
  ferrule_f=$(ferrule_fold "$ferrule_r"); ferrule_w=$(ferrule_fold "$ferrule_ws")
  case "$ferrule_f/" in "${ferrule_w%/}/"*) ;; *) ferrule_fail escape ;; esac
  ferrule_ifs=$IFS; IFS='
'
  for ferrule_x in $ferrule_denies; do
    case "$ferrule_f/" in "${ferrule_x%/}/"*) IFS=$ferrule_ifs; ferrule_fail hidden ;; esac
  done
  IFS=$ferrule_ifs
}
"#;

const EPILOGUE: &str = r#"
ferrule_main; ferrule_st=$?
printf '%s exit %s\n' "$N" "$ferrule_st" >&2
exit "$ferrule_st"
"#;

/// Learns the remote: home, OS, the real workspace, whether it's
/// writable, and the read denies resolved against the remote home.
/// Params: workspace spec, deny count, denies (`~/…`, absolute, or
/// relative to the workspace).
const HELLO: &str = r#"ferrule_main() {
  ferrule_p; ferrule_s=$ferrule_v
  case $ferrule_s in "~") ferrule_s=$HOME ;; "~/"*) ferrule_s=$HOME/${ferrule_s#"~/"} ;; esac
  printf 'home\t%s\n' "$HOME"
  printf 'os\t%s\n' "$(uname -s)"
  ferrule_wsr=$(cd -P -- "$ferrule_s" 2>/dev/null && pwd -P) || ferrule_fail nows
  printf 'ws\t%s\n' "$ferrule_wsr"
  [ -w "$ferrule_wsr" ] && printf 'writable\t1\n'
  ferrule_p; ferrule_n=$ferrule_v; ferrule_i=0
  while [ "$ferrule_i" -lt "$ferrule_n" ]; do
    ferrule_p; ferrule_x=$ferrule_v
    case $ferrule_x in "~") ferrule_x=$HOME ;; "~/"*) ferrule_x=$HOME/${ferrule_x#"~/"} ;; /*) ;; *) ferrule_x=$ferrule_wsr/$ferrule_x ;; esac
    if ferrule_real "$ferrule_x"; then printf 'deny\t%s\n' "$ferrule_r"; else printf 'deny\t%s\n' "$ferrule_x"; fi
    ferrule_i=$((ferrule_i + 1))
  done
  command -v curl >/dev/null 2>&1 && printf 'has\tcurl\n'
  command -v git >/dev/null 2>&1 && printf 'has\tgit\n'
  return 0
}"#;

/// Common params, path. Prints the file (at most [`READ_CAP`] + 1 bytes,
/// so the caller can tell it was cut).
const READ: &str = r#"ferrule_main() {
  ferrule_common; ferrule_p; ferrule_check "$ferrule_v"
  [ -d "$ferrule_r" ] && ferrule_fail dir
  [ -e "$ferrule_r" ] || ferrule_fail missing
  head -c 67108865 < "$ferrule_r"
}"#;

/// The most `read_file` and `edit_file` take from a remote file.
pub const READ_CAP: usize = 64 * 1024 * 1024;

/// Common params, path, guard (empty, `missing`, or `<crc> <size>` as
/// `cksum` prints it), then the content on the rest of stdin. Written to
/// a temp file beside the target (the original's mode kept) and renamed
/// over it. Prints the real path.
const WRITE: &str = r#"ferrule_main() {
  ferrule_common; ferrule_p; ferrule_check "$ferrule_v"; ferrule_p; ferrule_g=$ferrule_v
  ferrule_f=$ferrule_r
  [ -d "$ferrule_f" ] && ferrule_fail dir
  case $ferrule_g in
    '') ;;
    missing) [ -e "$ferrule_f" ] && ferrule_fail changed ;;
    *) [ -f "$ferrule_f" ] || ferrule_fail changed
       set -- $(cksum < "$ferrule_f"); [ "$1 $2" = "$ferrule_g" ] || ferrule_fail changed ;;
  esac
  ferrule_d=${ferrule_f%/*}; ferrule_d=${ferrule_d:-/}
  mkdir -p -- "$ferrule_d" || return 1
  ferrule_t=$ferrule_d/.${ferrule_f##*/}.ferrule-$$
  [ -f "$ferrule_f" ] && cp -p -- "$ferrule_f" "$ferrule_t" 2>/dev/null
  cat > "$ferrule_t" || { rm -f -- "$ferrule_t"; return 1; }
  mv -f -- "$ferrule_t" "$ferrule_f" || { rm -f -- "$ferrule_t"; return 1; }
  printf '%s\n' "$ferrule_f"
}"#;

/// Common params, path. One `dir\t<name>` or `file\t<name>` per entry
/// (a symlink is a file, as `list_dir` says locally).
const LIST: &str = r#"ferrule_main() {
  ferrule_common; ferrule_p; ferrule_check "$ferrule_v"
  if [ ! -d "$ferrule_r" ]; then [ -e "$ferrule_r" ] && ferrule_fail notdir; ferrule_fail missing; fi
  cd -- "$ferrule_r" || return 1
  set +f
  for ferrule_e in * .[!.]* ..?*; do
    [ -e "$ferrule_e" ] || [ -L "$ferrule_e" ] || continue
    if [ -d "$ferrule_e" ] && [ ! -L "$ferrule_e" ]; then printf 'dir\t%s\n' "$ferrule_e"; else printf 'file\t%s\n' "$ferrule_e"; fi
  done
  return 0
}"#;

/// Workspace, command, env count, `NAME=value` × count. stdin stays open
/// for as long as the command may run: when it closes (the link dropped,
/// a timeout, `/stop`), the watchdog kills the whole process group, which
/// sshd made a session of its own.
const SHELL: &str = r#"ferrule_main() {
  ferrule_p; ferrule_ws=$ferrule_v
  ferrule_p; ferrule_cmd=$ferrule_v
  ferrule_p; ferrule_n=$ferrule_v; ferrule_i=0
  while [ "$ferrule_i" -lt "$ferrule_n" ]; do ferrule_p; export "$ferrule_v"; ferrule_i=$((ferrule_i + 1)); done
  cd -- "$ferrule_ws" 2>/dev/null || ferrule_fail nows
  exec 3<&0
  ( IFS= read -r ferrule_l; kill -KILL 0 ) <&3 &
  ferrule_wd=$!
  set +f
  sh -c "$ferrule_cmd" </dev/null 3<&- &
  wait "$!"; ferrule_rc=$?
  kill "$ferrule_wd" 2>/dev/null
  return "$ferrule_rc"
}"#;

/// File name; the content on the rest of stdin. Prints where it went: a
/// fresh private dir under the remote's temp dir.
const UPLOAD: &str = r#"ferrule_main() {
  ferrule_p; ferrule_n=$ferrule_v
  umask 077
  ferrule_d=$(mktemp -d "${TMPDIR:-/tmp}/ferrule.XXXXXX") || return 1
  cat > "$ferrule_d/$ferrule_n" || return 1
  printf '%s\n' "$ferrule_d/$ferrule_n"
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Hello,
    Read,
    Write,
    List,
    Shell,
    Upload,
}

impl Op {
    /// The whole script for `op`.
    pub fn script(self) -> String {
        let body = match self {
            Op::Hello => HELLO,
            Op::Read => READ,
            Op::Write => WRITE,
            Op::List => LIST,
            Op::Shell => SHELL,
            Op::Upload => UPLOAD,
        };
        format!("{PRELUDE}{body}{EPILOGUE}")
    }

    /// Whether running it twice is harmless, so it may be retried even
    /// after the link dropped mid-way.
    pub fn idempotent(self) -> bool {
        matches!(self, Op::Hello | Op::Read | Op::List)
    }
}

/// One parameter line: printable ASCII other than `\` as is, every other
/// byte as `\0ooo`, which the remote's `printf %b` turns back. NUL can't
/// cross (a shell variable can't hold it).
pub fn param(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err("a NUL byte can't be sent to the remote shell".into());
    }
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if (0x20..0x7f).contains(&b) && b != b'\\' {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\0{b:03o}"));
        }
    }
    Ok(out)
}

/// What stdin carries before any data: the script's length and bytes, the
/// nonce, the parameters.
pub fn stdin_header(op: Op, nonce: &str, params: &[String]) -> Result<Vec<u8>, String> {
    let script = op.script();
    let mut out = format!("{}\n{script}{nonce}\n", script.len()).into_bytes();
    for p in params {
        out.extend_from_slice(param(p)?.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// What the markers in a script's stderr said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Markers {
    pub started: bool,
    pub exit: Option<i32>,
    pub fail: Option<String>,
}

/// Take the markers out of `stderr`, leaving what the command itself
/// wrote. A marker may follow the command's own output on the same line
/// (it didn't end with a newline), so they're cut out, not whole lines.
pub fn strip_markers(stderr: &str, nonce: &str) -> (Markers, String) {
    let mut markers = Markers::default();
    let mut rest = String::with_capacity(stderr.len());
    let mut s = stderr;
    while let Some(i) = s.find(nonce) {
        rest.push_str(&s[..i]);
        let after = &s[i + nonce.len()..];
        let end = after.find('\n').map(|e| e + 1).unwrap_or(after.len());
        let marker = after[..end].trim();
        let mut words = marker.split_whitespace();
        match (words.next(), words.next()) {
            (Some("start"), _) => markers.started = true,
            (Some("exit"), Some(code)) => markers.exit = code.parse().ok(),
            (Some("fail"), Some(code)) => markers.fail = Some(code.to_string()),
            _ => {}
        }
        s = &after[end..];
    }
    rest.push_str(s);
    (markers, rest)
}

/// POSIX `cksum`: the CRC and the length, as the remote's `cksum` prints
/// them, so an edit can check nothing changed the file in between.
pub fn cksum(bytes: &[u8]) -> String {
    fn step(crc: u32, byte: u8) -> u32 {
        let mut c = crc ^ ((byte as u32) << 24);
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
        }
        c
    }
    let mut crc = bytes.iter().fold(0u32, |c, &b| step(c, b));
    let mut len = bytes.len() as u64;
    while len != 0 {
        crc = step(crc, (len & 0xff) as u8);
        len >>= 8;
    }
    format!("{} {}", !crc, bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_escape_everything_a_line_could_break_on() {
        assert_eq!(param("src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(param("a\nb\\c").unwrap(), "a\\0012b\\0134c");
        assert_eq!(param("é").unwrap(), "\\0303\\0251");
        assert_eq!(param("$(rm -rf ~) `x` 'y'").unwrap(), "$(rm -rf ~) `x` 'y'");
        assert!(param("a\0b").is_err());
    }

    #[test]
    fn markers_come_out_even_mid_line() {
        let n = "3f1c";
        let (m, rest) = strip_markers("3f1c start\nwarning: x3f1c exit 2\n", n);
        assert_eq!(
            m,
            Markers {
                started: true,
                exit: Some(2),
                fail: None
            }
        );
        assert_eq!(rest, "warning: x");
        let (m, rest) = strip_markers("3f1c start\n3f1c fail escape\n", n);
        assert_eq!(m.fail.as_deref(), Some("escape"));
        assert_eq!((m.exit, rest.as_str()), (None, ""));
        let (m, _) = strip_markers("3f1c start\npartial", n);
        assert!(m.started && m.exit.is_none());
    }

    #[test]
    fn cksum_matches_posix() {
        // `printf 'hello\n' | cksum` and `printf '' | cksum`.
        assert_eq!(cksum(b"hello\n"), "3015617425 6");
        assert_eq!(cksum(b""), "4294967295 0");
    }

    #[test]
    fn the_header_is_length_script_nonce_params() {
        let h = stdin_header(Op::Read, "N1", &["/w".into(), "x y".into()]).unwrap();
        let h = String::from_utf8(h).unwrap();
        let (len, rest) = h.split_once('\n').unwrap();
        let len: usize = len.parse().unwrap();
        assert!(rest[..len].ends_with("exit \"$ferrule_st\"\n"));
        assert_eq!(&rest[len..], "N1\n/w\nx y\n");
    }

    /// The scripts, run by the local `sh` exactly as the remote would.
    #[cfg(unix)]
    mod sh {
        use super::super::*;
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        fn run(op: Op, params: &[&str], data: &[u8]) -> (Markers, String, String) {
            let nonce = uuid::Uuid::new_v4().to_string();
            let params: Vec<String> = params.iter().map(|s| s.to_string()).collect();
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(
                    BOOTSTRAP
                        .trim_start_matches("exec sh -c '")
                        .trim_end_matches('\''),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // As sshd does: a session of its own, which `shell`'s
                // watchdog may kill whole.
                .process_group(0)
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin
                .write_all(&stdin_header(op, &nonce, &params).unwrap())
                .unwrap();
            stdin.write_all(data).unwrap();
            // `shell` reads EOF as "kill the command": hold it open.
            let held = (op == Op::Shell).then_some(stdin);
            let out = child.wait_with_output().unwrap();
            drop(held);
            let (m, err) = strip_markers(&String::from_utf8_lossy(&out.stderr), &nonce);
            (m, String::from_utf8_lossy(&out.stdout).into_owned(), err)
        }

        fn ws() -> (tempfile::TempDir, String) {
            let dir = tempfile::tempdir().unwrap();
            let real = std::fs::canonicalize(dir.path()).unwrap();
            (dir, real.to_string_lossy().into_owned())
        }

        #[test]
        fn write_read_list_round_trip_odd_names() {
            let (_d, ws) = ws();
            let name = format!("{ws}/sub dir/$(x) é\\n.txt");
            let (m, out, err) = run(Op::Write, &[&ws, "0", "0", &name, ""], b"hi\n\0there");
            assert_eq!(m.exit, Some(0), "{err}");
            assert_eq!(out.trim_end(), name);
            let (m, out, _) = run(Op::Read, &[&ws, "0", "0", &name], b"");
            assert_eq!((m.exit, out.as_str()), (Some(0), "hi\n\0there"));
            let (_, out, _) = run(Op::List, &[&ws, "0", "0", &format!("{ws}/sub dir")], b"");
            assert_eq!(out, "file\t$(x) é\\n.txt\n");
        }

        #[test]
        fn a_symlink_out_is_an_escape_and_denies_hold() {
            let (_d, ws) = ws();
            std::os::unix::fs::symlink("/etc", format!("{ws}/out")).unwrap();
            std::os::unix::fs::symlink("../../../../../../../tmp/nope", format!("{ws}/dangle"))
                .unwrap();
            for p in [
                format!("{ws}/out/passwd"),
                format!("{ws}/dangle"),
                "/etc/passwd".into(),
            ] {
                let (m, _, _) = run(Op::Read, &[&ws, "0", "0", &p], b"");
                assert_eq!(m.fail.as_deref(), Some("escape"), "{p}");
            }
            let (m, _, _) = run(
                Op::Write,
                &[&ws, "0", "0", &format!("{ws}/dangle"), ""],
                b"x",
            );
            assert_eq!(m.fail.as_deref(), Some("escape"));
            std::fs::create_dir(format!("{ws}/secret")).unwrap();
            std::fs::write(format!("{ws}/secret/k"), "k").unwrap();
            let deny = format!("{ws}/secret");
            let (m, _, _) = run(
                Op::Read,
                &[&ws, "0", "1", &deny, &format!("{ws}/secret/k")],
                b"",
            );
            assert_eq!(m.fail.as_deref(), Some("hidden"));
            let (m, _, _) = run(
                Op::Read,
                &[&ws, "0", "1", &deny, &format!("{ws}/secrets")],
                b"",
            );
            assert_eq!(m.fail.as_deref(), Some("missing"));
        }

        #[test]
        fn a_guarded_write_refuses_a_changed_file() {
            let (_d, ws) = ws();
            let f = format!("{ws}/a.txt");
            std::fs::write(&f, "one\n").unwrap();
            let (m, _, _) = run(Op::Write, &[&ws, "0", "0", &f, &cksum(b"two\n")], b"x");
            assert_eq!(m.fail.as_deref(), Some("changed"));
            let (m, _, _) = run(Op::Write, &[&ws, "0", "0", &f, &cksum(b"one\n")], b"x");
            assert_eq!(m.exit, Some(0));
            assert_eq!(std::fs::read_to_string(&f).unwrap(), "x");
            let (m, _, _) = run(Op::Write, &[&ws, "0", "0", &f, "missing"], b"y");
            assert_eq!(m.fail.as_deref(), Some("changed"));
        }

        #[test]
        fn shell_runs_in_the_workspace_with_env() {
            let (_d, ws) = ws();
            let (m, out, err) = run(
                Op::Shell,
                &[
                    &ws,
                    "pwd -P; echo \"$FOO\"; echo oops >&2; exit 4",
                    "1",
                    "FOO=a b\nc",
                ],
                b"",
            );
            assert_eq!(m.exit, Some(4), "{err}");
            assert_eq!(out, format!("{ws}\na b\nc\n"));
            assert_eq!(err, "oops\n");
        }

        #[test]
        fn hello_resolves_home_and_denies() {
            let (_d, ws) = ws();
            let (m, out, err) = run(Op::Hello, &[&ws, "2", "~/.ssh", "rel"], b"");
            assert_eq!(m.exit, Some(0), "{err}");
            assert!(out.contains(&format!("ws\t{ws}\n")), "{out}");
            assert!(out.contains(&format!("deny\t{ws}/rel\n")), "{out}");
            assert!(out.contains("/.ssh\n"), "{out}");
            let (m, _, _) = run(Op::Hello, &["/nonexistent/ferrule", "0"], b"");
            assert_eq!(m.fail.as_deref(), Some("nows"));
        }
    }
}
