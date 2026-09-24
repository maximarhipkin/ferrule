//! Credential gateway for sandboxed commands.
//!
//! Each secret in `[secrets]` is bound to the hosts allowed to receive it.
//! Sandboxed commands see a same-shaped placeholder in its env var instead
//! of the real value, and run with `HTTPS_PROXY` pointing at a loopback proxy
//! owned by ferrule. For HTTPS requests to a bound host the proxy swaps the
//! placeholder for the real value in headers and the URL, and swaps it back
//! out of the response; every other host gets a blind tunnel, so a
//! placeholder sent anywhere else is just a useless string. The real values
//! only ever live in ferrule's own memory.
//!
//! This only holds while the OS sandbox keeps commands from reading
//! ferrule's memory or environment (`/proc/<pid>/environ`); see
//! `docs/research-credential-gateway.md` for the full threat model.

mod ca;
mod hosts;
mod placeholder;
mod server;
mod subst;
mod upstream;

pub use ca::default_ca_bundle;
pub use hosts::HostPattern;
pub use placeholder::placeholder;
pub use upstream::Upstream;

use anyhow::{bail, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Every variable a common HTTPS client reads its CA bundle from.
const CA_BUNDLE_VARS: &[&str] = &[
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "NODE_EXTRA_CA_CERTS",
    "DENO_CERT",
    "CARGO_HTTP_CAINFO",
    "AWS_CA_BUNDLE",
];

/// Where one secret may go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretRule {
    /// Host patterns allowed to receive the real value.
    pub hosts: Vec<String>,
    /// Also swap it into request URLs (path and query), for APIs that take
    /// the key there (Telegram's `/bot<token>/`, `?key=`). Off by default:
    /// a URL is often something the host keeps and shows back, so a
    /// hijacked model could have the real value stored where it can read it.
    pub in_url: bool,
}

impl From<Vec<String>> for SecretRule {
    fn from(hosts: Vec<String>) -> Self {
        Self {
            hosts,
            in_url: false,
        }
    }
}

pub struct BrokerConfig {
    /// Env var name → where it may go.
    pub secrets: BTreeMap<String, SecretRule>,
    /// Holds the CA, the placeholder seed and the CA bundles.
    pub state_dir: PathBuf,
    /// Where ferrule's own traffic goes (`Upstream::from_env()`, usually).
    pub upstream: Option<Upstream>,
    /// The CA bundle to trust upstream and to extend for commands;
    /// [`default_ca_bundle`] when `None`.
    pub ca_bundle: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SecretInfo {
    pub name: String,
    pub hosts: Vec<HostPattern>,
    pub placeholder: String,
    pub in_url: bool,
}

pub struct Broker {
    addr: SocketAddr,
    token: String,
    secrets: Vec<SecretInfo>,
    ca_cert_path: PathBuf,
    ca_spki_sha256: String,
    bundle: Option<PathBuf>,
    warnings: Vec<String>,
    task: tokio::task::JoinHandle<()>,
}

impl Broker {
    /// Validates `cfg`, reads each secret through `lookup` (the real
    /// environment, normally) and starts the proxy on the current tokio
    /// runtime. `None` when no configured secret has a value, so there is
    /// nothing to protect. A secret that is configured but unset is a
    /// warning, not an error.
    pub fn start(
        cfg: BrokerConfig,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>> {
        let mut warnings = Vec::new();
        let mut active = Vec::new();
        for (name, rule) in &cfg.secrets {
            let raw_hosts = &rule.hosts;
            if !valid_env_name(name) {
                bail!("[secrets]: `{name}` isn't a valid environment variable name");
            }
            if raw_hosts.is_empty() {
                bail!("[secrets]: `{name}` has no hosts; list the hosts allowed to receive it");
            }
            let hosts = raw_hosts
                .iter()
                .map(|h| HostPattern::parse(h))
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("[secrets]: `{name}`"))?;
            match lookup(name).filter(|v| !v.is_empty()) {
                Some(real) => active.push((name.clone(), hosts, real, rule.in_url)),
                None => warnings.push(format!(
                    "[secrets]: `{name}` isn't set in ferrule's environment; sandboxed commands won't have it"
                )),
            }
        }
        if active.is_empty() {
            for w in &warnings {
                tracing::warn!("{w}");
            }
            return Ok(None);
        }
        let handle = tokio::runtime::Handle::try_current()
            .context("the credential proxy needs a tokio runtime")?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let (seed, ca) = ca::load_or_create(&cfg.state_dir, provider.clone())?;
        let mut secrets = Vec::new();
        let mut infos = Vec::new();
        for (name, hosts, real, in_url) in active {
            let placeholder = placeholder::placeholder(&seed, &name, &real);
            infos.push(SecretInfo {
                name,
                hosts: hosts.clone(),
                placeholder: placeholder.clone(),
                in_url,
            });
            secrets.push(server::Secret {
                hosts,
                placeholder,
                real,
                in_url,
            });
        }

        let base = cfg.ca_bundle.or_else(default_ca_bundle);
        let bundle = match &base {
            Some(base) => Some(ca::write_bundle(&cfg.state_dir, base, &ca.cert_pem)?),
            None => {
                warnings.push(
                    "no system CA bundle found: only Node (NODE_EXTRA_CA_CERTS) will trust the proxy's \
                     certificate, so other tools' requests to secret-bound hosts will fail TLS"
                        .to_string(),
                );
                None
            }
        };
        let tls_client = client_config(provider, base.as_deref())?;

        if let Some(np) = std::env::var("NO_PROXY")
            .ok()
            .or_else(|| std::env::var("no_proxy").ok())
        {
            let bypass = Upstream::parse("http://unused", &np)?;
            for s in &infos {
                for h in &s.hosts {
                    let probe = match h {
                        HostPattern::Exact(h) => h.clone(),
                        HostPattern::Subdomains(s) => format!("ferrule-probe.{s}"),
                    };
                    if bypass.bypasses(&probe) {
                        warnings.push(format!(
                            "NO_PROXY covers {h}, so commands would skip the proxy there and send `{}`'s \
                             placeholder unswapped; remove it from NO_PROXY",
                            s.name
                        ));
                    }
                }
            }
        }

        let mut raw = [0u8; 16];
        SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| anyhow::anyhow!("no system randomness"))?;
        let token = raw.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").context("binding the credential proxy")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let listener = {
            let _guard = handle.enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        let ca_cert_path = ca.cert_path.clone();
        let ca_spki_sha256 = ca.spki_sha256.clone();
        let shared = Arc::new(server::Shared {
            expected_auth: format!("ferrule:{token}").into_bytes(),
            ca,
            secrets,
            upstream: cfg.upstream,
            tls_client,
        });
        let task = handle.spawn(server::serve(listener, shared));
        for w in &warnings {
            tracing::warn!("{w}");
        }
        Ok(Some(Self {
            addr,
            token,
            secrets: infos,
            ca_cert_path,
            ca_spki_sha256,
            bundle,
            warnings,
            task,
        }))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn secrets(&self) -> &[SecretInfo] {
        &self.secrets
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The proxy CA's certificate, alone.
    pub fn ca_cert_path(&self) -> &Path {
        &self.ca_cert_path
    }

    /// Base64 SHA-256 of the proxy CA's public key (SubjectPublicKeyInfo),
    /// for Chrome's `--ignore-certificate-errors-spki-list`: Chrome then
    /// accepts the proxy's certificates without the CA in any trust store.
    pub fn ca_spki_sha256(&self) -> &str {
        &self.ca_spki_sha256
    }

    /// The proxy's `host:port` and the credentials it asks for, apart, for
    /// a client that takes them separately (Chrome).
    pub fn proxy_auth(&self) -> (String, &str, &str) {
        (self.addr.to_string(), "ferrule", &self.token)
    }

    /// The URL commands use as `HTTPS_PROXY`, credentials included.
    pub fn proxy_url(&self) -> String {
        format!("http://ferrule:{}@{}", self.token, self.addr)
    }

    /// Variables to set on every sandboxed command: the placeholders, the
    /// proxy, and a CA bundle that trusts it.
    pub fn child_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .secrets
            .iter()
            .map(|s| (s.name.clone(), s.placeholder.clone()))
            .collect();
        let proxy = self.proxy_url();
        env.push(("HTTPS_PROXY".into(), proxy.clone()));
        env.push(("https_proxy".into(), proxy));
        // Node 24+ only honours HTTPS_PROXY with this set.
        env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
        match &self.bundle {
            Some(bundle) => {
                let bundle = bundle.display().to_string();
                for var in CA_BUNDLE_VARS {
                    env.push(((*var).into(), bundle.clone()));
                }
            }
            None => env.push((
                "NODE_EXTRA_CA_CERTS".into(),
                self.ca_cert_path.display().to_string(),
            )),
        }
        env
    }

    /// For the system prompt, so the model knows the placeholders work.
    pub fn model_note(&self) -> String {
        let mut note = String::from(
            "Some environment variables in the shell hold placeholders, not the real secrets. \
             Use them exactly as you would the real value, over HTTPS to the listed hosts, in the \
             Authorization header (Bearer or Basic) or a credential header such as x-api-key or \
             PRIVATE-TOKEN: ferrule's local proxy swaps the real value in on the way out and \
             back out of responses. Only the ones marked \"URL too\" are swapped in the request \
             URL. Anywhere else (other headers, request bodies, other hosts) the placeholder \
             stays a useless string, and printing it reveals nothing.\n",
        );
        for s in &self.secrets {
            let hosts: Vec<String> = s.hosts.iter().map(ToString::to_string).collect();
            let url = if s.in_url { " (URL too)" } else { "" };
            let _ = writeln!(note, "- ${}: {}{url}", s.name, hosts.join(", "));
        }
        note
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Trusts the Mozilla roots plus `bundle`, which carries any corporate or
/// gateway CA ferrule's own traffic already relies on.
fn client_config(
    provider: Arc<rustls::crypto::CryptoProvider>,
    bundle: Option<&Path>,
) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(bundle) = bundle {
        let certs = CertificateDer::pem_file_iter(bundle)
            .with_context(|| format!("reading CA bundle {}", bundle.display()))?
            .filter_map(Result::ok);
        roots.add_parsable_certificates(certs);
    }
    let mut cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &Path, secrets: &[(&str, &[&str])]) -> BrokerConfig {
        BrokerConfig {
            secrets: secrets
                .iter()
                .map(|(n, h)| {
                    let hosts: Vec<String> = h.iter().map(|s| s.to_string()).collect();
                    (n.to_string(), hosts.into())
                })
                .collect(),
            state_dir: dir.to_path_buf(),
            upstream: None,
            ca_bundle: None,
        }
    }

    #[test]
    fn env_names_are_validated() {
        assert!(valid_env_name("GITHUB_TOKEN"));
        assert!(valid_env_name("_x1"));
        assert!(!valid_env_name("1X"));
        assert!(!valid_env_name("A-B"));
        assert!(!valid_env_name(""));
    }

    #[tokio::test]
    async fn bad_config_fails_and_unset_secrets_only_warn() {
        let dir = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        assert!(Broker::start(cfg(dir.path(), &[("A-B", &["x.com"])]), none).is_err());
        assert!(Broker::start(cfg(dir.path(), &[("TOK", &[])]), none).is_err());
        assert!(Broker::start(cfg(dir.path(), &[("TOK", &["https://x.com"])]), none).is_err());
        let started = Broker::start(cfg(dir.path(), &[("TOK", &["api.x.com"])]), none).unwrap();
        assert!(started.is_none(), "nothing set, nothing to proxy");
        assert!(
            !dir.path().join("keys").exists(),
            "no CA until a secret is live"
        );
    }

    #[tokio::test]
    async fn child_env_carries_placeholders_proxy_and_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.pem");
        std::fs::write(&base, "").unwrap();
        let mut c = cfg(
            dir.path(),
            &[("GH", &["api.github.com"]), ("UNSET", &["x.com"])],
        );
        c.ca_bundle = Some(base);
        let real = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let b = Broker::start(c, |n| (n == "GH").then(|| real.to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(b.secrets().len(), 1);
        assert_eq!(b.warnings().len(), 1);
        let env: BTreeMap<_, _> = b.child_env().into_iter().collect();
        let ph = &env["GH"];
        assert_ne!(ph, real);
        assert_eq!(ph.len(), real.len());
        assert!(env["HTTPS_PROXY"].starts_with("http://ferrule:"));
        assert!(env["HTTPS_PROXY"].ends_with(&b.addr().to_string()));
        let bundle = std::fs::read_to_string(&env["SSL_CERT_FILE"]).unwrap();
        assert!(bundle.contains("BEGIN CERTIFICATE"));
        assert_eq!(env["SSL_CERT_FILE"], env["GIT_SSL_CAINFO"]);
        assert!(env.values().all(|v| !v.contains(real)));
        assert!(b.model_note().contains("- $GH: api.github.com"));
        assert!(!b.model_note().contains(real));
    }
}
