//! Where `cloudflared` is, and Cloudflare's own build fetched when it's
//! nowhere. The quick tunnels behind `/dashboard`'s phone link and the
//! OAuth redirect fallback need it, and an owner who never opens a
//! terminal can't install it; a service's pinned `PATH` (or `ProtectHome`)
//! can also hide a copy the owner's shell finds.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Cloudflare's releases; the asset digests GitHub computes are what a
/// download is checked against.
pub const RELEASES: &str = "https://api.github.com/repos/cloudflare/cloudflared/releases/latest";

/// No cloudflared build is near this; a bigger answer isn't one.
const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// One fetch at a time: a second `/dashboard` waits and finds the file.
static FETCHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `[connections] cloudflared`, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cloudflared {
    /// "off": no quick tunnels.
    Off,
    /// The path it's set to, or where it was found.
    At(PathBuf),
    /// Unset and not on this machine: Cloudflare's build goes here the
    /// first time a tunnel is wanted.
    Fetch(PathBuf),
    /// Unset, not found, and no data dir to fetch it into.
    Missing,
}

impl Cloudflared {
    /// `setting` is `[connections] cloudflared`; `bin_dir` is `<data>/bin`,
    /// where a fetched copy lives.
    pub fn resolve(setting: Option<&str>, bin_dir: Option<&Path>) -> Self {
        match setting {
            Some("off") => Self::Off,
            Some(path) => Self::At(PathBuf::from(path)),
            None => match find(bin_dir) {
                Some(path) => Self::At(path),
                None => bin_dir.map_or(Self::Missing, |d| Self::Fetch(d.join(exe_name()))),
            },
        }
    }

    /// Whether a tunnel can be had, perhaps after a fetch.
    pub fn possible(&self) -> bool {
        matches!(self, Self::At(_) | Self::Fetch(_))
    }

    /// Whether getting it means downloading it first.
    pub fn needs_fetch(&self) -> bool {
        matches!(self, Self::Fetch(dest) if !executable(dest))
    }

    /// The binary to run, fetched first if that's what it takes.
    pub async fn path(&self) -> Result<PathBuf> {
        match self {
            Self::Off => bail!("[connections] cloudflared is \"off\""),
            Self::Missing => {
                bail!("cloudflared isn't installed, and there's no data dir to fetch it into")
            }
            Self::At(path) => Ok(path.clone()),
            Self::Fetch(dest) => fetch(RELEASES, dest).await,
        }
    }
}

/// On `PATH`, in the usual install dirs a service's `PATH` may lack, or
/// the copy fetched into `bin_dir`.
pub fn find(bin_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = ferrule_mcp::browser::find_command("cloudflared") {
        return Some(path);
    }
    usual_dirs()
        .into_iter()
        .chain(bin_dir.map(Path::to_path_buf))
        .map(|dir| dir.join(exe_name()))
        .find(|path| executable(path))
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

fn executable(path: &Path) -> bool {
    path.to_str()
        .and_then(ferrule_mcp::browser::find_command)
        .is_some()
}

/// Where Cloudflare's packages, Homebrew, snap and the Windows installers
/// put it.
fn usual_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(p) = std::env::var_os(var) {
                dirs.push(PathBuf::from(p).join("cloudflared"));
            }
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(p)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        }
    } else {
        for d in [
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/local/sbin",
            "/usr/sbin",
            "/opt/homebrew/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/snap/bin",
        ] {
            dirs.push(PathBuf::from(d));
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local").join("bin"));
            dirs.push(home.join("bin"));
        }
    }
    dirs
}

/// Cloudflare's asset for an OS and arch (`std::env::consts`), and
/// whether it's a `.tgz` to unpack (macOS).
pub fn asset_for(os: &str, arch: &str) -> Option<(&'static str, bool)> {
    Some(match (os, arch) {
        ("linux", "x86_64") => ("cloudflared-linux-amd64", false),
        ("linux", "aarch64") => ("cloudflared-linux-arm64", false),
        ("linux", "x86") => ("cloudflared-linux-386", false),
        ("linux", "arm") => ("cloudflared-linux-arm", false),
        ("macos", "x86_64") => ("cloudflared-darwin-amd64.tgz", true),
        ("macos", "aarch64") => ("cloudflared-darwin-arm64.tgz", true),
        // Windows on Arm runs the x64 build.
        ("windows", "x86_64" | "aarch64") => ("cloudflared-windows-amd64.exe", false),
        ("windows", "x86") => ("cloudflared-windows-386.exe", false),
        _ => return None,
    })
}

/// Cloudflare's latest build for this machine into `dest`, checked
/// against its sha256; `api` is [`RELEASES`] outside tests.
pub async fn fetch(api: &str, dest: &Path) -> Result<PathBuf> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()?;
    fetch_with(&http, api, dest).await
}

pub async fn fetch_with(http: &reqwest::Client, api: &str, dest: &Path) -> Result<PathBuf> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let (name, packed) = asset_for(os, arch)
        .with_context(|| format!("Cloudflare publishes no cloudflared for {os}-{arch}"))?;
    let _one = FETCHING.lock().await;
    if executable(dest) {
        return Ok(dest.to_path_buf());
    }
    let release: Value = http
        .get(api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for cloudflared's latest release")?
        .error_for_status()
        .context("asking GitHub for cloudflared's latest release")?
        .json()
        .await
        .context("cloudflared's release list")?;
    let tag = release["tag_name"].as_str().unwrap_or("?");
    let asset = release["assets"]
        .as_array()
        .and_then(|all| all.iter().find(|a| a["name"] == name))
        .with_context(|| format!("cloudflared {tag} has no {name}"))?;
    let want = expected_sha256(&release, asset, name)
        .with_context(|| format!("cloudflared {tag} gives no sha256 for {name}"))?;
    let url = asset["browser_download_url"]
        .as_str()
        .with_context(|| format!("cloudflared {tag}: {name} has no download URL"))?;
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {name}"))?
        .error_for_status()
        .with_context(|| format!("downloading {name}"))?;
    if resp.content_length().is_some_and(|n| n > MAX_BYTES) {
        bail!("{name} is over {} MB", MAX_BYTES / 1024 / 1024);
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("downloading {name}"))?;
    let got = sha256_hex(&bytes);
    if got != want {
        bail!(
            "cloudflared {tag}: {name}'s sha256 is {got}, the release says {want}; not installed"
        );
    }
    let bin = if packed {
        unpack(&bytes).with_context(|| format!("unpacking {name}"))?
    } else {
        bytes.to_vec()
    };
    install(&bin, dest)?;
    tracing::info!("fetched cloudflared {tag} into {}", dest.display());
    Ok(dest.to_path_buf())
}

/// GitHub's own digest of the asset, else the line for it in the
/// release notes (Cloudflare lists `<name>: <sha256>`).
fn expected_sha256(release: &Value, asset: &Value, name: &str) -> Option<String> {
    if let Some(hex) = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        return Some(hex.to_ascii_lowercase());
    }
    let prefix = format!("{name}:");
    release["body"].as_str()?.lines().find_map(|line| {
        let hex = line.trim().strip_prefix(&prefix)?.trim();
        (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `cloudflared` inside a macOS `.tgz`.
fn unpack(tgz: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry
            .path()?
            .file_name()
            .is_some_and(|n| n == "cloudflared")
        {
            let mut bin = Vec::new();
            entry.read_to_end(&mut bin)?;
            return Ok(bin);
        }
    }
    bail!("no cloudflared inside")
}

/// Written beside `dest`, made executable, then renamed over it.
fn install(bin: &[u8], dest: &Path) -> Result<()> {
    let dir = dest.parent().context("no dir to put cloudflared in")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = dir.join(format!(".{}.part", exe_name()));
    std::fs::write(&part, bin).with_context(|| format!("writing {}", part.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&part, dest).with_context(|| format!("moving it to {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers GET `/release` with `release` (its `{base}` replaced by the
    /// server's URL) and GET `/asset` with `asset`.
    async fn serve(release: String, asset: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release = release.replace("{base}", &base);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (release, asset) = (release.clone(), asset.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut got = Vec::new();
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = sock.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                    }
                    let head = String::from_utf8_lossy(&got);
                    let body = if head.starts_with("GET /release ") {
                        release.into_bytes()
                    } else if head.starts_with("GET /asset ") {
                        asset
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                        return;
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        base
    }

    fn release_json(digest: Option<&str>, body: &str) -> String {
        let (name, _) = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let mut asset = serde_json::json!({
            "name": name,
            "browser_download_url": "{base}/asset",
        });
        if let Some(d) = digest {
            asset["digest"] = Value::String(d.into());
        }
        serde_json::json!({
            "tag_name": "2026.9.3",
            "body": body,
            "assets": [{"name": "cloudflared-other", "browser_download_url": "{base}/nope"}, asset],
        })
        .to_string()
    }

    /// What the asset for this machine is served as: the binary, or a
    /// `.tgz` holding it on macOS.
    fn packaged(bin: &[u8]) -> Vec<u8> {
        let (_, packed) = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        if !packed {
            return bin.to_vec();
        }
        tgz(bin)
    }

    fn tgz(bin: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(bin.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        tar.append_data(&mut h, "cloudflared", bin).unwrap();
        let tar = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn every_platform_ferrule_ships_for_has_an_asset() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
        ] {
            assert!(asset_for(os, arch).is_some(), "{os}-{arch}");
        }
        assert_eq!(
            asset_for("macos", "aarch64"),
            Some(("cloudflared-darwin-arm64.tgz", true))
        );
        assert_eq!(asset_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn off_a_path_found_or_fetched() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Cloudflared::resolve(Some("off"), Some(dir.path())),
            Cloudflared::Off
        );
        assert_eq!(
            Cloudflared::resolve(Some("/opt/cf"), Some(dir.path())),
            Cloudflared::At("/opt/cf".into())
        );
        let off = Cloudflared::Off;
        assert!(!off.possible());
        // Unset: whatever this machine has, or a fetch into bin_dir.
        match Cloudflared::resolve(None, Some(dir.path())) {
            Cloudflared::At(p) => assert!(executable(&p)),
            Cloudflared::Fetch(dest) => {
                assert_eq!(dest, dir.path().join(exe_name()));
                let c = Cloudflared::Fetch(dest);
                assert!(c.possible() && c.needs_fetch());
            }
            other => panic!("{other:?}"),
        }
        if find(None).is_none() {
            assert_eq!(Cloudflared::resolve(None, None), Cloudflared::Missing);
        }
    }

    #[test]
    fn the_sha256_is_githubs_digest_else_the_release_notes_line() {
        let asset = serde_json::json!({"digest": "sha256:ABCD"});
        let release = serde_json::json!({"body": ""});
        assert_eq!(
            expected_sha256(&release, &asset, "x").as_deref(),
            Some("abcd")
        );
        let hex = "0f".repeat(32);
        let release = serde_json::json!({
            "body": format!("### SHA256 Checksums:\n```\ncloudflared-linux-amd64.deb: {}\ncloudflared-linux-amd64: {hex}\n```", "1".repeat(64)),
        });
        assert_eq!(
            expected_sha256(&release, &serde_json::json!({}), "cloudflared-linux-amd64").as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            expected_sha256(&release, &serde_json::json!({}), "cloudflared-linux-arm64"),
            None
        );
    }

    #[test]
    fn the_binary_comes_out_of_the_macos_tgz() {
        assert_eq!(unpack(&tgz(b"MACHO")).unwrap(), b"MACHO");
    }

    #[tokio::test]
    async fn the_latest_build_is_fetched_checked_and_left_executable() {
        let bin = b"#!/bin/sh\necho cloudflared version 2026.9.3\n".to_vec();
        let served = packaged(&bin);
        let digest = format!("sha256:{}", sha256_hex(&served));
        let base = serve(release_json(Some(&digest), ""), served).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin").join(exe_name());
        let got = fetch_with(&client(), &format!("{base}/release"), &dest)
            .await
            .unwrap();
        assert_eq!(got, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), bin);
        assert!(executable(&dest));
        assert!(!Cloudflared::Fetch(dest.clone()).needs_fetch());
        // Found in bin_dir from now on, and not fetched again.
        assert!(find(Some(&dir.path().join("bin"))).is_some());
        let again = fetch_with(&client(), "http://127.0.0.1:9/unreachable", &dest)
            .await
            .unwrap();
        assert_eq!(again, dest);
    }

    #[tokio::test]
    async fn a_download_that_doesnt_match_its_sha256_isnt_installed() {
        let served = packaged(b"tampered");
        let digest = format!("sha256:{}", "0".repeat(64));
        let base = serve(release_json(Some(&digest), ""), served).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bin").join(exe_name());
        let e = fetch_with(&client(), &format!("{base}/release"), &dest)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("sha256"), "{e:#}");
        assert!(!dest.exists());
        assert!(!dir
            .path()
            .join("bin")
            .join(format!(".{}.part", exe_name()))
            .exists());
    }

    #[tokio::test]
    async fn a_release_without_a_checksum_isnt_used() {
        let base = serve(release_json(None, "no hashes here"), packaged(b"bin")).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(exe_name());
        let e = fetch_with(&client(), &format!("{base}/release"), &dest)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("no sha256"), "{e:#}");
        assert!(!dest.exists());
    }

    /// `cargo test -p ferrule-connections -- --ignored cloudflared_live`:
    /// the real latest release has this machine's asset and its digest.
    #[tokio::test]
    #[ignore]
    async fn cloudflared_live_the_real_release_has_our_asset() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(exe_name());
        let got = fetch(RELEASES, &dest).await.unwrap();
        let out = std::process::Command::new(&got)
            .arg("--version")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("cloudflared version"));
    }
}
