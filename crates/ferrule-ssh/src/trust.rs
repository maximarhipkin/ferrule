//! First contact with a host (docs/m34-ssh-local.md §4): fetch its keys
//! with `ssh-keyscan`, show their fingerprints, and only on the owner's
//! yes append them to ferrule's own known_hosts. ssh itself never accepts
//! a key (`StrictHostKeyChecking=yes`), so this is the only way in.

use crate::link::ssh_program;
use crate::target::Target;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// One host key as scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    /// The known_hosts line, named as ssh will look it up.
    pub line: String,
    /// `ssh-ed25519`, `ecdsa-sha2-nistp256` …
    pub key_type: String,
    /// `SHA256:…`, as `ssh-keygen -l` and `ssh` print it.
    pub fingerprint: String,
}

/// Where ssh really connects, from `ssh -G` (the ssh config applied):
/// the host name, the port, and the name known_hosts is checked under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub hostname: String,
    pub port: u16,
    pub known_as: String,
}

/// A program installed beside `ssh` (`ssh-keyscan`, `ssh-keygen`), else on
/// `PATH`.
pub fn sibling(target: &Target, name: &str) -> PathBuf {
    let ssh = ssh_program(target);
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    match ssh.parent() {
        Some(dir) if !dir.as_os_str().is_empty() && dir.join(&exe).exists() => dir.join(exe),
        _ => PathBuf::from(name),
    }
}

async fn output(
    mut cmd: tokio::process::Command,
    input: Option<&[u8]>,
    what: &str,
) -> Result<std::process::Output, String> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("can't run {what}: {e}"))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(bytes).await;
    }
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .map_err(|_| format!("{what} timed out"))?
        .map_err(|e| format!("{what}: {e}"))
}

/// `ssh -G`: the settings ssh would use for the target.
pub async fn resolve(target: &Target) -> Result<Resolved, String> {
    let mut c = tokio::process::Command::new(ssh_program(target));
    c.arg("-G");
    if let Some(p) = target.port {
        c.arg("-p").arg(p.to_string());
    }
    if let Some(u) = &target.user {
        c.arg("-l").arg(u);
    }
    if let Some(cfg) = &target.ssh_config {
        c.arg("-F").arg(cfg);
    }
    c.arg("--").arg(&target.host);
    let out = output(c, None, "ssh -G").await?;
    if !out.status.success() {
        return Err(format!(
            "ssh -G {}: {}",
            target.host,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(' ')))
            .map(str::to_string)
    };
    let hostname = get("hostname").unwrap_or_else(|| target.host.clone());
    let port = get("port").and_then(|p| p.parse().ok()).unwrap_or(22);
    let base = get("hostkeyalias")
        .filter(|a| !a.is_empty() && a != "none")
        .unwrap_or_else(|| hostname.clone());
    let known_as = if port == 22 {
        base
    } else {
        format!("[{base}]:{port}")
    };
    Ok(Resolved {
        hostname,
        port,
        known_as,
    })
}

/// The host's keys, straight from it. Only public keys cross; nothing is
/// trusted yet.
pub async fn scan(target: &Target) -> Result<(Resolved, Vec<HostKey>), String> {
    let r = resolve(target).await?;
    let mut c = tokio::process::Command::new(sibling(target, "ssh-keyscan"));
    c.arg("-T").arg("10").arg("-p").arg(r.port.to_string());
    c.arg("--").arg(&r.hostname);
    let out = output(c, None, "ssh-keyscan").await?;
    let mut keys = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(_), Some(kind), Some(blob)) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        let entry = format!("{} {kind} {blob}", r.known_as);
        keys.push(HostKey {
            fingerprint: fingerprint(target, &entry).await?,
            key_type: kind.to_string(),
            line: entry,
        });
    }
    if keys.is_empty() {
        return Err(format!(
            "no host keys from {}:{}{}",
            r.hostname,
            r.port,
            match String::from_utf8_lossy(&out.stderr).trim() {
                "" => String::new(),
                e => format!(": {e}"),
            }
        ));
    }
    Ok((r, keys))
}

async fn fingerprint(target: &Target, line: &str) -> Result<String, String> {
    let mut c = tokio::process::Command::new(sibling(target, "ssh-keygen"));
    c.arg("-l").arg("-f").arg("-");
    let out = output(c, Some(format!("{line}\n").as_bytes()), "ssh-keygen -l").await?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find(|w| w.starts_with("SHA256:"))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "ssh-keygen couldn't fingerprint the key: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })
}

/// Keep only the keys whose fingerprint is `expected` (`SHA256:…`), for a
/// scripted trust that was told the fingerprint out of band.
pub fn matching(keys: &[HostKey], expected: &str) -> Vec<HostKey> {
    let want = expected.trim();
    keys.iter()
        .filter(|k| k.fingerprint == want)
        .cloned()
        .collect()
}

/// Append `keys` to ferrule's known_hosts (made 0600, its dir 0700).
pub fn add(known_hosts: &Path, keys: &[HostKey]) -> std::io::Result<()> {
    if let Some(dir) = known_hosts.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let existing = std::fs::read_to_string(known_hosts).unwrap_or_default();
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(known_hosts)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    for k in keys {
        if !existing.lines().any(|l| l.trim() == k.line) {
            writeln!(f, "{}", k.line)?;
        }
    }
    Ok(())
}

/// Whether any of `files` already has a key for `known_as`
/// (`ssh-keygen -F`, which also finds hashed entries).
pub async fn is_known(target: &Target, known_as: &str, files: &[PathBuf]) -> bool {
    for f in files.iter().filter(|f| f.is_file()) {
        let mut c = tokio::process::Command::new(sibling(target, "ssh-keygen"));
        c.arg("-F").arg(known_as).arg("-f").arg(f);
        if let Ok(out) = output(c, None, "ssh-keygen -F").await {
            if out.status.success() && !out.stdout.is_empty() {
                return true;
            }
        }
    }
    false
}

/// The keys `files` already hold for `known_as`, as `(file, key type,
/// base64 blob)`: what a scan is compared with.
pub async fn known_keys(
    target: &Target,
    known_as: &str,
    files: &[PathBuf],
) -> Vec<(PathBuf, String, String)> {
    let mut out = Vec::new();
    for f in files.iter().filter(|f| f.is_file()) {
        let mut c = tokio::process::Command::new(sibling(target, "ssh-keygen"));
        c.arg("-F").arg(known_as).arg("-f").arg(f);
        let Ok(o) = output(c, None, "ssh-keygen -F").await else {
            continue;
        };
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            // A `@cert-authority`/`@revoked` marker comes first.
            let first = words.next().unwrap_or_default();
            let names = if first.starts_with('@') {
                words.next()
            } else {
                Some(first)
            };
            if let (Some(_), Some(kind), Some(blob)) = (names, words.next(), words.next()) {
                out.push((f.clone(), kind.to_string(), blob.to_string()));
            }
        }
    }
    out
}

/// Whether a scanned key is one of the known ones.
pub fn same_key(scanned: &HostKey, kind: &str, blob: &str) -> bool {
    let mut w = scanned.line.split_whitespace().skip(1);
    w.next() == Some(kind) && w.next() == Some(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_appends_once_and_keeps_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("ssh").join("known_hosts");
        let k = HostKey {
            line: "[h]:2222 ssh-ed25519 AAAA".into(),
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:x".into(),
        };
        add(&kh, std::slice::from_ref(&k)).unwrap();
        add(&kh, std::slice::from_ref(&k)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&kh).unwrap(),
            "[h]:2222 ssh-ed25519 AAAA\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&kh).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(matching(std::slice::from_ref(&k), " SHA256:x "), vec![k]);
        assert!(matching(&[], "SHA256:x").is_empty());
    }
}
