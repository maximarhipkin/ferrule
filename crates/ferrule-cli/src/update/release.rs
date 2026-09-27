//! Which release, and whether to trust it (docs/m36-self-update.md §2):
//! GitHub's release list, the highest version the channel allows, then the
//! archive's size, sha256 and minisign signature, and the binary inside it
//! running as the version it claims.

use super::Channel;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const API: &str = "https://api.github.com";
pub const REPO: &str = "maximarhipkin/ferrule";
/// This binary's target triple: an update is always the same flavour.
pub const TARGET: &str = env!("FERRULE_TARGET");
/// The keys a release must be signed with, one per line.
pub const KEYS: &str = include_str!("release.pub");
/// No archive is larger; a bigger one is refused before it's downloaded.
pub const MAX_ARCHIVE: u64 = 200 << 20;
/// A `.sha256` or `.minisig` file is a few hundred bytes.
const MAX_SMALL: u64 = 4096;
/// `<new> --version` must answer within this.
const RUNS_WITHIN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// A published release whose tag is a version.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub tag: String,
    pub version: semver::Version,
    /// GitHub's flag, or a pre-release version (`0.6.0-rc.1`).
    pub prerelease: bool,
    pub notes: String,
    pub assets: Vec<Asset>,
}

impl Release {
    /// Drafts, and tags that aren't `v<semver>`, are not releases here.
    fn from_api(r: ApiRelease) -> Option<Self> {
        if r.draft {
            return None;
        }
        let version = parse_version(&r.tag_name)?;
        Some(Self {
            prerelease: r.prerelease || !version.pre.is_empty(),
            tag: r.tag_name,
            version,
            notes: r.body.unwrap_or_default(),
            assets: r.assets,
        })
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The notes' first line with words in it, for the owner's one line.
    pub fn headline(&self) -> String {
        let line = self
            .notes
            .lines()
            .map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']).trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        clip(line, 200)
    }
}

/// `v0.6.0` or `0.6.0`.
pub fn parse_version(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

pub fn current() -> semver::Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// The archive `release.yml` builds for a target.
pub fn archive_name(target: &str) -> String {
    if target.contains("windows") {
        format!("ferrule-{target}.zip")
    } else {
        format!("ferrule-{target}.tar.gz")
    }
}

pub fn binary_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "ferrule.exe"
    } else {
        "ferrule"
    }
}

/// The highest release newer than `current` that the channel allows, isn't
/// pinned and has an archive for `target`.
pub fn choose<'a>(
    releases: &'a [Release],
    current: &semver::Version,
    channel: Channel,
    pinned: &[String],
    target: &str,
) -> Option<&'a Release> {
    let archive = archive_name(target);
    releases
        .iter()
        .filter(|r| channel == Channel::Prerelease || !r.prerelease)
        .filter(|r| r.version > *current)
        .filter(|r| !pinned.iter().any(|p| same_tag(p, &r.tag)))
        .filter(|r| r.asset(&archive).is_some())
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// `v0.6.0` and `0.6.0` are the same release.
pub fn same_tag(a: &str, b: &str) -> bool {
    a.strip_prefix('v').unwrap_or(a) == b.strip_prefix('v').unwrap_or(b)
}

/// Where releases come from, and the keys they must be signed with:
/// GitHub's API and [`KEYS`], or a mock and a test key.
#[derive(Clone)]
pub struct Source {
    base: String,
    http: reqwest::Client,
    keys: String,
}

impl Source {
    pub fn github() -> Self {
        Self::new(API)
    }

    pub fn new(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION"), " (updater)"));
        #[cfg(test)]
        let http = http.no_proxy();
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: http.build().expect("static client config"),
            keys: KEYS.to_string(),
        }
    }

    /// Trust these keys instead of the compiled-in ones.
    #[cfg(test)]
    pub fn with_keys(mut self, keys: &str) -> Self {
        self.keys = keys.to_string();
        self
    }

    /// A client of the caller's (the live tests': one that trusts a
    /// TLS-inspecting proxy's CA).
    #[cfg(test)]
    pub fn with_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The last 30 published releases, drafts dropped.
    pub async fn list(&self) -> Result<Vec<Release>> {
        let url = format!("{}/repos/{REPO}/releases?per_page=30", self.base);
        let list: Vec<ApiRelease> = self.api(&url).await?;
        Ok(list.into_iter().filter_map(Release::from_api).collect())
    }

    /// One release by its tag (`v0.6.0`; `0.6.0` is taken to mean it).
    pub async fn tag(&self, tag: &str) -> Result<Release> {
        let tag = if tag.starts_with('v') {
            tag.to_string()
        } else {
            format!("v{tag}")
        };
        let url = format!("{}/repos/{REPO}/releases/tags/{tag}", self.base);
        let release: ApiRelease = self.api(&url).await?;
        Release::from_api(release).ok_or_else(|| anyhow!("{tag} is not a published release"))
    }

    async fn api<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| anyhow!("couldn't reach GitHub: {}", e.without_url()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!("GitHub has no such release");
        }
        if !status.is_success() {
            bail!("GitHub answered HTTP {}", status.as_u16());
        }
        resp.json()
            .await
            .map_err(|e| anyhow!("GitHub's answer wasn't a release list: {}", e.without_url()))
    }

    /// The bytes at `url`, refusing more than `limit`.
    async fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let mut resp = self
            .http
            .get(url)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| anyhow!("download failed: {}", e.without_url()))?;
        if !resp.status().is_success() {
            bail!("download failed: HTTP {}", resp.status().as_u16());
        }
        if resp.content_length().is_some_and(|n| n > limit) {
            bail!("the download is larger than {} bytes", limit);
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow!("download failed: {}", e.without_url()))?
        {
            if out.len() as u64 + chunk.len() as u64 > limit {
                bail!("the download is larger than {} bytes", limit);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// A release's binary, checked and unpacked into a temp dir of its own.
pub struct Checked {
    pub binary: PathBuf,
    /// The start of the key that signed it; `None` for an old unsigned
    /// release taken with `--to --unsigned`.
    pub signed_by: Option<String>,
    _dir: tempfile::TempDir,
}

/// Download `release`'s archive for `target` and check it: size, sha256,
/// signature (§2.3 2–3). `unsigned_ok` accepts a release with no `.minisig`
/// at all (an old one, asked for by tag); a signature that is there and
/// doesn't verify is refused regardless.
pub async fn fetch(
    source: &Source,
    release: &Release,
    target: &str,
    unsigned_ok: bool,
    temp_in: Option<&Path>,
) -> Result<Checked> {
    let tag = &release.tag;
    let name = archive_name(target);
    let asset = release
        .asset(&name)
        .ok_or_else(|| anyhow!("{tag} has no build for {target} ({name})"))?;
    if asset.size > MAX_ARCHIVE {
        bail!("{name} in {tag} is {} bytes, over the limit", asset.size);
    }
    let sums = release
        .asset(&format!("{name}.sha256"))
        .ok_or_else(|| anyhow!("{tag} has no {name}.sha256"))?;
    let sums = source
        .download(&sums.browser_download_url, MAX_SMALL)
        .await?;
    let want = parse_sha256(&String::from_utf8_lossy(&sums), &name)?;
    let signature = match release.asset(&format!("{name}.minisig")) {
        Some(a) => Some(source.download(&a.browser_download_url, MAX_SMALL).await?),
        None if unsigned_ok => None,
        None => bail!(
            "{tag} isn't signed, so it won't be installed (`ferrule update --to {tag} --unsigned` takes an old unsigned release)"
        ),
    };
    let archive = source
        .download(&asset.browser_download_url, MAX_ARCHIVE)
        .await
        .with_context(|| format!("downloading {name}"))?;
    let got = hex(ring::digest::digest(&ring::digest::SHA256, &archive).as_ref());
    if got != want {
        bail!("{name}'s sha256 is {got}, but {tag} says {want}: refused");
    }
    let signed_by = match signature {
        Some(sig) => Some(verify_signature(
            &source.keys,
            &archive,
            &String::from_utf8_lossy(&sig),
            &format!("ferrule {tag} {name}"),
        )?),
        None => None,
    };
    let dir = match temp_in {
        Some(parent) => tempfile::Builder::new()
            .prefix(".ferrule-update-")
            .tempdir_in(parent)?,
        None => tempfile::Builder::new()
            .prefix("ferrule-update-")
            .tempdir()?,
    };
    let binary = extract(&archive, &name, binary_name(target), dir.path())?;
    Ok(Checked {
        binary,
        signed_by,
        _dir: dir,
    })
}

/// The hex digest in a `sha256sum` line, if the name (when there is one)
/// is the archive's.
pub fn parse_sha256(text: &str, name: &str) -> Result<String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut words = text.split_whitespace();
    let hash = words.next().unwrap_or("").to_ascii_lowercase();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{name}.sha256 holds no sha256");
    }
    if let Some(file) = words.next() {
        if file.trim_start_matches('*') != name {
            bail!("{name}.sha256 is for {file}");
        }
    }
    Ok(hash)
}

/// The signature verifies against one of `keys` and its trusted comment is
/// exactly `comment`. Returns the start of the key that signed.
pub fn verify_signature(keys: &str, data: &[u8], sig: &str, comment: &str) -> Result<String> {
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| anyhow!("the signature file is malformed ({e})"))?;
    let mut why = String::from("no key");
    for line in keys
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let key = minisign_verify::PublicKey::from_base64(line)
            .map_err(|e| anyhow!("a compiled-in release key is malformed ({e})"))?;
        match key.verify(data, &sig, false) {
            Ok(()) if sig.trusted_comment() == comment => {
                return Ok(line.chars().take(12).collect());
            }
            Ok(()) => bail!(
                "the signature is for \"{}\", not \"{comment}\": refused",
                sig.trusted_comment()
            ),
            Err(e) => why = e.to_string(),
        }
    }
    bail!("the signature doesn't verify against Ferrule's release key ({why}): refused")
}

/// Unpack the one file named `binary` from a `.tar.gz` or `.zip`.
pub fn extract(archive: &[u8], name: &str, binary: &str, dir: &Path) -> Result<PathBuf> {
    let out = dir.join(binary);
    let mut found = false;
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
            .context("the archive isn't a zip")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if entry.is_file() && entry_is(entry.name(), binary) {
                let mut file = std::fs::File::create(&out)?;
                std::io::copy(&mut entry, &mut file)?;
                found = true;
                break;
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries().context("the archive isn't a tar.gz")? {
            let mut entry = entry.context("the archive isn't a tar.gz")?;
            let path = entry.path()?.to_string_lossy().into_owned();
            if entry.header().entry_type().is_file() && entry_is(&path, binary) {
                let mut file = std::fs::File::create(&out)?;
                std::io::copy(&mut entry, &mut file)?;
                found = true;
                break;
            }
        }
    }
    if !found {
        bail!("{name} holds no {binary}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(out)
}

/// `ferrule` at the archive's top level (`./ferrule` too).
fn entry_is(path: &str, binary: &str) -> bool {
    path.trim_start_matches("./") == binary
}

/// `<binary> --version` prints `ferrule <version>` within 10 seconds
/// (§2.3 4).
pub fn runs(binary: &Path, version: &semver::Version) -> Result<()> {
    let mut child = std::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("the new binary doesn't start")?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > RUNS_WITHIN {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the new binary didn't answer --version in 10 seconds");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if said != format!("ferrule {version}") {
        bail!(
            "the new binary says \"{}\", not ferrule {version}",
            clip(&said, 80)
        );
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}
