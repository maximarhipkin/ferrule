//! System packages (M54 §6.1): the bot's only two root actions, `apt-get
//! install` and `apt-get remove`, through `sudo -n` by absolute path.
//! Names are checked before sudo runs; an install or a removal that would
//! take the machine's own tools with it is refused after a dry run.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

const SUDO: &str = "/usr/bin/sudo";
const ENV: &str = "/usr/bin/env";
const APT: &str = "/usr/bin/apt-get";

/// How long one apt-get run may take.
const LIMIT: Duration = Duration::from_secs(600);

/// How many of apt's last lines the answer keeps.
const SHOWN: usize = 20;

/// What an install or a removal may never take with it: the ways in and
/// the way to fix things.
const KEPT: [&str; 5] = [
    "sudo",
    "apt",
    "openssh-server",
    "systemd",
    "ca-certificates",
];

const NO_APT: &str = "System packages are apt-only, and this machine has no apt.";

const NO_SUDO: &str = "This account can't use sudo without a password, so I can't install system \
packages. User-level installs (pipx, uv, npm, a binary in ~/.local/bin) still work from my shell.";

/// A package name as apt takes it: `^[a-z0-9][a-z0-9+.-]*$`, at most 128
/// characters, not a `.deb` file. A path, an option or a space never is,
/// and nor is a trailing `-`, which apt reads as "remove" instead (a
/// trailing `+` is a real name, `g++`; the dry run's check catches apt
/// reading it as "install").
pub fn valid(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
        && !name.ends_with(".deb")
        && !name.ends_with('-')
}

/// Why `names` can't be installed or removed here, said before anything
/// runs (the card's check, and again before sudo).
pub fn check(names: &[String]) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !Path::new(APT).exists() {
        return Err(NO_APT.into());
    }
    if names.is_empty() {
        return Err("name at least one package".into());
    }
    if let Some(bad) = names.iter().find(|n| !valid(n)) {
        return Err(format!(
            "`{bad}` isn't a package name: lower-case letters, digits, + . and - (- never last), \
             no paths, .deb files or options"
        ));
    }
    Ok(())
}

/// `apt-get install` the `names`, once a dry run shows it removes none of
/// [`KEPT`] (an install can, for a package that conflicts); when apt
/// doesn't know one, `update` once first. Then `clean`: the server is
/// short on disk.
pub async fn install(names: &[String]) -> Result<String, String> {
    check(names)?;
    let flags = ["--no-install-recommends"];
    let mut plan = apt(&args("install", true, &flags, names)).await?;
    if plan.1.contains("Unable to locate package") {
        apt(&["update", "-y"]).await?;
        plan = apt(&args("install", true, &flags, names)).await?;
    }
    guard("install", names, plan)?;
    let done = apt(&args("install", false, &flags, names)).await?;
    let _ = apt(&["clean", "-y"]).await;
    said("install", done)
}

/// `apt-get remove` the `names`, once a dry run shows it takes none of
/// [`KEPT`] with it. Then `clean`.
pub async fn remove(names: &[String]) -> Result<String, String> {
    check(names)?;
    guard(
        "remove",
        names,
        apt(&args("remove", true, &[], names)).await?,
    )?;
    let done = apt(&args("remove", false, &[], names)).await?;
    let _ = apt(&["clean", "-y"]).await;
    said("remove", done)
}

/// `apt-get`'s arguments: `<verb> -y [-s] <flags…> -- <names…>`.
fn args<'a>(verb: &'a str, dry: bool, flags: &[&'a str], names: &'a [String]) -> Vec<&'a str> {
    let mut v = vec![verb, "-y"];
    if dry {
        v.push("-s");
    }
    v.extend(flags.iter().copied());
    v.push("--");
    v.extend(names.iter().map(String::as_str));
    v
}

/// A dry run's verdict (review focus 4): refused when it failed, when apt
/// didn't take a name as exactly that package (a regex, a substitute), or,
/// with the list, when a `Remv <pkg> [<version>]` line names one of
/// [`KEPT`] (an `:arch` is left off).
fn guard(verb: &str, names: &[String], (ok, plan): (bool, String)) -> Result<(), String> {
    if !ok {
        return said(verb, (ok, plan)).map(|_| ());
    }
    if let Some(name) = names.iter().find(|n| !planned(verb, n, &plan)) {
        return Err(format!("apt didn't read `{name}` as one package"));
    }
    let kept: Vec<&str> = plan
        .lines()
        .filter_map(|l| l.strip_prefix("Remv "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|p| p.split(':').next().unwrap_or(p))
        .filter(|p| KEPT.contains(p))
        .collect();
    if kept.is_empty() {
        return Ok(());
    }
    Err(format!(
        "apt-get {verb} {} would also remove {}, which this machine needs to be reached and \
         fixed; nothing was changed",
        names.join(", "),
        kept.join(", ")
    ))
}

/// Whether a dry run of `verb` names `name` itself as a package: for an
/// install an `Inst <name> ` line or "<name> is already the newest
/// version", for a removal a `Remv <name> ` line or "<name> is not
/// installed" (with or without `Package` and quotes, as apt versions say it).
fn planned(verb: &str, name: &str, plan: &str) -> bool {
    plan.lines().any(|l| {
        let says = |what: &str| {
            let rest = l.strip_prefix("Package ").unwrap_or(l);
            rest.starts_with(&format!("{name} {what}"))
                || rest.starts_with(&format!("'{name}' {what}"))
        };
        if verb == "install" {
            l.starts_with(&format!("Inst {name} ")) || says("is already the newest version")
        } else {
            l.starts_with(&format!("Remv {name} ")) || says("is not installed")
        }
    })
}

/// One `sudo -n env DEBIAN_FRONTEND=noninteractive apt-get <args…>` within
/// [`LIMIT`], in the C locale so the lines matched here are apt's and
/// sudo's English ones: whether it worked, and what it printed.
async fn apt(args: &[&str]) -> Result<(bool, String), String> {
    let verb = args.first().copied().unwrap_or_default();
    let run = tokio::process::Command::new(SUDO)
        .args(["-n", ENV, "DEBIAN_FRONTEND=noninteractive", APT])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(out) = tokio::time::timeout(LIMIT, run).await else {
        return Err(format!(
            "apt-get {verb} took over 10 minutes; I stopped waiting, and it may still be running"
        ));
    };
    let out = out.map_err(|e| format!("couldn't run {SUDO}: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success()
        && (text.contains("a password is required") || text.contains("a terminal is required"))
    {
        return Err(NO_SUDO.into());
    }
    Ok((out.status.success(), text))
}

/// The answer: apt's last lines, as a result or as why it failed.
fn said(verb: &str, (ok, text): (bool, String)) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let last = lines[lines.len().saturating_sub(SHOWN)..].join("\n");
    if ok {
        Ok(format!("apt-get {verb} finished:\n{last}"))
    } else {
        Err(format!("apt-get {verb} failed:\n{last}"))
    }
}
