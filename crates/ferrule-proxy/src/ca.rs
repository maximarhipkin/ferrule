//! The proxy's local certificate authority and the placeholder seed.
//!
//! Both are generated once into `<state_dir>/keys/` and reused, so the CA a
//! tool was told to trust and the placeholders it holds survive restarts. Only
//! sandboxed commands are pointed at this CA, via the bundle written next to it.

use anyhow::{bail, Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::crypto::CryptoProvider;
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};

const SEED_LEN: usize = 32;

pub(crate) struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert_der: CertificateDer<'static>,
    pub cert_pem: String,
    pub cert_path: PathBuf,
    provider: Arc<CryptoProvider>,
    leaves: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

/// Load `<state_dir>/keys/{seed,ca.key,ca.pem}`, creating them on first use.
pub(crate) fn load_or_create(
    state_dir: &Path,
    provider: Arc<CryptoProvider>,
) -> Result<(Vec<u8>, Ca)> {
    let dir = state_dir.join("keys");
    if !dir.join("ca.pem").exists() {
        create(state_dir, &dir)?;
    }
    let seed =
        fs::read(dir.join("seed")).with_context(|| format!("reading {}/seed", dir.display()))?;
    if seed.len() != SEED_LEN {
        bail!(
            "{}/seed is corrupt; delete {} to regenerate (placeholders will change)",
            dir.display(),
            dir.display()
        );
    }
    let key_pem = fs::read_to_string(dir.join("ca.key"))?;
    let key = KeyPair::from_pem(&key_pem).context("parsing the proxy CA key")?;
    let cert_path = dir.join("ca.pem");
    let cert_pem = fs::read_to_string(&cert_path)?;
    let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .context("parsing the proxy CA certificate")?
        .into_owned();
    // Same parameters as at creation, so the issuer name and key id on every
    // leaf match the certificate on disk.
    let issuer = Issuer::new(ca_params(&seed), key);
    let ca = Ca {
        issuer,
        cert_der,
        cert_pem,
        cert_path,
        provider,
        leaves: Mutex::new(HashMap::new()),
    };
    Ok((seed, ca))
}

/// Built in a scratch directory and renamed into place, so a concurrent
/// ferrule either wins the race or finds a complete set.
fn create(state_dir: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let rng = SystemRandom::new();
    let mut seed = [0u8; SEED_LEN];
    rng.fill(&mut seed)
        .map_err(|_| anyhow::anyhow!("no system randomness"))?;
    let tmp = state_dir.join(format!(".keys-{}-{}", std::process::id(), hex(&seed[..4])));
    private_dir(&tmp)?;
    let result = (|| -> Result<()> {
        write_private(&tmp.join("seed"), &seed)?;
        let key = KeyPair::generate()?;
        let mut params = ca_params(&seed);
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(3650);
        let cert = params.self_signed(&key)?;
        write_private(&tmp.join("ca.key"), key.serialize_pem().as_bytes())?;
        fs::write(tmp.join("ca.pem"), cert.pem())?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, dir) {
        let _ = fs::remove_dir_all(&tmp);
        if !dir.join("ca.pem").exists() {
            return Err(e).with_context(|| {
                format!(
                    "installing {} (delete it to regenerate the proxy CA)",
                    dir.display()
                )
            });
        }
    }
    Ok(())
}

fn ca_params(seed: &[u8]) -> CertificateParams {
    let mut params = CertificateParams::default();
    let id = hex(&digest::digest(&digest::SHA256, seed).as_ref()[..4]);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("ferrule local proxy CA {id}"));
    dn.push(DnType::OrganizationName, "ferrule");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

impl Ca {
    /// TLS config presenting a certificate for `host`, minted once per host.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        if let Some(cfg) = self.leaves.lock().unwrap().get(host) {
            return Ok(cfg.clone());
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(365);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &self.issuer)?;

        let chain = vec![cert.der().clone(), self.cert_der.clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut cfg = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)?;
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let cfg = Arc::new(cfg);
        self.leaves
            .lock()
            .unwrap()
            .insert(host.to_string(), cfg.clone());
        Ok(cfg)
    }
}

/// The CA bundle ferrule itself trusts: `SSL_CERT_FILE` when it points at a
/// file, else the first system bundle found.
pub fn default_ca_bundle() -> Option<PathBuf> {
    let from_env = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    from_env
        .into_iter()
        .chain(
            [
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/cert.pem",
                "/etc/ssl/ca-bundle.pem",
            ]
            .map(PathBuf::from),
        )
        .find(|p| p.is_file())
}

/// `base` + the proxy CA, content-addressed so ferrules started with different
/// bundles never overwrite each other's file.
pub(crate) fn write_bundle(state_dir: &Path, base: &Path, ca_pem: &str) -> Result<PathBuf> {
    let mut content =
        fs::read(base).with_context(|| format!("reading CA bundle {}", base.display()))?;
    if !content.ends_with(b"\n") {
        content.push(b'\n');
    }
    content.extend_from_slice(ca_pem.as_bytes());
    let id = hex(&digest::digest(&digest::SHA256, &content).as_ref()[..8]);
    let path = state_dir.join(format!("bundle-{id}.pem"));
    if !path.exists() {
        let tmp = state_dir.join(format!(".bundle-{id}-{}", std::process::id()));
        fs::write(&tmp, &content)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    #[test]
    fn keys_are_created_once_and_reloaded_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (seed, ca) = load_or_create(dir.path(), provider()).unwrap();
        let (seed2, ca2) = load_or_create(dir.path(), provider()).unwrap();
        assert_eq!(seed, seed2);
        assert_eq!(ca.cert_pem, ca2.cert_pem);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| {
                fs::metadata(dir.path().join("keys").join(p))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("ca.key"), 0o600);
            assert_eq!(mode("seed"), 0o600);
        }
        ca2.server_config("api.example.com").unwrap();
        ca2.server_config("127.0.0.1").unwrap();
    }

    #[test]
    fn bundle_appends_the_ca_to_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.pem");
        fs::write(&base, "BASE").unwrap();
        let path = write_bundle(dir.path(), &base, "CA\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "BASE\nCA\n");
        assert_eq!(write_bundle(dir.path(), &base, "CA\n").unwrap(), path);
    }
}
