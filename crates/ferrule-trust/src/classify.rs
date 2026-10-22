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
    /// M64.0: a POST, PUT or PATCH sent to a host with a bound secret (or
    /// to a host the command line doesn't show). Asked about only once
    /// the run has read outside content: see [`classify_http_write`].
    HttpWrite,
    /// M20: a connected service's tool that doesn't say it only reads.
    ConnectedWrite,
    /// M32: a tool that declares every call needs the owner's approval
    /// (a plugin tool marked `approval` in its manifest).
    DeclaredApproval,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::RecursiveDelete => "recursive delete",
            Kind::ForcePush => "force push",
            Kind::HttpDelete => "DELETE request to a host with a bound secret",
            Kind::HttpWrite => "POST, PUT or PATCH to a host with a bound secret",
            Kind::ConnectedWrite => "change through a connected service",
            Kind::DeclaredApproval => "tool that asks for approval on every call",
        })
    }
}

impl Kind {
    /// M54.5: reaches past this machine and can't be taken back: what an
    /// injected page or email would steer the owner's keys toward.
    pub fn leaves_the_box(self) -> bool {
        matches!(
            self,
            Kind::ForcePush | Kind::HttpDelete | Kind::HttpWrite | Kind::ConnectedWrite
        )
    }
}

/// M54.5: a call whose result is content from outside the owner's control,
/// so the model may be reading someone else's instructions after it: a web
/// page or search, the browser, a connected service's read (email,
/// messages, documents), or a shell command that fetches (`curl`, `wget`,
/// httpie, `xh`, `gh`). A connected service's change doesn't count: its
/// answer is a status, and two sends in a row stay untapped.
pub fn untrusted_source(tool: &str, args: &Value, changes: bool, connected: &[String]) -> bool {
    match tool {
        "web_fetch" | "web_search" => true,
        "shell" => args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(fetches),
        _ => {
            let Some((server, _)) = tool.strip_prefix("mcp__").and_then(|r| r.split_once("__"))
            else {
                return false;
            };
            server == "browser" || (!changes && connected.iter().any(|c| c == server))
        }
    }
}

/// Whether some simple command in `command` is an HTTP client.
fn fetches(command: &str) -> bool {
    segments(command).iter().any(|segment| {
        let words = words(segment);
        unwrap(&words).first().is_some_and(|w| {
            matches!(
                program(w).as_str(),
                "curl" | "wget" | "http" | "https" | "xh" | "xhs" | "gh" | "lynx" | "w3m"
            )
        })
    })
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

/// M56.2: the words a standing "Always here" grant binds a shell call to:
/// `command` must be exactly the one simple command `g` gates, with
/// nothing the shell would expand or chain (no `$`, backquote, glob,
/// brace, `~` or escape), so the words are what runs. `None`: it can be
/// approved once or for the run, never for good.
pub fn grantable_command(command: &str, g: &Gated) -> Option<Vec<String>> {
    let segments = segments(command);
    let [only] = segments.as_slice() else {
        return None;
    };
    let literal = !only.contains(['$', '`', '*', '?', '[', '{', '~', '\\']);
    (literal && only.trim() == g.command).then(|| words(only))
}

/// M56.2: who a connected service's change goes to, lower-cased: the
/// addresses in `to`, `recipient` or `recipients` (a comma-separated
/// string or a list), plus `cc` and `bcc`. Empty when the call names no
/// `to` of its own (a reply that goes to a thread's sender): then no
/// standing grant can cover it.
pub fn recipients(args: &Value) -> Vec<String> {
    let field = |key: &str| -> Vec<String> {
        let raw: Vec<String> = match args.get(key) {
            Some(Value::String(s)) => s.split([',', ';']).map(str::to_string).collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        raw.iter()
            .map(|a| {
                let a = a.trim();
                let inner = match (a.rfind('<'), a.rfind('>')) {
                    (Some(l), Some(r)) if l < r => &a[l + 1..r],
                    _ => a,
                };
                inner.trim().to_lowercase()
            })
            .filter(|a| !a.is_empty())
            .collect()
    };
    let mut out: Vec<String> = ["to", "recipient", "recipients"]
        .iter()
        .flat_map(|k| field(k))
        .collect();
    if out.is_empty() {
        return out;
    }
    out.extend(["cc", "bcc"].iter().flat_map(|k| field(k)));
    out.sort();
    out.dedup();
    out
}

/// Wave A (M54.5): whether a connected service's send goes to the owner
/// alone. It names its addressees (`to`, `recipient`, `recipients`, `cc`,
/// `bcc`, or a `chat_id`), each one exactly one of `own` (the owner's
/// chats and accounts, from the config and the connection store, never
/// from the call); every address in its other arguments but the message's
/// own text (a `from`, a `reply_to`) is one of `own` too; and nothing asks
/// to reply to all. A reply that names no `to` goes to whoever wrote, so
/// it isn't covered.
pub fn only_to(args: &Value, own: &[String]) -> bool {
    const ADDRESSING: &[&str] = &["to", "recipient", "recipients", "cc", "bcc", "chat_id"];
    const TEXT: &[&str] = &[
        "body",
        "text",
        "subject",
        "content",
        "html",
        "message",
        "title",
        "caption",
        "description",
    ];
    fn addresses(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.extend(
                s.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
                    .filter(|w| w.contains('@'))
                    .map(|w| {
                        w.trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')'))
                            .to_lowercase()
                    }),
            ),
            Value::Array(a) => a.iter().for_each(|v| addresses(v, out)),
            Value::Object(o) => o.values().for_each(|v| addresses(v, out)),
            _ => {}
        }
    }
    let Some(fields) = args.as_object() else {
        return false;
    };
    let mut named = recipients(args);
    match args.get("chat_id") {
        None => {}
        Some(Value::String(s)) => named.push(s.trim().to_lowercase()),
        Some(Value::Number(n)) => named.push(n.to_string()),
        Some(_) => return false,
    }
    let mine = |a: &String| own.iter().any(|o| o == a);
    if named.is_empty() || !named.iter().all(mine) {
        return false;
    }
    let mut elsewhere = Vec::new();
    for (key, value) in fields {
        let k = key.to_ascii_lowercase();
        if k.contains("reply_all") {
            return false;
        }
        if !ADDRESSING.contains(&k.as_str()) && !TEXT.contains(&k.as_str()) {
            addresses(value, &mut elsewhere);
        }
    }
    elsewhere.iter().all(mine)
}

/// M20: a tool of a connected service (`mcp__<name>__*`, `name` in
/// `connected`) that can change things: every tool whose server doesn't
/// mark it read-only. The hint only ever skips the gate for a tool that
/// claims to read; a missing one gates.
pub fn classify_connected(tool: &str, changes: bool, connected: &[String]) -> Option<Gated> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, _) = rest.split_once("__")?;
    (changes && connected.iter().any(|c| c == server)).then(|| Gated {
        kind: Kind::ConnectedWrite,
        command: tool.to_string(),
    })
}

/// M32: a tool that declares it needs approval (`Tool::needs_approval`).
pub fn classify_declared(tool: &str, needs_approval: bool) -> Option<Gated> {
    needs_approval.then(|| Gated {
        kind: Kind::DeclaredApproval,
        command: tool.to_string(),
    })
}

pub fn classify_command(command: &str, bound_hosts: &[HostPattern]) -> Option<Gated> {
    scan(command, 0, &|name, args| match name {
        "rm" => rm_is_recursive(args).then_some(Kind::RecursiveDelete),
        "find" => find_deletes(args).then_some(Kind::RecursiveDelete),
        "rsync" => args
            .iter()
            .any(|w| w.starts_with("--delete"))
            .then_some(Kind::RecursiveDelete),
        "git" => git(args),
        _ => request(name, args)
            .filter(|(method, urls)| method == "DELETE" && reaches(urls, bound_hosts))
            .map(|_| Kind::HttpDelete),
    })
}

/// M64.0: a shell command whose HTTP client sends a POST, PUT or PATCH
/// (said with `-X`, or implied: curl's `-d`, `--json`, `-F` and `-T`,
/// wget's `--post-data`, httpie's data items, `gh api` with fields, a
/// `gh` verb that changes something) to a host a secret is bound to, or
/// to a host the command line doesn't show. Not a gate by itself: the
/// guard asks only once the run has read outside content, so the owner's
/// own API calls (a light turned on, an issue filed) stay untapped.
pub fn classify_http_write(tool: &str, args: &Value, bound: &[HostPattern]) -> Option<Gated> {
    if tool != "shell" {
        return None;
    }
    let command = args.get("command")?.as_str()?;
    scan(command, 0, &|name, args| {
        request(name, args)
            .filter(|(method, urls)| {
                matches!(method.as_str(), "POST" | "PUT" | "PATCH") && reaches(urls, bound)
            })
            .map(|_| Kind::HttpWrite)
    })
}

/// Each simple command in `command` (and inside `sh -c` and `eval`), its
/// program's name and its arguments given to `matcher`; the first kind it
/// names, with that part of the command.
fn scan(
    command: &str,
    depth: usize,
    matcher: &dyn Fn(&str, &[String]) -> Option<Kind>,
) -> Option<Gated> {
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
            if let Some(g) = scan(&inner, depth + 1, matcher) {
                return Some(g);
            }
            continue;
        }
        if let Some(kind) = matcher(&name, &words[1..]) {
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

/// Whether a request to `urls` may reach a host a secret is bound to: one
/// of them is, or none was found, or one's host can't be told (a variable
/// in it). The last two can't be told apart from a bound one, so they ask.
fn reaches(urls: &[String], bound: &[HostPattern]) -> bool {
    urls.is_empty()
        || urls.iter().any(|u| match host_of(u) {
            None => true,
            Some(h) => bound.iter().any(|p| p.matches(&h)),
        })
}

/// What an HTTP client's command line sends: its method, upper-cased, and
/// the URLs it names. `None`: not an HTTP client this reads, or a `gh`
/// command that only reads.
fn request(program: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    match program {
        "curl" => Some(curl(args)),
        "wget" => Some(wget(args)),
        "gh" => gh(args),
        "http" | "https" | "xh" | "xhs" => Some(httpie(args)),
        _ => None,
    }
}

/// A command line read the getopt way: `--name value`, `--name=value`,
/// `-x value`, `-xvalue` and clusters (`-sX POST`), where `long` and
/// `short` list the options that take a value. `seen` gets each option's
/// name (a long one without its `--`, a short one as its letter) and its
/// value; the words that aren't options or their values are returned.
fn options<'a>(
    args: &'a [String],
    long: &[&str],
    short: &str,
    mut seen: impl FnMut(&str, Option<&'a str>),
) -> Vec<&'a String> {
    let mut rest = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        i += 1;
        if a == "--" {
            rest.extend(&args[i..]);
            break;
        }
        if let Some(name) = a.strip_prefix("--") {
            match name.split_once('=') {
                Some((n, v)) => seen(n, Some(v)),
                None if long.contains(&name) => {
                    seen(name, args.get(i).map(String::as_str));
                    i += 1;
                }
                None => seen(name, None),
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for (at, c) in a.char_indices().skip(1) {
                let mut buf = [0u8; 4];
                let letter = c.encode_utf8(&mut buf);
                if short.contains(c) {
                    let tail = &a[at + c.len_utf8()..];
                    if tail.is_empty() {
                        seen(letter, args.get(i).map(String::as_str));
                        i += 1;
                    } else {
                        seen(letter, Some(tail));
                    }
                    break;
                }
                seen(letter, None);
            }
        } else {
            rest.push(a);
        }
    }
    rest
}

/// curl: `-X` names the method; else `-I` is a HEAD, `-G` a GET, `-T` a
/// PUT and a body (`-d`, `--data…`, `--json`, `-F`, `--form…`) a POST.
fn curl(args: &[String]) -> (String, Vec<String>) {
    const LONG: &[&str] = &[
        "request",
        "data",
        "data-raw",
        "data-binary",
        "data-ascii",
        "data-urlencode",
        "json",
        "form",
        "form-string",
        "header",
        "proxy-header",
        "output",
        "output-dir",
        "user",
        "user-agent",
        "referer",
        "cookie",
        "cookie-jar",
        "proxy",
        "proxy-user",
        "preproxy",
        "upload-file",
        "write-out",
        "max-time",
        "connect-timeout",
        "config",
        "resolve",
        "connect-to",
        "cacert",
        "capath",
        "cert",
        "key",
        "pass",
        "range",
        "retry",
        "retry-delay",
        "retry-max-time",
        "url",
        "oauth2-bearer",
        "unix-socket",
        "abstract-unix-socket",
        "dump-header",
        "trace",
        "trace-ascii",
        "stderr",
        "variable",
        "aws-sigv4",
        "limit-rate",
        "max-filesize",
        "max-redirs",
        "interface",
        "noproxy",
        "proto",
        "proto-redir",
        "request-target",
        "time-cond",
        "continue-at",
        "quote",
        "speed-limit",
        "speed-time",
        "netrc-file",
        "socks4",
        "socks4a",
        "socks5",
        "socks5-hostname",
    ];
    const SHORT: &str = "XdFHouAebcxTwmKrzEUQYyCPtD";
    let (mut method, mut body, mut upload, mut get, mut head) = (None, false, false, false, false);
    let mut urls = Vec::new();
    let rest = options(args, LONG, SHORT, |name, value| match name {
        "X" | "request" => method = value.map(str::to_uppercase),
        "d" | "data" | "data-raw" | "data-binary" | "data-ascii" | "data-urlencode" | "json"
        | "F" | "form" | "form-string" => body = true,
        "T" | "upload-file" => upload = true,
        "G" | "get" => get = true,
        "I" | "head" => head = true,
        "url" => urls.extend(value.map(str::to_string)),
        _ => {}
    });
    urls.extend(rest.into_iter().filter(|a| looks_like_url(a)).cloned());
    let implied = match () {
        _ if head => "HEAD",
        _ if get => "GET",
        _ if upload => "PUT",
        _ if body => "POST",
        _ => "GET",
    };
    (method.unwrap_or_else(|| implied.into()), urls)
}

/// wget: `--method` names it; else `--post-data` or `--post-file` is a
/// POST.
fn wget(args: &[String]) -> (String, Vec<String>) {
    const LONG: &[&str] = &[
        "method",
        "post-data",
        "post-file",
        "body-data",
        "body-file",
        "header",
        "output-document",
        "output-file",
        "append-output",
        "user-agent",
        "user",
        "password",
        "http-user",
        "http-password",
        "directory-prefix",
        "input-file",
        "tries",
        "timeout",
        "wait",
        "quota",
        "level",
        "domains",
        "accept",
        "reject",
        "include-directories",
        "exclude-directories",
        "referer",
        "load-cookies",
        "save-cookies",
        "ca-certificate",
        "certificate",
        "private-key",
        "execute",
        "base",
        "config",
    ];
    const SHORT: &str = "OoaiPUeTtwQlDARIXB";
    let (mut method, mut post) = (None, false);
    let rest = options(args, LONG, SHORT, |name, value| match name {
        "method" => method = value.map(str::to_uppercase),
        "post-data" | "post-file" => post = true,
        _ => {}
    });
    let urls = rest
        .into_iter()
        .filter(|a| looks_like_url(a))
        .cloned()
        .collect();
    let implied = if post { "POST" } else { "GET" };
    (method.unwrap_or_else(|| implied.into()), urls)
}

/// `gh api`: `-X`/`--method` names it, else a field (`-f`, `-F`,
/// `--input`) makes it a POST; the host is `--hostname`'s, else
/// api.github.com. Any other `gh` command is a POST to api.github.com when
/// its verb changes something (`gh issue create`, `gh pr merge`, `gh
/// release upload`, `gh secret set`…); `None` when it reads.
fn gh(args: &[String]) -> Option<(String, Vec<String>)> {
    let to = |host: Option<&str>| vec![format!("https://{}/", host.unwrap_or("api.github.com"))];
    if args.first().is_some_and(|a| a == "api") {
        const LONG: &[&str] = &[
            "method",
            "field",
            "raw-field",
            "input",
            "hostname",
            "header",
            "jq",
            "template",
            "cache",
            "preview",
        ];
        let (mut method, mut fields, mut host) = (None, false, None);
        options(&args[1..], LONG, "XfFHqtp", |name, value| match name {
            "X" | "method" => method = value.map(str::to_uppercase),
            "f" | "F" | "field" | "raw-field" | "input" => fields = true,
            "hostname" => host = value.map(str::to_string),
            _ => {}
        });
        let implied = if fields { "POST" } else { "GET" };
        return Some((
            method.unwrap_or_else(|| implied.into()),
            to(host.as_deref()),
        ));
    }
    const CHANGES: &[&str] = &[
        "create",
        "edit",
        "delete",
        "delete-asset",
        "close",
        "reopen",
        "merge",
        "comment",
        "review",
        "ready",
        "lock",
        "unlock",
        "transfer",
        "pin",
        "unpin",
        "archive",
        "unarchive",
        "rename",
        "fork",
        "sync",
        "upload",
        "rerun",
        "cancel",
        "enable",
        "disable",
        "run",
        "set",
        "remove",
        "add",
        "revert",
        "update-branch",
        "develop",
        "link",
        "unlink",
        "copy",
    ];
    let verb = args.get(1)?.as_str();
    (CHANGES.contains(&verb) || verb.starts_with("item-") || verb.starts_with("field-"))
        .then(|| ("POST".to_string(), to(None)))
}

/// httpie and xh: `http [flags] [METHOD] URL [ITEM…]`. Without a method, a
/// data item (`a=b`, `a:=1`, `a@file`) or `--raw` makes it a POST.
fn httpie(args: &[String]) -> (String, Vec<String>) {
    const LONG: &[&str] = &[
        "auth",
        "auth-type",
        "output",
        "session",
        "session-read-only",
        "verify",
        "cert",
        "cert-key",
        "cert-key-pass",
        "ciphers",
        "ssl",
        "timeout",
        "max-redirects",
        "default-scheme",
        "format-options",
        "pretty",
        "style",
        "print",
        "history-print",
        "boundary",
        "raw",
        "proxy",
        "response-charset",
        "response-mime",
    ];
    let mut raw = false;
    let rest = options(args, LONG, "aAospP", |name, _| {
        if name == "raw" {
            raw = true;
        }
    });
    let (method, rest) = match rest.split_first() {
        Some((m, more)) if is_method(m) => (Some(m.to_uppercase()), more),
        _ => (None, &rest[..]),
    };
    // `:3000/x` is httpie's short form for localhost.
    let url = rest.first().map(|u| match u.strip_prefix(':') {
        Some(port) => format!("localhost:{port}"),
        None => u.to_string(),
    });
    let implied = if raw || rest.iter().skip(1).any(|i| data_item(i)) {
        "POST"
    } else {
        "GET"
    };
    (
        method.unwrap_or_else(|| implied.into()),
        url.into_iter().collect(),
    )
}

fn is_method(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

/// An httpie request item that is data (`a=b`, `a:=1`, `a@file`, `a=@f`),
/// not a header (`a:b`) or a query parameter (`a==b`).
fn data_item(item: &str) -> bool {
    let Some(at) = item.find([':', '=', '@']) else {
        return false;
    };
    let sep = &item[at..];
    sep.starts_with(":=")
        || (sep.starts_with('=') && !sep.starts_with("=="))
        || sep.starts_with('@')
}

/// A word that looks like a URL: with a scheme, a variable, `localhost`,
/// or a dotted host name.
fn looks_like_url(a: &str) -> bool {
    a.contains("://")
        || a.starts_with('$')
        || a.starts_with("localhost")
        || a.split(['/', ':']).next().is_some_and(|h| {
            h.contains('.')
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                && h.chars().any(|c| c.is_ascii_alphabetic())
        })
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

    #[test]
    fn only_a_connected_servers_changing_tools_are_gated() {
        let c = vec!["notion".to_string()];
        assert!(classify_connected("mcp__notion__create", true, &c).is_some());
        assert!(classify_connected("mcp__notion__search", false, &c).is_none());
        assert!(classify_connected("mcp__notionx__create", true, &c).is_none());
        assert!(classify_connected("notion__create", true, &c).is_none());
        assert!(classify_connected("mcp__notion", true, &c).is_none());
    }

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
