//! A TCP connection, over TLS unless it's a test's plain mock.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, Clone, PartialEq)]
pub struct Addr {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl Addr {
    pub fn tls(host: &str, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: true,
        }
    }

    /// A test's mock on 127.0.0.1.
    pub fn plain(host: &str, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: false,
        }
    }
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub const TIMEOUT: Duration = Duration::from_secs(30);

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            // A proxy's CA (ferrule's gateway, a corporate one) when set.
            for var in ["SSL_CERT_FILE", "FERRULE_EXTRA_CA"] {
                if let Some(path) = std::env::var_os(var) {
                    if let Ok(pem) = std::fs::read(path) {
                        for der in pem_certs(&pem) {
                            let _ = roots.add(der);
                        }
                    }
                }
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Arc::new(
                rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .expect("ring supports the default versions")
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

fn pem_certs(pem: &[u8]) -> Vec<rustls_pki_types::CertificateDer<'static>> {
    use base64::Engine;
    let text = String::from_utf8_lossy(pem);
    let begin = concat!("-----BEGIN ", "CERTIFICATE-----");
    let end = concat!("-----END ", "CERTIFICATE-----");
    let mut out = Vec::new();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find(begin) {
        let after = &rest[at + begin.len()..];
        let Some(stop) = after.find(end) else { break };
        let body: String = after[..stop]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body) {
            out.push(rustls_pki_types::CertificateDer::from(der));
        }
        rest = &after[stop + end.len()..];
    }
    out
}

/// Connects to `addr`; the error is plain.
pub async fn connect(addr: &Addr) -> Result<Box<dyn Stream>, String> {
    let unreachable = || format!("couldn't reach {}:{}", addr.host, addr.port);
    let tcp = tokio::time::timeout(
        TIMEOUT,
        tokio::net::TcpStream::connect((addr.host.as_str(), addr.port)),
    )
    .await
    .map_err(|_| unreachable())?
    .map_err(|_| unreachable())?;
    if !addr.tls {
        return Ok(Box::new(tcp));
    }
    let name = rustls_pki_types::ServerName::try_from(addr.host.clone())
        .map_err(|_| format!("{} isn't a host name", addr.host))?;
    let tls = tokio::time::timeout(
        TIMEOUT,
        tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp),
    )
    .await
    .map_err(|_| unreachable())?
    .map_err(|_| format!("couldn't make a secure connection to {}", addr.host))?;
    Ok(Box::new(tls))
}

/// STARTTLS: the same connection, secured after the plain greeting (IMAP on
/// 143, SMTP on 587).
pub async fn upgrade(stream: Box<dyn Stream>, host: &str) -> Result<Box<dyn Stream>, String> {
    let name = rustls_pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| format!("{host} isn't a host name"))?;
    let tls = tokio::time::timeout(
        TIMEOUT,
        tokio_rustls::TlsConnector::from(tls_config()).connect(name, stream),
    )
    .await
    .map_err(|_| format!("couldn't reach {host}"))?
    .map_err(|_| format!("couldn't make a secure connection to {host}"))?;
    Ok(Box::new(tls))
}
