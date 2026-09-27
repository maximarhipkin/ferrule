//! M36's hermetic tests (docs/m36-self-update.md §9): a mock release
//! server, a test signing key, a fake service and a fake owner.

use super::apply::{Apply, Outcome, Service, Want};
use super::notice::{Owner, Watch};
use super::release::{self, Release, Source};
use super::state::{self, EventKind, Lock, Request, State, Told};
use super::{swap, Channel};
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use blake2::Digest;
use ferrule_gateway::health::{MarkedTurn, RunningMarker, RUNNING_FILE};
use ring::signature::KeyPair;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LINUX: &str = "x86_64-unknown-linux-musl";
const WINDOWS: &str = "x86_64-pc-windows-msvc";

fn v(text: &str) -> semver::Version {
    release::parse_version(text).unwrap()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A minisign key made in the test: prehashed Ed25519, as `minisign -S -H`.
struct Key {
    pair: ring::signature::Ed25519KeyPair,
    id: [u8; 8],
}

impl Key {
    fn new(seed: u8) -> Self {
        Self {
            pair: ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap(),
            id: [seed; 8],
        }
    }

    fn public(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.id);
        raw.extend_from_slice(self.pair.public_key().as_ref());
        b64(&raw)
    }

    fn sign(&self, data: &[u8], comment: &str) -> String {
        let hash = blake2::Blake2b512::digest(data);
        let sig = self.pair.sign(&hash);
        let mut line = b"ED".to_vec();
        line.extend_from_slice(&self.id);
        line.extend_from_slice(sig.as_ref());
        let mut global = sig.as_ref().to_vec();
        global.extend_from_slice(comment.as_bytes());
        let global = self.pair.sign(&global);
        format!(
            "untrusted comment: test\n{}\ntrusted comment: {comment}\n{}\n",
            b64(&line),
            b64(global.as_ref())
        )
    }
}

fn sha256_line(data: &[u8], name: &str) -> String {
    let hash = release::hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref());
    format!("{hash}  {name}\n")
}

fn tar_gz(name: &str, body: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, name, body).unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

fn zip_of(name: &str, body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(body).unwrap();
    zip.finish().unwrap().into_inner()
}

/// A stand-in `ferrule` that answers `--version` like the real one.
fn script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho ferrule {version}\n").into_bytes()
}

/// A GitHub-shaped release server on localhost.
struct Mock {
    base: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    releases: Vec<serde_json::Value>,
}

impl Mock {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let served = routes.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = served.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let mut n = 0;
                    while n < buf.len() {
                        let m = sock.read(&mut buf[n..]).await.unwrap_or(0);
                        if m == 0 {
                            break;
                        }
                        n += m;
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/");
                    let path = path.split('?').next().unwrap_or(path).to_string();
                    let found = routes.lock().unwrap().get(&path).cloned();
                    let (status, body) = match found {
                        Some(body) => ("200 OK", body),
                        None => ("404 Not Found", b"{}".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self {
            base,
            routes,
            releases: Vec::new(),
        }
    }

    fn put(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.into(), body);
    }

    /// A release with these files as assets; the list and the tag's own
    /// URL are rewritten.
    fn release(&mut self, tag: &str, files: &[(&str, Vec<u8>)], draft: bool, prerelease: bool) {
        let assets: Vec<serde_json::Value> = files
            .iter()
            .map(|(name, body)| {
                let path = format!("/dl/{tag}/{name}");
                self.put(&path, body.clone());
                serde_json::json!({
                    "name": name,
                    "size": body.len(),
                    "browser_download_url": format!("{}{path}", self.base),
                })
            })
            .collect();
        let release = serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "body": format!("## Faster\n\n- {tag} is faster"),
            "assets": assets,
        });
        self.put(
            &format!("/repos/{}/releases/tags/{tag}", release::REPO),
            serde_json::to_vec(&release).unwrap(),
        );
        self.releases.push(release);
        self.put(
            &format!("/repos/{}/releases", release::REPO),
            serde_json::to_vec(&self.releases).unwrap(),
        );
    }

    /// A signed release for `target` whose binary is `body`.
    fn signed(&mut self, tag: &str, target: &str, key: &Key, body: &[u8]) {
        let name = release::archive_name(target);
        let archive = if name.ends_with(".zip") {
            zip_of(release::binary_name(target), body)
        } else {
            tar_gz(release::binary_name(target), body)
        };
        let sig = key.sign(&archive, &format!("ferrule {tag} {name}"));
        self.release(
            tag,
            &[
                (
                    &format!("{name}.sha256"),
                    sha256_line(&archive, &name).into_bytes(),
                ),
                (&format!("{name}.minisig"), sig.into_bytes()),
                (&name, archive),
            ],
            false,
            false,
        );
    }

    fn source(&self, key: &Key) -> Source {
        Source::new(&self.base).with_keys(&key.public())
    }
}

fn found(r: Option<&Release>) -> Option<String> {
    r.map(|r| r.tag.clone())
}

async fn listed(mock: &Mock, key: &Key) -> Vec<Release> {
    mock.source(key).list().await.unwrap()
}

#[tokio::test]
async fn choose_takes_the_newest_allowed_release_and_never_downgrades() {
    let key = Key::new(1);
    let mut mock = Mock::start().await;
    for tag in ["v0.5.0", "v0.5.2", "v0.6.0"] {
        mock.signed(tag, LINUX, &key, b"x");
    }
    mock.signed("v0.7.0-rc.1", LINUX, &key, b"x");
    mock.release("v0.9.0", &[], true, false); // a draft
    mock.signed("v0.8.0", WINDOWS, &key, b"x"); // no Linux build
    mock.release("nightly", &[], false, false); // not a version
    let all = listed(&mock, &key).await;
    assert!(all.iter().all(|r| r.tag != "v0.9.0" && r.tag != "nightly"));
    let current = v("0.5.1");
    let pick =
        |channel, pinned: &[String]| found(release::choose(&all, &current, channel, pinned, LINUX));
    assert_eq!(pick(Channel::Stable, &[]).as_deref(), Some("v0.6.0"));
    assert_eq!(
        pick(Channel::Prerelease, &[]).as_deref(),
        Some("v0.7.0-rc.1")
    );
    assert_eq!(
        pick(Channel::Stable, &["0.6.0".into()]).as_deref(),
        Some("v0.5.2")
    );
    let newest = v("0.6.0");
    assert_eq!(
        found(release::choose(&all, &newest, Channel::Stable, &[], LINUX)),
        None
    );
    let later = v("1.0.0");
    assert_eq!(
        found(release::choose(
            &all,
            &later,
            Channel::Prerelease,
            &[],
            LINUX
        )),
        None,
        "never a downgrade"
    );
}

#[tokio::test]
async fn fetch_accepts_a_signed_release_and_refuses_a_tampered_one() {
    let key = Key::new(2);
    let mut mock = Mock::start().await;
    mock.signed("v0.6.0", LINUX, &key, b"new binary");
    let r = mock.source(&key).tag("0.6.0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let checked = release::fetch(&mock.source(&key), &r, LINUX, false, Some(dir.path()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&checked.binary).unwrap(), b"new binary");
    assert!(checked.signed_by.is_some());
    assert!(
        checked.binary.starts_with(dir.path()),
        "unpacked beside the binary it replaces"
    );

    // Another key signed it.
    let other = Key::new(3);
    let e = release::fetch(&mock.source(&other), &r, LINUX, false, None)
        .await
        .err()
        .unwrap();
    assert!(format!("{e:#}").contains("doesn't verify"), "{e:#}");
    // --unsigned doesn't excuse a bad signature.
    assert!(release::fetch(&mock.source(&other), &r, LINUX, true, None)
        .await
        .is_err());

    // The archive changed after the checksum was written.
    let name = release::archive_name(LINUX);
    mock.put(&format!("/dl/v0.6.0/{name}"), tar_gz("ferrule", b"evil"));
    let e = release::fetch(&mock.source(&key), &r, LINUX, false, None)
        .await
        .err()
        .unwrap();
    assert!(format!("{e:#}").contains("sha256"), "{e:#}");
}

#[tokio::test]
async fn fetch_checks_the_trusted_comment_names_this_tag_and_asset() {
    let key = Key::new(4);
    let mut mock = Mock::start().await;
    let name = release::archive_name(LINUX);
    let archive = tar_gz("ferrule", b"old but signed");
    // A genuine signature from v0.5.0, replayed as v0.6.0.
    let replayed = key.sign(&archive, &format!("ferrule v0.5.0 {name}"));
    mock.release(
        "v0.6.0",
        &[
            (
                &format!("{name}.sha256"),
                sha256_line(&archive, &name).into_bytes(),
            ),
            (&format!("{name}.minisig"), replayed.into_bytes()),
            (&name, archive),
        ],
        false,
        false,
    );
    let r = mock.source(&key).tag("v0.6.0").await.unwrap();
    let e = release::fetch(&mock.source(&key), &r, LINUX, false, None)
        .await
        .err()
        .unwrap();
    assert!(format!("{e:#}").contains("not \"ferrule v0.6.0"), "{e:#}");
}

#[tokio::test]
async fn an_unsigned_release_needs_unsigned_ok() {
    let key = Key::new(5);
    let mut mock = Mock::start().await;
    let name = release::archive_name(LINUX);
    let archive = tar_gz("ferrule", b"from before signing");
    mock.release(
        "v0.5.0",
        &[
            (
                &format!("{name}.sha256"),
                sha256_line(&archive, &name).into_bytes(),
            ),
            (&name, archive),
        ],
        false,
        false,
    );
    let source = mock.source(&key);
    let r = source.tag("v0.5.0").await.unwrap();
    let e = release::fetch(&source, &r, LINUX, false, None)
        .await
        .err()
        .unwrap();
    assert!(format!("{e:#}").contains("isn't signed"), "{e:#}");
    let checked = release::fetch(&source, &r, LINUX, true, None)
        .await
        .unwrap();
    assert_eq!(checked.signed_by, None);
    assert_eq!(
        std::fs::read(&checked.binary).unwrap(),
        b"from before signing"
    );
}

#[tokio::test]
async fn a_windows_release_is_a_zip_with_ferrule_exe() {
    let key = Key::new(6);
    let mut mock = Mock::start().await;
    mock.signed("v0.6.0", WINDOWS, &key, b"MZ exe");
    let source = mock.source(&key);
    let r = source.tag("v0.6.0").await.unwrap();
    let checked = release::fetch(&source, &r, WINDOWS, false, None)
        .await
        .unwrap();
    assert!(checked.binary.ends_with("ferrule.exe"));
    assert_eq!(std::fs::read(&checked.binary).unwrap(), b"MZ exe");
}

#[test]
fn extract_finds_the_binary_at_the_top_level_only() {
    let dir = tempfile::tempdir().unwrap();
    let nested = tar_gz("sub/ferrule", b"x");
    assert!(release::extract(&nested, "a.tar.gz", "ferrule", dir.path()).is_err());
    let dotted = tar_gz("./ferrule", b"y");
    let out = release::extract(&dotted, "a.tar.gz", "ferrule", dir.path()).unwrap();
    assert_eq!(std::fs::read(out).unwrap(), b"y");
    assert!(release::extract(b"not an archive", "a.zip", "ferrule.exe", dir.path()).is_err());
}

#[test]
fn a_sha256_file_must_hold_a_digest_for_this_archive() {
    let digest = "a".repeat(64);
    assert_eq!(
        release::parse_sha256(&format!("{digest}  x.tar.gz\n"), "x.tar.gz").unwrap(),
        digest
    );
    assert_eq!(
        release::parse_sha256(&format!("\u{feff}{}", digest.to_uppercase()), "x.zip").unwrap(),
        digest
    );
    assert!(release::parse_sha256(&format!("{digest} *y.tar.gz"), "x.tar.gz").is_err());
    assert!(release::parse_sha256("nothing", "x.tar.gz").is_err());
}

#[test]
fn a_glibc_build_updates_to_the_musl_release() {
    assert!(
        !release::TARGET.contains("-linux-gnu"),
        "{}",
        release::TARGET
    );
    assert!(!release::TARGET.is_empty());
}

#[test]
fn the_compiled_in_key_parses() {
    let keys: Vec<&str> = release::KEYS
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(keys.len(), 1);
    assert!(minisign_verify::PublicKey::from_base64(keys[0]).is_ok());
}

#[test]
fn swap_keeps_the_previous_binary_and_rolls_back_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(if cfg!(windows) {
        "ferrule.exe"
    } else {
        "ferrule"
    });
    std::fs::write(&exe, b"old").unwrap();
    let new = dir.path().join("download");
    std::fs::write(&new, b"new").unwrap();
    let previous = swap::swap(&exe, &new).unwrap();
    assert_eq!(previous, swap::previous_path(&exe));
    assert_eq!(std::fs::read(&exe).unwrap(), b"new");
    assert_eq!(std::fs::read(&previous).unwrap(), b"old");
    swap::roll_back(&exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    assert!(previous.exists(), "kept for another rollback");
    swap::clean_old(&exe);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| !n.ends_with(".old") && !n.starts_with(".ferrule")),
        "{names:?}"
    );
}

#[cfg(windows)]
#[test]
fn a_running_exe_steps_aside_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("ferrule.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    // Running, the file can't be overwritten, only renamed.
    let mut child = std::process::Command::new(&exe)
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let new = dir.path().join("download.exe");
    std::fs::write(&new, b"new").unwrap();
    let swapped = swap::swap(&exe, &new);
    let _ = child.wait();
    swapped.unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"new");
    swap::clean_old(&exe);
    let old = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".old")
        })
        .count();
    assert_eq!(old, 0);
}

#[cfg(unix)]
#[test]
fn runs_wants_the_version_it_was_promised() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("ferrule");
    std::fs::write(&bin, script("0.6.0")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A freshly written script can briefly be "text file busy" while
    // another test thread forks.
    let mut ok = release::runs(&bin, &v("0.6.0"));
    for _ in 0..5 {
        if ok.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        ok = release::runs(&bin, &v("0.6.0"));
    }
    ok.unwrap();
    let e = release::runs(&bin, &v("0.7.0")).unwrap_err();
    assert!(e.to_string().contains("not ferrule 0.7.0"), "{e}");
    assert!(release::runs(&dir.path().join("missing"), &v("0.6.0")).is_err());
}

/// The gateway's marker, as a running gateway writes it.
fn mark(data: &Path, version: &str, busy: bool, up_for: u64) {
    let marker = RunningMarker {
        pid: std::process::id(),
        version: version.into(),
        started: state::now() - up_for,
        turns: if busy {
            vec![MarkedTurn {
                place: "tg:1".into(),
                channel: "telegram".into(),
                chat_id: "1".into(),
                text: "working".into(),
            }]
        } else {
            Vec::new()
        },
    };
    let dir = data.join("gateway");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(RUNNING_FILE), serde_json::to_vec(&marker).unwrap()).unwrap();
}

/// Restarting "starts" a gateway reporting `comes_up`, if anything.
struct FakeService {
    data: PathBuf,
    comes_up: Option<String>,
    restarts: Mutex<u32>,
}

impl Service for FakeService {
    fn restart(&self) -> Result<()> {
        let mut n = self.restarts.lock().unwrap();
        *n += 1;
        if *n == 1 {
            if let Some(version) = &self.comes_up {
                mark(&self.data, version, false, 60);
            }
        }
        Ok(())
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    exe: PathBuf,
    data: PathBuf,
    state_dir: PathBuf,
    mock: Mock,
    key: Key,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("ferrule");
    std::fs::write(&exe, b"old").unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("update")).unwrap();
    let key = Key::new(9);
    let mut mock = Mock::start().await;
    mock.signed("v0.6.0", LINUX, &key, &script("0.6.0"));
    Setup {
        exe,
        state_dir: super::state_dir(&data),
        data,
        mock,
        key,
        _dir: dir,
    }
}

fn apply<'a>(s: &Setup, service: Option<&'a dyn Service>) -> Apply<'a> {
    let mut a = Apply::new_defaults(
        s.mock.source(&s.key),
        s.exe.clone(),
        s.data.clone(),
        s.state_dir.clone(),
    );
    a.target = LINUX.into();
    a.current = v("0.5.1");
    a.service = service;
    a.check_runs = false;
    a.idle_for = Duration::from_millis(300);
    a.health_for = Duration::from_millis(800);
    a.healthy_after = Duration::from_secs(30);
    a.poll = Duration::from_millis(20);
    a
}

#[tokio::test]
async fn a_healthy_update_stays_and_is_recorded() {
    let s = setup().await;
    let service = FakeService {
        data: s.data.clone(),
        comes_up: Some("0.6.0".into()),
        restarts: Mutex::new(0),
    };
    let out = apply(&s, Some(&service))
        .run(&Want::default())
        .await
        .unwrap();
    assert_eq!(
        out,
        Outcome::Updated {
            from: "0.5.1".into(),
            to: "0.6.0".into(),
            restart_needed: false
        }
    );
    assert_eq!(std::fs::read(&s.exe).unwrap(), script("0.6.0"));
    assert_eq!(std::fs::read(swap::previous_path(&s.exe)).unwrap(), b"old");
    let state = State::load(&s.state_dir);
    let e = state.events.last().unwrap();
    assert_eq!((e.kind, e.to.as_str()), (EventKind::Updated, "0.6.0"));
    assert_eq!(e.notes, "Faster");
    assert_eq!(state.latest.as_deref(), Some("v0.6.0"));
    assert!(state.signed_seen);
    assert!(
        !s.state_dir.join("apply.lock").exists(),
        "the lock is let go"
    );
}

#[tokio::test]
async fn an_update_that_doesnt_come_up_is_rolled_back_and_pinned() {
    let s = setup().await;
    // The gateway keeps reporting the old version.
    let service = FakeService {
        data: s.data.clone(),
        comes_up: Some("0.5.1".into()),
        restarts: Mutex::new(0),
    };
    let out = apply(&s, Some(&service))
        .run(&Want::default())
        .await
        .unwrap();
    assert_eq!(
        out,
        Outcome::RolledBack {
            from: "0.5.1".into(),
            to: "0.6.0".into()
        }
    );
    assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
    assert_eq!(
        *service.restarts.lock().unwrap(),
        2,
        "the new one, then the old one"
    );
    let state = State::load(&s.state_dir);
    assert!(state.is_pinned("v0.6.0"));
    assert_eq!(state.events.last().unwrap().kind, EventKind::RolledBack);
    // Pinned: the next run leaves it alone…
    assert_eq!(
        apply(&s, Some(&service))
            .run(&Want::default())
            .await
            .unwrap(),
        Outcome::UpToDate
    );
    // …unless it's asked for by name, which unpins it.
    let named = Want {
        to: Some("v0.6.0".into()),
        unsigned_ok: false,
    };
    let fine = FakeService {
        data: s.data.clone(),
        comes_up: Some("0.6.0".into()),
        restarts: Mutex::new(0),
    };
    assert!(matches!(
        apply(&s, Some(&fine)).run(&named).await.unwrap(),
        Outcome::Updated { .. }
    ));
    assert!(!State::load(&s.state_dir).is_pinned("v0.6.0"));
}

#[tokio::test]
async fn a_busy_gateway_gets_nothing_installed() {
    let s = setup().await;
    mark(&s.data, "0.5.1", true, 600);
    let service = FakeService {
        data: s.data.clone(),
        comes_up: Some("0.6.0".into()),
        restarts: Mutex::new(0),
    };
    let out = apply(&s, Some(&service))
        .run(&Want::default())
        .await
        .unwrap();
    assert_eq!(out, Outcome::Busy);
    assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
    assert_eq!(*service.restarts.lock().unwrap(), 0);
}

#[tokio::test]
async fn without_a_service_the_binary_is_swapped_and_a_restart_is_owed() {
    let s = setup().await;
    let out = apply(&s, None).run(&Want::default()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::Updated {
                restart_needed: true,
                ..
            }
        ),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_refused_release_is_recorded_once() {
    let s = setup().await;
    let a = Apply {
        source: s.mock.source(&Key::new(10)),
        ..apply(&s, None)
    };
    assert!(a.run(&Want::default()).await.is_err());
    assert!(a.run(&Want::default()).await.is_err());
    let state = State::load(&s.state_dir);
    let failed: Vec<_> = state
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Failed)
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(std::fs::read(&s.exe).unwrap(), b"old");
}

#[tokio::test]
async fn a_check_records_what_it_saw() {
    let mut s = setup().await;
    s.mock.release("v0.7.0-rc.1", &[], false, true);
    let a = apply(&s, None);
    let r = a.check(&Want::default()).await.unwrap().unwrap();
    assert_eq!(r.tag, "v0.6.0");
    let state = State::load(&s.state_dir);
    assert_eq!(
        state.latest.as_deref(),
        Some("v0.6.0"),
        "stable ignores the rc"
    );
    assert_eq!(state.last_check_ok, Some(true));
    let broken = Apply {
        source: Source::new("http://127.0.0.1:9"),
        ..apply(&s, None)
    };
    assert!(broken.check(&Want::default()).await.is_err());
    let state = State::load(&s.state_dir);
    assert_eq!(state.last_check_ok, Some(false));
    assert!(state.last_error.is_some());
}

#[test]
fn the_lock_is_one_at_a_time_and_a_dead_holder_loses_it() {
    let dir = tempfile::tempdir().unwrap();
    let held = Lock::take(dir.path()).unwrap();
    assert!(Lock::take(dir.path()).is_err());
    drop(held);
    let again = Lock::take(dir.path()).unwrap();
    drop(again);
    std::fs::write(dir.path().join("apply.lock"), "999999999").unwrap();
    Lock::take(dir.path()).unwrap();
}

#[test]
fn a_request_is_read_once_never_through_a_link_and_never_picks_a_version() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    std::fs::create_dir_all(data.join("update")).unwrap();
    assert_eq!(state::take_request(data), None);
    let path = data.join("update").join(state::REQUEST_FILE);
    std::fs::write(&path, r#"{"ferrule":true,"to":"v0.1.0"}"#).unwrap();
    let r = state::take_request(data).unwrap();
    assert_eq!(
        r,
        Request {
            ferrule: true,
            claude: false,
            to: None,
            id: 0
        }
    );
    assert!(!path.exists());
    std::fs::write(&path, vec![b' '; 5000]).unwrap();
    assert_eq!(state::take_request(data), None, "too big");
    assert!(!path.exists());
    #[cfg(unix)]
    {
        let secret = dir.path().join("secret");
        std::fs::write(&secret, r#"{"ferrule":true}"#).unwrap();
        std::os::unix::fs::symlink(&secret, &path).unwrap();
        assert_eq!(state::take_request(data), None);
        assert!(secret.exists(), "the link goes, not its target");
    }
    state::write_request(
        data,
        &Request {
            claude: true,
            to: Some("v9".into()),
            ..Request::default()
        },
    )
    .unwrap();
    assert_eq!(state::take_request(data).unwrap().to, None);
}

#[test]
fn events_are_capped_and_numbered() {
    let mut state = State::default();
    for i in 0..25 {
        state.push(EventKind::Updated, "a", &i.to_string(), "");
    }
    assert_eq!(state.events.len(), 20);
    assert_eq!(state.events.last().unwrap().id, 25);
    let dir = tempfile::tempdir().unwrap();
    state.save(dir.path()).unwrap();
    assert_eq!(State::load(dir.path()), state);
}

#[derive(Default)]
struct FakeOwner {
    told: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
    allow: bool,
}

#[async_trait]
impl Owner for FakeOwner {
    fn tell(&self, text: String) {
        self.told.lock().unwrap().push(text);
    }

    async fn ask(&self, _subject: &str, question: &str) -> bool {
        self.asked.lock().unwrap().push(question.into());
        self.allow
    }
}

fn watch(data: &Path, units: bool, auto: Option<bool>, source: Source) -> Watch {
    Watch {
        data: data.into(),
        auto,
        channel: Channel::Stable,
        units,
        source,
        current: v("0.5.1"),
        target: LINUX.into(),
        jitter: 0,
        claude: None,
    }
}

fn told(owner: &FakeOwner) -> Vec<String> {
    owner.told.lock().unwrap().clone()
}

#[tokio::test]
async fn each_update_is_told_once_and_old_news_isnt_told_on_a_first_start() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    std::fs::create_dir_all(data.join("update")).unwrap();
    let state_dir = super::state_dir(data);
    let mut state = State::default();
    state.push(EventKind::Updated, "0.4.0", "0.5.0", "");
    state.events[0].at -= 3 * 24 * 3600;
    state.push(EventKind::Updated, "0.5.0", "0.5.1", "Faster");
    state.save(&state_dir).unwrap();
    let fake = Arc::new(FakeOwner::default());
    let owner: Arc<dyn Owner> = fake.clone();
    let w = watch(data, true, None, Source::new("http://127.0.0.1:9"));
    w.tick(&owner).await.unwrap();
    assert_eq!(told(&fake), ["Updated Ferrule 0.5.0 → 0.5.1: Faster"]);
    w.tick(&owner).await.unwrap();
    assert_eq!(told(&fake).len(), 1, "told once");

    // Rolled back, then the same refusal twice: two lines, not three.
    let mut state = State::load(&state_dir);
    state.push(EventKind::RolledBack, "0.5.1", "0.6.0", "no");
    state.push(EventKind::Failed, "0.5.1", "0.7.0", "bad signature");
    state.push(EventKind::Failed, "0.5.1", "0.7.0", "bad signature");
    state.save(&state_dir).unwrap();
    w.tick(&owner).await.unwrap();
    let lines = told(&fake);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[1].contains("went back to 0.5.1") && lines[1].contains("--to v0.6.0"));
    assert!(lines[2].starts_with("Ferrule 0.7.0 is out but wasn't installed"));

    // A state that started over (a reinstall): only what's recent.
    let mut fresh = State::default();
    fresh.push(EventKind::Updated, "0.5.1", "0.6.0", "");
    fresh.save(&state_dir).unwrap();
    w.tick(&owner).await.unwrap();
    assert_eq!(
        told(&fake).last().unwrap(),
        "Updated Ferrule 0.5.1 → 0.6.0."
    );
    assert_eq!(Told::load(data).unwrap().id, 1);
}

#[tokio::test]
async fn with_auto_off_the_owner_is_asked_once_and_allow_requests_the_install() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    std::fs::create_dir_all(data.join("update")).unwrap();
    let state = State {
        latest: Some("v0.6.0".into()),
        ..State::default()
    };
    state.save(&super::state_dir(data)).unwrap();
    let fake = Arc::new(FakeOwner {
        allow: true,
        ..FakeOwner::default()
    });
    let owner: Arc<dyn Owner> = fake.clone();
    let w = watch(data, true, Some(false), Source::new("http://127.0.0.1:9"));
    w.tick(&owner).await.unwrap();
    w.tick(&owner).await.unwrap();
    let request = data.join("update").join(state::REQUEST_FILE);
    for _ in 0..100 {
        if request.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fake.asked.lock().unwrap().len(), 1);
    assert!(fake.asked.lock().unwrap()[0].contains("v0.6.0 is out (this is 0.5.1)"));
    assert!(state::take_request(data).unwrap().ferrule);

    // Auto on: the unit installs it, nobody is asked.
    let quiet = Arc::new(FakeOwner::default());
    let owner: Arc<dyn Owner> = quiet.clone();
    let w = watch(data, true, None, Source::new("http://127.0.0.1:9"));
    std::fs::remove_file(data.join("update").join(state::TOLD_FILE)).unwrap();
    w.tick(&owner).await.unwrap();
    assert!(quiet.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_the_units_the_gateway_checks_daily_and_tells_once_per_release() {
    let key = Key::new(11);
    let mut mock = Mock::start().await;
    mock.signed("v0.6.0", LINUX, &key, b"x");
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    std::fs::create_dir_all(data.join("update")).unwrap();
    let fake = Arc::new(FakeOwner::default());
    let owner: Arc<dyn Owner> = fake.clone();
    let w = watch(data, false, None, mock.source(&key));
    w.tick(&owner).await.unwrap();
    assert_eq!(
        told(&fake),
        ["Ferrule v0.6.0 is out: run `ferrule update`."]
    );
    w.tick(&owner).await.unwrap();
    // Next day, still the same release: nothing new.
    let mut t = Told::load(data).unwrap();
    t.checked = Some(state::now() - 2 * 24 * 3600);
    t.save(data).unwrap();
    w.tick(&owner).await.unwrap();
    assert_eq!(told(&fake).len(), 1);
}

#[test]
fn the_report_says_how_updates_happen_and_what_went_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let state_dir = super::state_dir(data);
    let mut state = State {
        last_check: Some(state::now() - 3600),
        last_check_ok: Some(true),
        ..State::default()
    };
    state.push(
        EventKind::RolledBack,
        "0.5.1",
        "0.6.0",
        "it didn't report healthy",
    );
    state.pin("v0.6.0");
    state.pin("v0.7.0");
    state.save(&state_dir).unwrap();
    let lines = super::report(data, None, true);
    assert!(
        lines[0].1.contains("installed by itself when idle"),
        "{lines:?}"
    );
    assert!(
        lines[1].1.starts_with("last checked 1 h 0 min ago"),
        "{lines:?}"
    );
    let rolled = lines
        .iter()
        .find(|(_, l)| l.contains("rolled back"))
        .unwrap();
    assert_eq!(rolled.0, super::Tone::Warn);
    assert!(lines
        .iter()
        .any(|(_, l)| l == "never installed by itself: v0.7.0"));
    let off = super::report(data, Some(false), true);
    assert!(off[0].1.contains("installed when you say so"));
}

#[test]
fn the_system_state_lives_under_roots_home() {
    assert_eq!(
        super::state_dir(Path::new(crate::service::SYSTEM_DATA)),
        Path::new(crate::service::SYSTEM_HOME).join("update")
    );
    assert_eq!(super::state_dir(Path::new("/d")), Path::new("/d/update"));
}

#[test]
fn the_update_units_run_update_apply_daily_and_on_request() {
    use crate::service;
    let spec = service::Spec {
        exe: "/usr/local/bin/ferrule".into(),
        workspace: "/w".into(),
        config: "/etc/ferrule/config.toml".into(),
        path_env: "/usr/bin".into(),
    };
    let system = service::system_update_unit(&spec, Path::new("/var/lib/ferrule/data"));
    assert!(system.contains("ExecStart=\"/usr/local/bin/ferrule\" update --apply\n"));
    assert!(system.contains("Environment=\"FERRULE_DATA_DIR=/var/lib/ferrule/data\"\n"));
    assert!(system.contains("TimeoutStartSec=7h"));
    assert!(
        !system.contains("ProtectSystem"),
        "it replaces /usr/local/bin/ferrule"
    );
    assert!(!system.contains("User="), "root: it restarts the service");
    let user = service::user_update_unit(&spec);
    assert!(user.contains("ExecStart=\"/usr/local/bin/ferrule\" update --apply\n"));
    let timer = service::update_timer();
    assert!(timer.contains("OnCalendar=daily") && timer.contains("RandomizedDelaySec=6h"));
    let path = service::update_path_unit(Path::new("/var/lib/ferrule/data/update/request"), true);
    assert!(path.contains("PathExists=/var/lib/ferrule/data/update/request\n"));
    assert!(path.contains(&format!("Unit={}", service::UPDATE_SERVICE)));
    assert!(path.contains("WantedBy=paths.target"));
    let plist = service::launchd_update_plist(
        &spec,
        Path::new("/Users/me/data/update/request"),
        Path::new("/Users/me/data/update/update.log"),
        3,
        17,
    );
    assert!(plist.contains("<string>update</string><string>--apply</string>"));
    assert!(plist.contains("<key>Hour</key><integer>3</integer>"));
    assert!(plist.contains("<array><string>/Users/me/data/update/request</string></array>"));
}

#[test]
fn the_example_update_block_parses_uncommented() {
    let example = crate::config::EXAMPLE_CONFIG;
    let start = example.find("# [update]").unwrap();
    let end = start + example[start..].find("\n\n").unwrap();
    let uncommented: String = example[start..end]
        .lines()
        .map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l)))
        .collect();
    let cfg: crate::config::Config = toml::from_str(&uncommented).unwrap();
    assert_eq!(cfg.update.auto, Some(true));
    assert_eq!(cfg.update.channel, Channel::Stable);
    assert!(cfg.update.claude);
    assert!(toml::from_str::<crate::config::Config>(
        "[update]\nauto = true\nchanel = \"stable\"\n"
    )
    .is_err());
    let default: crate::config::Config = toml::from_str("").unwrap();
    assert_eq!(default.update.auto, None);
}

/// `.github/scripts/minisign.sh` (the release job's signer) and the
/// updater's verifier agree.
#[cfg(target_os = "linux")]
#[test]
fn the_release_jobs_signer_makes_signatures_the_updater_accepts() {
    let openssl = |args: &[&str]| std::process::Command::new("openssl").args(args).output();
    let blake = openssl(&["dgst", "-blake2b512", "/dev/null"]);
    if !blake.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no openssl with BLAKE2b-512");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key.pem");
    let made = openssl(&[
        "genpkey",
        "-algorithm",
        "ed25519",
        "-out",
        key.to_str().unwrap(),
    ])
    .unwrap();
    assert!(made.status.success());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/scripts/minisign.sh");
    let run = |args: &[&str]| {
        let out = std::process::Command::new("sh")
            .arg(&script)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let public = run(&["pub", key.to_str().unwrap()]).trim().to_string();
    let file = dir.path().join("ferrule-x86_64-unknown-linux-gnu.tar.gz");
    std::fs::write(&file, b"an archive").unwrap();
    run(&[
        "sign",
        key.to_str().unwrap(),
        "v0.6.0",
        file.to_str().unwrap(),
    ]);
    let sig = std::fs::read_to_string(
        dir.path()
            .join("ferrule-x86_64-unknown-linux-gnu.tar.gz.minisig"),
    )
    .unwrap();
    let comment = "ferrule v0.6.0 ferrule-x86_64-unknown-linux-gnu.tar.gz";
    release::verify_signature(&public, b"an archive", &sig, comment).unwrap();
    assert!(release::verify_signature(&public, b"another", &sig, comment).is_err());
    assert!(release::verify_signature(&public, b"an archive", &sig, "ferrule v0.7.0 x").is_err());
}

/// The real release list, through whatever proxy this machine has
/// (`FERRULE_EXTRA_CA`, as the gateway takes it).
#[tokio::test]
#[ignore = "live: GitHub's API"]
async fn live_the_real_release_list_parses() {
    let mut http = reqwest::Client::builder().user_agent("ferrule-test");
    if let Ok(ca) = std::env::var("FERRULE_EXTRA_CA") {
        let pem = std::fs::read(ca).unwrap();
        for cert in reqwest::Certificate::from_pem_bundle(&pem).unwrap() {
            http = http.add_root_certificate(cert);
        }
    }
    let source = Source::github().with_client(http.build().unwrap());
    let releases = source.list().await.unwrap();
    assert!(
        releases.iter().any(|r| r.tag == "v0.5.1"),
        "{:?}",
        releases.iter().map(|r| &r.tag).collect::<Vec<_>>()
    );
    let r = source.tag("v0.5.1").await.unwrap();
    assert!(r.asset(&release::archive_name(LINUX)).is_some());
    assert!(r
        .asset(&format!("{}.sha256", release::archive_name(LINUX)))
        .is_some());
}

// M36 §5: `claude`, only ever the stand-in (`ferrule-fake-claude`).

/// The stand-in `claude`, next to this test binary's `ferrule` when the
/// workspace built it, else built now.
fn fake_claude() -> PathBuf {
    static FAKE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FAKE.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let profile = exe.parent().and_then(Path::parent).unwrap();
        let fake = profile.join(format!(
            "ferrule-fake-claude{}",
            std::env::consts::EXE_SUFFIX
        ));
        if !fake.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let mut cmd = std::process::Command::new(cargo);
            cmd.args([
                "build",
                "-p",
                "ferrule-plans",
                "--bin",
                "ferrule-fake-claude",
            ]);
            if profile.file_name() == Some("release".as_ref()) {
                cmd.arg("--release");
            }
            assert!(cmd.status().unwrap().success(), "building the fake claude");
        }
        fake
    })
    .clone()
}

/// A copy where claude's native installer puts it, so its version files
/// are this test's own; the copy waits until it runs ("text file busy").
fn native_claude(root: &Path) -> PathBuf {
    let dir = root.join(".local/share/claude/versions/2.1.283");
    std::fs::create_dir_all(&dir).unwrap();
    let claude = dir.join(format!("claude{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(fake_claude(), &claude).unwrap();
    for _ in 0..100 {
        match std::process::Command::new(&claude)
            .arg("--version")
            .output()
        {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(20)),
            _ => break,
        }
    }
    claude
}

const DIST_TAGS: &str = "/-/package/@anthropic-ai/claude-code/dist-tags";

async fn npm(latest: &str) -> Mock {
    let mock = Mock::start().await;
    mock.put(
        DIST_TAGS,
        serde_json::to_vec(&serde_json::json!({"latest": latest, "stable": "2.0.0"})).unwrap(),
    );
    mock
}

fn claude_at(binary: &Path, root: &Path, mock: &Mock) -> super::claude::Claude {
    super::claude::Claude {
        binary: binary.into(),
        config_dir: root.join("claude-code"),
        npm: format!("{}{DIST_TAGS}", mock.base),
    }
}

#[tokio::test]
async fn claude_is_updated_when_npm_has_a_newer_one_and_left_alone_when_current() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let state_dir = super::state_dir(&data);
    std::fs::create_dir_all(&state_dir).unwrap();
    let binary = native_claude(dir.path());
    let mock = npm("2.2.0").await;
    let claude = claude_at(&binary, dir.path(), &mock);

    let done = super::claude::run(&state_dir, &claude, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((done.from.as_str(), done.to.as_str()), ("2.1.283", "2.2.0"));
    let st = State::load(&state_dir);
    assert_eq!(st.claude_installed.as_deref(), Some("2.2.0"));
    assert_eq!(st.claude_latest.as_deref(), Some("2.2.0"));
    assert!(st.claude_checked.is_some());
    assert_eq!(st.events.len(), 1);
    assert_eq!(st.events[0].kind, EventKind::ClaudeUpdated);
    assert_eq!(
        super::notice::event_text(&st.events[0]),
        None,
        "a routine update isn't told"
    );

    // Current: nothing runs.
    std::fs::remove_file(binary.with_file_name("fake-update.json")).unwrap();
    assert_eq!(
        super::claude::run(&state_dir, &claude, None).await.unwrap(),
        None
    );
    assert!(!binary.with_file_name("fake-update.json").exists());
    assert_eq!(State::load(&state_dir).events.len(), 1);
    let lines = super::status_lines(&data, None);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("claude 2.2.0, up to date")),
        "{lines:?}"
    );

    // A turn that needs it forces the update, and is told.
    let done = super::claude::run(&state_dir, &claude, Some("needs an update"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.from, done.to, "the fake is already the latest");
    assert_eq!(State::load(&state_dir).events.len(), 1, "nothing changed");
}

#[tokio::test]
async fn a_failing_claude_update_is_told_once_until_it_works() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let state_dir = super::state_dir(&data);
    std::fs::create_dir_all(&state_dir).unwrap();
    let binary = native_claude(dir.path());
    let fails = binary.with_file_name("fake-update-fails");
    std::fs::write(&fails, "").unwrap();
    let mock = npm("2.2.0").await;
    let mut w = watch(&data, false, None, Source::new("http://127.0.0.1:9"));
    w.claude = Some(claude_at(&binary, dir.path(), &mock));
    let fake = Arc::new(FakeOwner::default());
    let owner: Arc<dyn Owner> = fake.clone();

    w.tick(&owner).await.unwrap();
    let lines = told(&fake);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("The claude CLI couldn't be updated") && lines[0].contains("EACCES"),
        "{lines:?}"
    );
    let report = super::report(&data, None, false);
    assert!(
        report
            .iter()
            .any(|(t, l)| *t == super::Tone::Warn && l.contains("its update failed")),
        "{report:?}"
    );

    // The next day, the same failure: not told again, nor recorded twice.
    let mut st = State::load(&state_dir);
    st.claude_checked = None;
    st.save(&state_dir).unwrap();
    w.tick(&owner).await.unwrap();
    assert_eq!(told(&fake).len(), 1);
    assert_eq!(State::load(&state_dir).events.len(), 1);

    // Then it works: quietly.
    std::fs::remove_file(&fails).unwrap();
    let mut st = State::load(&state_dir);
    st.claude_checked = None;
    st.save(&state_dir).unwrap();
    w.tick(&owner).await.unwrap();
    assert_eq!(told(&fake).len(), 1);
    let st = State::load(&state_dir);
    assert_eq!(st.events.last().unwrap().kind, EventKind::ClaudeUpdated);
    assert_eq!(Told::load(&data).unwrap().claude_failed, None);

    // With the units, the gateway leaves claude to them.
    std::fs::write(binary.with_file_name("fake-latest"), "2.3.0").unwrap();
    let mock = npm("2.3.0").await;
    let mut w = watch(&data, true, None, Source::new("http://127.0.0.1:9"));
    w.claude = Some(claude_at(&binary, dir.path(), &mock));
    let mut st = State::load(&state_dir);
    st.claude_checked = None;
    st.save(&state_dir).unwrap();
    w.tick(&owner).await.unwrap();
    assert_eq!(
        State::load(&state_dir).claude_installed.as_deref(),
        Some("2.2.0")
    );
}

#[tokio::test]
async fn the_gateway_asks_the_unit_to_update_claude_and_waits_for_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let state_dir = super::state_dir(&data);
    std::fs::create_dir_all(&state_dir).unwrap();
    let binary = native_claude(dir.path());
    let mock = npm("2.2.0").await;
    let claude = claude_at(&binary, dir.path(), &mock);

    // The unit: started by the request, it runs claude's update.
    let unit = {
        let (data, claude) = (data.clone(), claude.clone());
        tokio::spawn(async move {
            loop {
                if data.join("update").join(state::REQUEST_FILE).exists() {
                    let exe = std::env::current_exe().unwrap();
                    let settings = crate::config::UpdateConfig::default();
                    super::apply_unit(&settings, &data, &exe, Some(&claude))
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let asked = tokio::spawn({
        let (data, state_dir) = (data.clone(), state_dir.clone());
        async move {
            super::claude::ask_unit(
                &data,
                &state_dir,
                Duration::from_secs(60),
                Duration::from_millis(20),
            )
            .await
        }
    });
    asked.await.unwrap().unwrap();
    unit.await.unwrap();
    let st = State::load(&state_dir);
    assert_eq!(st.claude_installed.as_deref(), Some("2.2.0"));
    let e = st.events.last().unwrap();
    assert_eq!(e.kind, EventKind::ClaudeUpdated);
    assert_eq!(
        super::notice::event_text(e).unwrap(),
        "Updated the claude CLI 2.1.283 → 2.2.0 after a turn failed."
    );

    // A unit that can't: the gateway hears why, and doesn't wait it out.
    std::fs::write(binary.with_file_name("fake-update-fails"), "").unwrap();
    std::fs::write(binary.with_file_name("fake-latest"), "2.3.0").unwrap();
    let unit = {
        let data = data.clone();
        tokio::spawn(async move {
            loop {
                if data.join("update").join(state::REQUEST_FILE).exists() {
                    let exe = std::env::current_exe().unwrap();
                    let settings = crate::config::UpdateConfig::default();
                    // No claude to update here.
                    super::apply_unit(&settings, &data, &exe, None)
                        .await
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let e = super::claude::ask_unit(
        &data,
        &state_dir,
        Duration::from_secs(60),
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    unit.await.unwrap();
    assert!(format!("{e}").contains("updating claude is off"), "{e}");

    // Nobody answers: it gives up after the wait.
    let e = super::claude::ask_unit(
        &data,
        &state_dir,
        Duration::from_millis(100),
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    assert!(format!("{e}").contains("didn't run within"), "{e}");
}

#[tokio::test]
async fn doctor_shows_claudes_version_the_latest_and_how_it_updates() {
    let dir = tempfile::tempdir().unwrap();
    let binary = native_claude(dir.path());
    let mock = npm("2.2.0").await;
    let claude = claude_at(&binary, dir.path(), &mock);
    let (ok, line) = super::claude::doctor_line(&claude, false).await;
    assert!(!ok);
    assert!(
        line.starts_with("claude 2.1.283, 2.2.0 is out (native installer)"),
        "{line}"
    );
    let (ok, line) = super::claude::doctor_line(&claude, true).await;
    assert!(ok, "offline: not known to be behind");
    assert!(line.starts_with("claude 2.1.283 (native"), "{line}");
    let missing = super::claude::Claude {
        binary: dir.path().join("nowhere/claude"),
        ..claude
    };
    let (ok, line) = super::claude::doctor_line(&missing, true).await;
    assert!(!ok && line.starts_with("claude: "), "{line}");
}
