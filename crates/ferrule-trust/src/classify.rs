//! Which shell commands need the owner's "yes" (M19 approval gates).
//!
//! Conservative on purpose: a command is split on every separator the
//! shell has (`;`, `&&`, `||`, `|`, `&`, newlines, `$(`, backticks and
//! parentheses), even inside double quotes, `sh -c` and `eval` are looked
//! into, and wrappers like `sudo` and `xargs` are skipped. A false positive
//! costs the owner one "yes".
//!
//! What it can't see, by design: anything a program does that the command
//! line doesn't show (a script file, `make`, `python -c`, a variable as the
//! command, an alias, a git hook), and tools other than the shell. The OS
//! sandbox is the boundary; this is a speed bump for the honest mistake.

use ferrule_proxy::HostPattern;
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    RecursiveDelete,
    ForcePush,
    HttpDelete,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::RecursiveDelete => "recursive delete",
            Kind::ForcePush => "force push",
            Kind::HttpDelete => "DELETE request to a host with a bound secret",
        })
    }
}

/// A gated command: what kind, and the part of the command that is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    pub kind: Kind,
    pub command: String,
}

/// The gate's verdict on one tool call: `Some` needs the owner's approval.
pub fn classify(tool: &str, args: &Value, bound_hosts: &[HostPattern]) -> Option<Gated> {
    if tool != "shell" {
        return None;
    }
    let command = args.get("command")?.as_str()?;
    classify_command(command, bound_hosts)
}

pub fn classify_command(command: &str, bound_hosts: &[HostPattern]) -> Option<Gated> {
    classify_depth(command, bound_hosts, 0)
}

fn classify_depth(command: &str, bound: &[HostPattern], depth: usize) -> Option<Gated> {
    if depth > 4 {
        return None;
    }
    for segment in segments(command) {
        let words = words(&segment);
        let words = unwrap(&words);
        let Some(first) = words.first() else { continue };
        let name = program(first);
        // A shell given a script on its command line, and `eval`: look inside.
        let inner = match name.as_str() {
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => words
                .iter()
                .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
                .and_then(|i| words.get(i + 1))
                .cloned(),
            "eval" => Some(words[1..].join(" ")),
            _ => None,
        };
        if let Some(inner) = inner {
            if let Some(g) = classify_depth(&inner, bound, depth + 1) {
                return Some(g);
            }
            continue;
        }
        let kind = match name.as_str() {
            "rm" => rm_is_recursive(&words[1..]).then_some(Kind::RecursiveDelete),
            "find" => find_deletes(&words[1..]).then_some(Kind::RecursiveDelete),
            "rsync" => words[1..]
                .iter()
                .any(|w| w.starts_with("--delete"))
                .then_some(Kind::RecursiveDelete),
            "git" => git(&words[1..]),
            "curl" | "wget" | "http" | "https" | "xh" | "xhs" | "gh" => {
                http_delete(&name, &words[1..], bound).then_some(Kind::HttpDelete)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            return Some(Gated {
                kind,
                command: segment.trim().to_string(),
            });
        }
    }
    None
}

/// The command split on every shell separator outside single quotes.
fn segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut single = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if single {
            cur.push(c);
            if c == '\'' {
                single = false;
            }
            continue;
        }
        match c {
            '\'' => {
                single = true;
                cur.push(c);
            }
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ';' | '&' | '|' | '\n' | '`' | '(' | ')' | '{' | '}' => {
                out.push(std::mem::take(&mut cur));
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.retain(|s| !s.trim().is_empty());
    out
}

/// One simple command's words, quotes removed.
fn words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A program's name: its path's last part, lower-cased (a case-insensitive
/// file system runs `RM` as `rm`).
fn program(word: &str) -> String {
    word.rsplit('/').next().unwrap_or(word).to_lowercase()
}

/// Skips what only runs the real command: `sudo`, `env`, `xargs`, `VAR=x`…
fn unwrap(words: &[String]) -> Vec<String> {
    let mut i = 0;
    loop {
        let Some(w) = words.get(i) else {
            return Vec::new();
        };
        let name = program(w);
        let (flags_with_arg, positional): (&[&str], usize) = match name.as_str() {
            "sudo" | "doas" => (&["-u", "-g", "-h", "-p", "-c", "-r", "-t", "-d"], 0),
            "env" => (&["-u", "-c", "-s", "-C", "-S"], 0),
            "nice" => (&["-n"], 0),
            "ionice" => (&["-c", "-n", "-p"], 0),
            "xargs" => (
                &["-n", "-i", "-I", "-l", "-L", "-P", "-d", "-s", "-E", "-a"],
                0,
            ),
            "timeout" => (&["-s", "-k", "--signal", "--kill-after"], 1),
            "nohup" | "time" | "command" | "exec" | "builtin" | "stdbuf" | "unbuffer"
            | "chronic" | "caffeinate" | "setsid" => (&["-o", "-e", "-i"], 0),
            _ if w.contains('=') && !w.starts_with('-') && !w.starts_with('=') => {
                i += 1;
                continue;
            }
            _ => return words[i..].to_vec(),
        };
        i += 1;
        let mut positional = positional;
        while let Some(w) = words.get(i) {
            if w.starts_with('-') {
                i += if flags_with_arg.contains(&w.as_str()) {
                    2
                } else {
                    1
                };
            } else if name == "env" && w.contains('=') {
                i += 1;
            } else if positional > 0 {
                positional -= 1;
                i += 1;
            } else {
                break;
            }
        }
    }
}

fn rm_is_recursive(args: &[String]) -> bool {
    for a in args {
        if a == "--" {
            return false;
        }
        if a == "--recursive" {
            return true;
        }
        if a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R']) {
            return true;
        }
    }
    false
}

fn find_deletes(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, a)| {
        a == "-delete"
            || (matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
                && args
                    .get(i + 1)
                    .is_some_and(|p| matches!(program(p).as_str(), "rm" | "rmdir" | "shred")))
    })
}

/// `git [global options] <command> …`: a forced push or a forced clean.
fn git(args: &[String]) -> Option<Kind> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if matches!(
            a.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let sub = args.get(i)?.to_lowercase();
    let rest = &args[i + 1..];
    match sub.as_str() {
        "push" => rest
            .iter()
            .any(|a| {
                matches!(
                    a.as_str(),
                    "--force" | "--force-if-includes" | "--mirror" | "--delete" | "--prune"
                ) || a.starts_with("--force-with-lease")
                    || (a.starts_with('-') && !a.starts_with("--") && a.contains(['f', 'd']))
                    || a.starts_with('+')
                    || a.starts_with(':')
                    || a.contains(":+")
            })
            .then_some(Kind::ForcePush),
        "clean" => rest
            .iter()
            .any(|a| {
                a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            })
            .then_some(Kind::RecursiveDelete),
        _ => None,
    }
}

/// A DELETE sent by an HTTP client to a host a secret is bound to, or to a
/// host the command line doesn't show.
fn http_delete(program: &str, args: &[String], bound: &[HostPattern]) -> bool {
    let (is_delete, urls) = match program {
        "curl" => (
            curl_method(args).is_some_and(|m| m.eq_ignore_ascii_case("delete")),
            positional_urls(args),
        ),
        "wget" => (
            args.iter().enumerate().any(|(i, a)| {
                a.eq_ignore_ascii_case("--method=delete")
                    || (a == "--method"
                        && args
                            .get(i + 1)
                            .is_some_and(|m| m.eq_ignore_ascii_case("delete")))
            }),
            positional_urls(args),
        ),
        "gh" => {
            let is_api = args.first().is_some_and(|a| a == "api");
            let method = args.iter().enumerate().find_map(|(i, a)| match a.as_str() {
                "-X" | "--method" => args.get(i + 1).cloned(),
                _ => a
                    .strip_prefix("--method=")
                    .or_else(|| a.strip_prefix("-X"))
                    .filter(|m| !m.is_empty())
                    .map(str::to_string),
            });
            let host = args
                .iter()
                .position(|a| a == "--hostname")
                .and_then(|i| args.get(i + 1).cloned())
                .unwrap_or_else(|| "api.github.com".into());
            (
                is_api && method.is_some_and(|m| m.eq_ignore_ascii_case("delete")),
                vec![format!("https://{host}/")],
            )
        }
        // httpie and xh: `http [flags] DELETE url`.
        _ => {
            let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            match pos.first() {
                Some(m) if m.eq_ignore_ascii_case("delete") => (
                    true,
                    pos.get(1).map(|u| vec![u.to_string()]).unwrap_or_default(),
                ),
                _ => (false, Vec::new()),
            }
        }
    };
    if !is_delete {
        return false;
    }
    let hosts: Vec<Option<String>> = urls.iter().map(|u| host_of(u)).collect();
    // No URL, or one whose host is a variable: can't tell, so ask.
    hosts.is_empty()
        || hosts.iter().any(|h| match h {
            None => true,
            Some(h) => bound.iter().any(|p| p.matches(h)),
        })
}

fn curl_method(args: &[String]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == "--request" || a == "-X" {
            return args.get(i + 1).cloned();
        }
        if let Some(m) = a.strip_prefix("--request=") {
            return Some(m.to_string());
        }
        if a.starts_with('-') && !a.starts_with("--") {
            if let Some(at) = a.find('X') {
                let rest = &a[at + 1..];
                return if rest.is_empty() {
                    args.get(i + 1).cloned()
                } else {
                    Some(rest.to_string())
                };
            }
        }
    }
    None
}

/// Arguments that look like URLs: with a scheme, or a dotted host name.
fn positional_urls(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with('-'))
        .filter(|a| {
            a.contains("://")
                || a.starts_with('$')
                || a.starts_with("localhost")
                || a.split(['/', ':']).next().is_some_and(|h| {
                    h.contains('.')
                        && h.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                        && h.chars().any(|c| c.is_ascii_alphabetic())
                })
        })
        .cloned()
        .collect()
}

/// A URL's host, `None` when it can't be told (a variable in it).
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    if host.is_empty() || host.contains(['$', '{', '}', '*']) {
        return None;
    }
    Some(host.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound() -> Vec<HostPattern> {
        ["api.github.com", "*.example.com"]
            .iter()
            .map(|h| HostPattern::parse(h).unwrap())
            .collect()
    }

    fn kind(cmd: &str) -> Option<Kind> {
        classify_command(cmd, &bound()).map(|g| g.kind)
    }

    #[test]
    fn recursive_deletes_are_gated() {
        for cmd in [
            "rm -rf build",
            "rm -r x",
            "rm -R x",
            "rm -fr x",
            "rm -vrf x",
            "rm --recursive x",
            "rm -f -r x",
            "/bin/rm -rf x",
            "'rm' -rf x",
            "\\rm -rf x",
            "RM -RF x",
            "sudo rm -rf /tmp/x",
            "sudo -u root rm -rf x",
            "env FOO=1 rm -rf x",
            "FOO=1 rm -rf x",
            "nice -n 5 rm -rf x",
            "timeout 10 rm -rf x",
            "find . -name '*.o' | xargs rm -rf",
            "find . -name '*.o' | xargs -0 -n 1 rm -r",
            "find . -type d -delete",
            "find . -exec rm {} \\;",
            "find . -execdir /bin/rm -f {} +",
            "git clean -fdx",
            "git clean -f",
            "git -C repo clean --force",
            "rsync -a --delete src/ dst/",
            "cd x && rm -rf y",
            "true; rm -rf y",
            "false || rm -rf y",
            "echo $(rm -rf y)",
            "echo `rm -rf y`",
            "echo \"$(rm -rf y)\"",
            "(cd x; rm -rf y)",
            "{ rm -rf y; }",
            "sh -c 'rm -rf y'",
            "bash -lc \"cd x && rm -rf y\"",
            "bash -c 'sh -c \"rm -rf y\"'",
            "eval 'rm -rf y'",
            "ls\nrm -rf y",
            "sleep 1 & rm -rf y",
        ] {
            assert_eq!(kind(cmd), Some(Kind::RecursiveDelete), "{cmd}");
        }
    }

    #[test]
    fn force_pushes_are_gated() {
        for cmd in [
            "git push --force",
            "git push -f origin main",
            "git push -fu origin main",
            "git push -uf origin main",
            "git push --force-with-lease",
            "git push --force-with-lease=main:abc origin main",
            "git push --force-if-includes",
            "git push origin +main",
            "git push origin +HEAD:main",
            "git push origin HEAD:+main",
            "git push origin :old-branch",
            "git push --delete origin old",
            "git push -d origin old",
            "git push --mirror",
            "git push --prune origin",
            "git -C repo push -f",
            "git -c core.x=y push --force",
            "GIT_TRACE=1 git push -f",
            "git status && git push -f",
        ] {
            assert_eq!(kind(cmd), Some(Kind::ForcePush), "{cmd}");
        }
    }

    #[test]
    fn deletes_to_bound_hosts_are_gated() {
        for cmd in [
            "curl -X DELETE https://api.github.com/repos/o/r",
            "curl -XDELETE https://api.github.com/repos/o/r",
            "curl -sX DELETE https://api.github.com/x",
            "curl -sXDELETE https://api.github.com/x",
            "curl --request DELETE https://api.github.com/x",
            "curl --request=delete https://api.github.com/x",
            "curl -H 'Authorization: token x' -X DELETE https://api.github.com/x",
            "curl -X DELETE https://a.example.com/v1/x",
            "curl -X DELETE a.example.com/v1/x",
            "curl -X DELETE \"$URL\"",
            "curl -X DELETE https://$HOST/x",
            "curl -X DELETE",
            "wget --method=DELETE https://api.github.com/x",
            "wget --method DELETE https://api.github.com/x",
            "http DELETE https://api.github.com/x",
            "xh delete api.github.com/x",
            "gh api -X DELETE repos/o/r",
            "gh api --method DELETE repos/o/r",
            "gh api --method=DELETE repos/o/r",
        ] {
            assert_eq!(kind(cmd), Some(Kind::HttpDelete), "{cmd}");
        }
    }

    #[test]
    fn everyday_commands_pass() {
        for cmd in [
            "rm -f file.txt",
            "rm file",
            "rm -- -r",
            "rmdir empty",
            "ls -R",
            "grep -r foo .",
            "cp -r a b",
            "find . -name '*.rs'",
            "git push",
            "git push origin main",
            "git push -u origin feature",
            "git push --set-upstream origin feature",
            "git push --tags",
            "git clean -n",
            "git clean --dry-run",
            "git status",
            "git log --format=%H",
            "git commit -m 'rm -rf is dangerous'",
            "echo 'rm -rf /'",
            "curl https://api.github.com/x",
            "curl -X POST https://api.github.com/x",
            "curl -X DELETE https://unbound.org/x",
            "curl -X DELETE https://example.com/x",
            "http GET https://api.github.com/x",
            "gh api repos/o/r",
            "gh pr list",
            "cargo test -- --nocapture",
            "sh script.sh",
            "rsync -a src/ dst/",
        ] {
            assert_eq!(kind(cmd), None, "{cmd}");
        }
    }

    /// The documented blind spots: these run the same destruction, and the
    /// gate doesn't see it. The sandbox is what limits them.
    #[test]
    fn what_it_cannot_see() {
        for cmd in [
            "python3 -c 'import shutil; shutil.rmtree(\"x\")'",
            "node -e 'require(\"fs\").rmSync(\"x\", {recursive: true})'",
            "./clean.sh",
            "make clean",
            "$CMD -rf x",
            "echo cm0gLXJmIHgK | base64 -d | sh",
            "git reset --hard HEAD~3",
        ] {
            assert_eq!(kind(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn only_the_shell_tool_is_classified() {
        let args = serde_json::json!({"command": "rm -rf x"});
        assert!(classify("shell", &args, &[]).is_some());
        assert!(classify("write_file", &args, &[]).is_none());
        let g = classify(
            "shell",
            &serde_json::json!({"command": "ls; rm -rf x"}),
            &[],
        )
        .unwrap();
        assert_eq!(g.command, "rm -rf x");
    }
}
