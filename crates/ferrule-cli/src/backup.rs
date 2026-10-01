//! M41: `ferrule backup` and `ferrule restore` (docs/backup.md): one
//! `.tar.gz` of an instance's data dir and config, with a manifest of every
//! file's sha256; a restore checks all of it before anything moves, and
//! moves the data that was there aside instead of deleting it.

use crate::config;
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The archive layout this ferrule writes and reads.
pub const FORMAT: u32 = 1;

/// Left out unless `--include-secrets`: keys, sign-ins and credentials.
pub const SECRETS: &[&str] = &[
    "private",
    "claude-code",
    "ssh",
    "proxy/keys",
    "gateway/matrix/session.json",
];

/// Never backed up: downloads, build and run output, and what says a
/// gateway is running right now.
pub const CACHES: &[&str] = &[
    "models",
    "update",
    "bin",
    "worktrees",
    "eval",
    "telemetry",
    "sandbox",
    "gateway/running.json",
    "gateway/status.txt",
    // One lock file per running process, held open for as long as it runs;
    // on Windows a held lock can't be read, and a restored one means nothing.
    "agents.owners",
    "backup.json",
    // The page's own backups (M47): a backup never holds the last one.
    "backups",
];

/// `<data>/backup.json`: the newest backup, for `ferrule doctor`.
const RECORD: &str = "backup.json";
const MANIFEST: &str = "manifest.json";
/// The config file's place in the archive.
const CONFIG: &str = "config/config.toml";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub ferrule_version: String,
    pub instance: String,
    /// UTC, RFC 3339.
    pub created: String,
    pub secrets: bool,
    /// Where the config file was read from.
    #[serde(default)]
    pub config_from: Option<String>,
    pub files: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// `data/<path>` or `config/config.toml`, with `/`.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    file: String,
    /// Unix seconds.
    created: u64,
    size: u64,
    secrets: bool,
}

/// Whether `rel` is `prefix` or inside it.
fn under(rel: &str, prefix: &str) -> bool {
    rel == prefix
        || rel
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn is_secret(rel: &str) -> bool {
    SECRETS.iter().any(|p| under(rel, p))
}

fn is_cache(rel: &str) -> bool {
    CACHES.iter().any(|p| under(rel, p))
        || ["-wal", "-shm", "-journal"]
            .iter()
            .any(|s| rel.ends_with(s))
}

/// Every regular file under `dir`, by its `/`-separated path; symlinks
/// are left alone.
fn walk(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                go(root, &path, out)?;
            } else if kind.is_file() {
                let rel = path
                    .strip_prefix(root)?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn is_sqlite(path: &Path) -> bool {
    let mut head = [0u8; 16];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == b"SQLite format 3\0"
}

/// A consistent copy of a database that may be open and mid-write.
fn vacuum_into(src: &Path, dst: &Path) -> Result<()> {
    let conn = rusqlite::Connection::open_with_flags(
        src,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute("VACUUM INTO ?1", [dst.to_string_lossy()])?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copies `from` into `to`, returning the size and sha256 of what passed.
fn hashed_copy(mut from: impl Read, mut to: impl Write) -> std::io::Result<(u64, String)> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        size += n as u64;
    }
    Ok((size, hex(ctx.finish().as_ref())))
}

fn sha256_of(path: &Path) -> Result<(u64, String)> {
    Ok(hashed_copy(
        BufReader::new(File::open(path)?),
        std::io::sink(),
    )?)
}

fn instance_label() -> String {
    crate::instance::label(crate::instance::current().as_deref()).to_string()
}

fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub(crate) fn stamp_now() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

/// A file only its owner reads.
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// `ferrule backup`.
pub fn backup(out: Option<PathBuf>, include_secrets: bool) -> Result<()> {
    let data = config::data_dir_path().ok_or_else(|| anyhow!("no data dir"))?;
    if !data.is_dir() {
        bail!(
            "there's nothing to back up: {} doesn't exist",
            data.display()
        );
    }
    let instance = instance_label();
    let out = match out {
        Some(out) => out,
        None => PathBuf::from(format!(
            "{}-backup-{}.tar.gz",
            crate::instance::dir_name(crate::instance::current().as_deref()),
            stamp_now()
        )),
    };
    if out.exists() {
        bail!(
            "{} already exists; pick another name with --out",
            out.display()
        );
    }
    let out_dir = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&out_dir)?;
    let out = dunce::canonicalize(&out_dir)?.join(
        out.file_name()
            .ok_or_else(|| anyhow!("--out needs a file name"))?,
    );
    let partial = out.with_extension("gz.partial");

    // What goes in: the data dir's files and the config file.
    let staging = tempfile::tempdir_in(&out_dir).context("a temp dir beside the backup")?;
    let mut left_out = Vec::new();
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    let config_from = config::config_path()?.filter(|p| p.is_file());
    // A config inside the data dir (a container's /data) is stored once,
    // under `config`, not again as `data/<rel>` (M44).
    let config_canon = config_from
        .as_ref()
        .and_then(|p| dunce::canonicalize(p).ok());
    for (rel, path) in walk(&data)? {
        if path == out || path == partial || is_cache(&rel) {
            continue;
        }
        if config_canon.is_some() && dunce::canonicalize(&path).ok() == config_canon {
            continue;
        }
        if is_secret(&rel) && !include_secrets {
            let top = SECRETS.iter().find(|p| under(&rel, p)).unwrap();
            if !left_out.contains(top) {
                left_out.push(*top);
            }
            continue;
        }
        let path = if is_sqlite(&path) {
            let copy = staging.path().join(format!("{}.db", sources.len()));
            vacuum_into(&path, &copy).with_context(|| format!("copying the database {rel}"))?;
            copy
        } else {
            path
        };
        sources.push((format!("data/{rel}"), path));
    }
    if let Some(p) = &config_from {
        sources.push((CONFIG.to_string(), p.clone()));
    }

    let mut files = Vec::with_capacity(sources.len());
    for (name, path) in &sources {
        let (size, sha256) = sha256_of(path).with_context(|| format!("reading {name}"))?;
        files.push(Entry {
            path: name.clone(),
            size,
            sha256,
        });
    }
    let manifest = Manifest {
        format: FORMAT,
        ferrule_version: env!("CARGO_PKG_VERSION").to_string(),
        instance: instance.clone(),
        created: utc_now(),
        secrets: include_secrets,
        config_from: config_from.as_ref().map(|p| p.display().to_string()),
        files,
    };

    let _ = std::fs::remove_file(&partial);
    let written = (|| -> Result<u64> {
        let file =
            create_private(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let mut tar = tar::Builder::new(GzEncoder::new(
            BufWriter::new(file),
            flate2::Compression::default(),
        ));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let header = |size: u64| {
            let mut h = tar::Header::new_gnu();
            h.set_size(size);
            h.set_mode(0o600);
            h.set_mtime(now);
            h.set_entry_type(tar::EntryType::Regular);
            h
        };
        let json = serde_json::to_vec_pretty(&manifest)?;
        tar.append_data(&mut header(json.len() as u64), MANIFEST, json.as_slice())?;
        for ((name, path), entry) in sources.iter().zip(&manifest.files) {
            let file = File::open(path).with_context(|| format!("reading {name}"))?;
            // Exactly the bytes that were hashed, even if the file grew since.
            tar.append_data(&mut header(entry.size), name, file.take(entry.size))?;
        }
        let mut w = tar.into_inner()?.finish()?;
        w.flush()?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    })();
    let size = match written {
        Ok(size) => size,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    std::fs::rename(&partial, &out)?;

    let record = Record {
        file: out.display().to_string(),
        created: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        size,
        secrets: include_secrets,
    };
    let _ = std::fs::write(
        data.join(RECORD),
        serde_json::to_vec_pretty(&record).unwrap_or_default(),
    );

    println!(
        "Backed up {} of the `{instance}` instance ({}) to {}",
        count(manifest.files.len()),
        megabytes(size),
        out.display()
    );
    if include_secrets {
        eprintln!(
            "warning: this backup holds your secrets (keys, sign-ins, credentials). \
             It's readable by you only; keep it somewhere as safe as this machine."
        );
    } else if !left_out.is_empty() {
        println!(
            "Secrets left out: {} (--include-secrets adds them).",
            left_out.join(", ")
        );
    }
    println!(
        "Put it back with `ferrule {}restore {}`.",
        crate::instance::flag(crate::instance::current().as_deref()),
        out.display()
    );
    Ok(())
}

fn count(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

fn megabytes(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// A path from the archive, made safe: `data/…` or the config, nothing
/// that climbs out.
fn checked_name(name: &str) -> Option<&str> {
    if name == CONFIG {
        return Some(name);
    }
    let rel = name.strip_prefix("data/")?;
    let ok = !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !rel.contains('\\');
    ok.then_some(name)
}

/// What a newer ferrule may have changed: the major version, or the minor
/// while it's 0.x.
fn release_line(v: &semver::Version) -> (u64, u64) {
    if v.major == 0 {
        (0, v.minor)
    } else {
        (v.major, 0)
    }
}

/// Reads the archive: the manifest, then every file checked against it.
/// With `into`, the files are written there (`data/…`, `config/…`).
fn read_archive(file: &Path, into: Option<&Path>) -> Result<Manifest> {
    let damaged = |e: std::io::Error| anyhow!("the archive is damaged ({e}); nothing was changed");
    let gz = GzDecoder::new(BufReader::new(
        File::open(file).with_context(|| format!("opening {}", file.display()))?,
    ));
    let mut tar = tar::Archive::new(gz);
    let mut entries = tar.entries().map_err(damaged)?;
    let mut first = entries
        .next()
        .ok_or_else(|| anyhow!("{} is empty; nothing was changed", file.display()))?
        .map_err(damaged)?;
    if first.path_bytes().as_ref() != MANIFEST.as_bytes() {
        bail!(
            "{} isn't a ferrule backup (no {MANIFEST} at its start); nothing was changed",
            file.display()
        );
    }
    let mut json = Vec::new();
    first.read_to_end(&mut json).map_err(damaged)?;
    let manifest: Manifest = serde_json::from_slice(&json)
        .map_err(|e| anyhow!("its {MANIFEST} can't be read ({e}); nothing was changed"))?;
    if manifest.format > FORMAT {
        bail!(
            "this backup was made by ferrule {} in a newer format than this ferrule ({}) reads. \
             Update Ferrule first (/update in a chat), then restore it.",
            manifest.ferrule_version,
            env!("CARGO_PKG_VERSION")
        );
    }
    let mine = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    if let Ok(theirs) = semver::Version::parse(&manifest.ferrule_version) {
        if release_line(&theirs) > release_line(&mine) {
            bail!(
                "this backup was made by ferrule {theirs}, newer than this ferrule ({mine}), \
                 and it may not read that data. Update Ferrule first (/update in a chat), then restore it."
            );
        }
    }

    let mut expected: BTreeMap<&str, &Entry> = manifest
        .files
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    for entry in entries {
        let mut entry = entry.map_err(damaged)?;
        // As written, `/` and all: never the platform's reading of it.
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        let Some(name) = checked_name(&name) else {
            bail!("the archive holds `{name}`, which a backup never does; nothing was changed");
        };
        let Some(want) = expected.remove(name) else {
            bail!("the archive holds `{name}`, which its manifest doesn't list (tampered?); nothing was changed");
        };
        if !kind.is_file() {
            bail!("`{name}` in the archive isn't a plain file; nothing was changed");
        }
        let (size, sha256) = match into {
            Some(dir) => {
                let path = dir.join(name);
                std::fs::create_dir_all(path.parent().unwrap())?;
                let out = create_private(&path)?;
                let mut w = BufWriter::new(out);
                let got = hashed_copy(&mut entry, &mut w).map_err(damaged)?;
                w.flush()?;
                got
            }
            None => hashed_copy(&mut entry, std::io::sink()).map_err(damaged)?,
        };
        if size != want.size || sha256 != want.sha256 {
            bail!("`{name}` doesn't match its checksum in the manifest (changed or corrupted); nothing was changed");
        }
    }
    if let Some(missing) = expected.keys().next() {
        bail!(
            "`{missing}` is in the manifest but not in the archive ({} missing in all); nothing was changed",
            expected.len()
        );
    }
    Ok(manifest)
}

/// What stops a restore now: this instance's gateway or its service.
fn running() -> Option<String> {
    let svc = crate::service::Svc::current();
    if let crate::service::Status::Installed { running: true, .. } = svc.status() {
        return Some(format!(
            "the `{}` service is running. Stop it first: `{}`",
            svc.short(),
            svc.stop_hint()
        ));
    }
    crate::health::marker_pid().map(|pid| {
        format!(
            "a gateway is running on this data (pid {pid}). Stop it first \
             (Ctrl-C where it runs, or `kill {pid}`)"
        )
    })
}

/// Copies `from` (a file or a directory) to `to`.
fn copy_all(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else if from.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// `ferrule restore`.
pub fn restore(file: &Path, dry_run: bool) -> Result<()> {
    let data = config::data_dir_path().ok_or_else(|| anyhow!("no data dir"))?;
    let blocker = running();
    if let (Some(why), false) = (&blocker, dry_run) {
        bail!("not restoring: {why}. Nothing was changed.");
    }
    let parent = data
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", data.display()))?;
    let name = data
        .file_name()
        .ok_or_else(|| anyhow!("{} has no name", data.display()))?
        .to_string_lossy()
        .into_owned();
    let stamp = stamp_now();
    let aside = parent.join(format!("{name}.pre-restore-{stamp}"));
    let config_to = match config::config_path()? {
        Some(p) => p,
        None => config::global_config_path()?,
    };

    if dry_run {
        let m = read_archive(file, None)?;
        let has_config = m.files.iter().any(|e| e.path == CONFIG);
        println!(
            "{} checks out: {} made by ferrule {} for the `{}` instance at {}{}.",
            file.display(),
            count(m.files.len()),
            m.ferrule_version,
            m.instance,
            m.created,
            if m.secrets { ", secrets included" } else { "" }
        );
        println!("A restore would:");
        if data.exists() {
            println!("  move {} to {}", data.display(), aside.display());
        }
        println!("  put the backup's data in {}", data.display());
        if !m.secrets && data.exists() {
            println!("  keep this machine's secrets (the backup has none)");
        }
        if has_config {
            if config_to.exists() {
                println!(
                    "  move {} to {}.pre-restore-{stamp} and put the backup's config there",
                    config_to.display(),
                    config_to.display()
                );
            } else {
                println!("  put the backup's config at {}", config_to.display());
            }
        }
        println!("Nothing was changed.");
        if let Some(why) = blocker {
            println!("It would refuse now: {why}.");
        }
        return Ok(());
    }

    // An empty data dir (a container's /data, a mount point whose parent
    // can't be written) is filled in place: nothing to move aside (M44).
    let in_place = data.is_dir() && std::fs::read_dir(&data)?.next().is_none();
    let staging = if in_place {
        tempfile::Builder::new()
            .prefix(".restore-")
            .tempdir_in(&data)
            .with_context(|| format!("a temp dir in {}", data.display()))?
    } else {
        std::fs::create_dir_all(parent)?;
        tempfile::Builder::new()
            .prefix(&format!(".{name}.restore-"))
            .tempdir_in(parent)?
    };
    let manifest = read_archive(file, Some(staging.path()))?;

    // Everything checked out: now the swap.
    let had_data = data.exists() && !in_place;
    if had_data {
        std::fs::rename(&data, &aside).with_context(|| {
            format!(
                "moving {} aside (is something still using it?); nothing was changed",
                data.display()
            )
        })?;
    }
    let restored = staging.path().join("data");
    let moved = if in_place {
        move_entries(&restored, &data)
    } else if restored.is_dir() {
        std::fs::rename(&restored, &data)
    } else {
        std::fs::create_dir_all(&data)
    };
    if let Err(e) = moved {
        if had_data {
            let _ = std::fs::rename(&aside, &data);
        }
        return Err(anyhow!(e).context("putting the backup's data in place; nothing was changed"));
    }
    let mut kept = Vec::new();
    if !manifest.secrets && had_data {
        for s in SECRETS {
            let from = aside.join(s);
            if from.exists() {
                copy_all(&from, &data.join(s))
                    .with_context(|| format!("keeping this machine's {s}"))?;
                kept.push(*s);
            }
        }
    }
    let staged_config = staging.path().join(CONFIG);
    let mut config_aside = None;
    if staged_config.is_file() {
        if config_to.exists() {
            let to = PathBuf::from(format!("{}.pre-restore-{stamp}", config_to.display()));
            std::fs::rename(&config_to, &to)?;
            config_aside = Some(to);
        }
        if let Some(dir) = config_to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        copy_all(&staged_config, &config_to)?;
    }
    drop(staging);

    println!(
        "Restored {} from {} (made by ferrule {} at {}).",
        count(manifest.files.len()),
        file.display(),
        manifest.ferrule_version,
        manifest.created
    );
    if had_data {
        println!(
            "The data that was here is in {}; nothing was deleted.",
            aside.display()
        );
    }
    if let Some(to) = config_aside {
        println!("The config that was here is {}.", to.display());
    }
    if !kept.is_empty() {
        println!(
            "The backup had no secrets, so this machine's were kept: {}.",
            kept.join(", ")
        );
    } else if !manifest.secrets {
        println!("The backup had no secrets: set your keys again with `ferrule setup`.");
    }
    if had_data && aside.join("models").is_dir() {
        println!(
            "Downloaded models stay in {}; move `models` back to skip downloading them again.",
            aside.display()
        );
    }
    Ok(())
}

/// Moves what is in `from` (if it exists) into the existing `to`.
fn move_entries(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        std::fs::rename(entry.path(), to.join(entry.file_name()))?;
    }
    Ok(())
}

/// The doctor's line: when the newest backup was made.
pub fn doctor_line() -> String {
    let record: Option<Record> = config::data_dir_path()
        .and_then(|d| std::fs::read(d.join(RECORD)).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());
    match record {
        None => "none made yet: `ferrule backup` (docs/backup.md)".into(),
        Some(r) => {
            let at = SystemTime::UNIX_EPOCH + Duration::from_secs(r.created);
            let age = SystemTime::now().duration_since(at).unwrap_or_default();
            let days = age.as_secs() / 86_400;
            let ago = match days {
                0 => format!("{} ago", ferrule_gateway::health::human(age)),
                1 => "1 day ago".into(),
                n => format!("{n} days ago"),
            };
            format!("the last one was {ago}: {}", r.file)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_and_caches_are_matched_by_whole_path_parts() {
        assert!(is_secret("private/secrets.env"));
        assert!(is_secret("proxy/keys/ca.key"));
        assert!(is_secret("gateway/matrix/session.json"));
        assert!(!is_secret("proxy/state.json"));
        assert!(!is_secret("private-notes.md"));
        assert!(!is_secret("gateway/matrix/state.json"));
        assert!(is_cache("models/qwen.gguf"));
        assert!(is_cache("memory.db-wal"));
        assert!(is_cache("gateway/running.json"));
        assert!(is_cache("agents.owners/7f4f.lock"));
        assert!(!is_cache("modelsfile"));
        assert!(!is_cache("tasks.db"));
        assert!(!is_cache("sessions/telegram_1.jsonl"));
    }

    #[test]
    fn only_data_and_the_config_come_out_of_an_archive() {
        assert!(checked_name("data/sessions/a.jsonl").is_some());
        assert!(checked_name("config/config.toml").is_some());
        for bad in [
            "data/../x",
            "data/",
            "/etc/passwd",
            "config/other",
            "data//abs",
            "x/y",
        ] {
            assert!(checked_name(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_newer_release_line_is_newer() {
        let v = |s| semver::Version::parse(s).unwrap();
        assert!(release_line(&v("0.11.0")) > release_line(&v("0.10.3")));
        assert!(release_line(&v("0.10.9")) <= release_line(&v("0.10.0")));
        assert!(release_line(&v("1.0.0")) > release_line(&v("0.99.0")));
        assert!(release_line(&v("1.4.0")) <= release_line(&v("1.0.0")));
    }
}
