//! Secrets at rest: AES-256-GCM under a 32-byte key kept beside the store
//! (or in `$FERRULE_CONNECTIONS_KEY`), a fresh nonce per seal, and the
//! connection's name as associated data, so a sealed blob can't be moved
//! to another connection. It keeps tokens out of anything that copies the
//! store; it doesn't stop someone who can read the private dir.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use std::path::Path;

pub const KEY_ENV: &str = "FERRULE_CONNECTIONS_KEY";

pub struct Sealer {
    key: LessSafeKey,
}

impl Sealer {
    /// `$FERRULE_CONNECTIONS_KEY` (base64url, 32 bytes) if set, else the
    /// key file, made (0600) if it's missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(v) = std::env::var(KEY_ENV).ok().filter(|v| !v.is_empty()) {
            let bytes = unb64(v.trim()).with_context(|| format!("${KEY_ENV}"))?;
            return Self::from_bytes(&bytes).with_context(|| format!("${KEY_ENV}"));
        }
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = unb64(text.trim())
                    .with_context(|| format!("{} isn't a connections key", path.display()))?;
                Self::from_bytes(&bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key: [u8; 32] = random();
                write_private(path, b64(&key).as_bytes())?;
                Self::from_bytes(&key)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The key file only, never made: for readers that must not create one.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if std::env::var(KEY_ENV).is_ok_and(|v| !v.is_empty()) || path.exists() {
            return Self::load_or_create(path).map(Some);
        }
        Ok(None)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            bail!("a connections key is 32 bytes");
        }
        let key = UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| anyhow!("bad key"))?;
        Ok(Self {
            key: LessSafeKey::new(key),
        })
    }

    /// base64url(nonce ‖ ciphertext ‖ tag).
    pub fn seal(&self, aad: &str, plain: &[u8]) -> String {
        let nonce: [u8; NONCE_LEN] = random();
        let mut buf = plain.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad.as_bytes()),
                &mut buf,
            )
            .expect("sealing a buffer in memory");
        let mut out = nonce.to_vec();
        out.extend(buf);
        b64(&out)
    }

    pub fn open(&self, aad: &str, sealed: &str) -> Result<Vec<u8>> {
        let mut raw = unb64(sealed)?;
        if raw.len() < NONCE_LEN + 16 {
            bail!("sealed value too short");
        }
        let body = raw.split_off(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(&raw).map_err(|_| anyhow!("bad nonce"))?;
        let mut body = body;
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(aad.as_bytes()), &mut body)
            .map_err(|_| anyhow!("can't open the sealed value (another key, or edited)"))?;
        Ok(plain.to_vec())
    }
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .expect("the system's random source");
    out
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .map_err(|_| anyhow!("not base64url"))
}

pub fn sha256_b64(bytes: &[u8]) -> String {
    b64(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Write `bytes` to `path` through a temp file and a rename, 0600 on unix,
/// making the parent (0700) if needed.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a path with a parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let name = path.file_name().context("a file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", b64(&random::<6>())));
    {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_only_under_its_name_and_key() {
        let s = Sealer::from_bytes(&[7u8; 32]).unwrap();
        let sealed = s.seal("notion", b"tok_123");
        assert!(!sealed.contains("tok_123"));
        assert_eq!(s.open("notion", &sealed).unwrap(), b"tok_123");
        assert!(s.open("linear", &sealed).is_err(), "bound to its name");
        let other = Sealer::from_bytes(&[8u8; 32]).unwrap();
        assert!(other.open("notion", &sealed).is_err());
        assert_ne!(
            sealed,
            s.seal("notion", b"tok_123"),
            "a fresh nonce each time"
        );
    }

    #[test]
    fn the_key_file_is_made_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/connections.key");
        assert!(Sealer::load(&path).unwrap().is_none());
        let a = Sealer::load_or_create(&path).unwrap();
        let b = Sealer::load_or_create(&path).unwrap();
        assert_eq!(b.open("x", &a.seal("x", b"v")).unwrap(), b"v");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
