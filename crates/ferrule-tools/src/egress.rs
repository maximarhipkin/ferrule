//! HTTP clients for ferrule's own requests — `web_fetch`, MCP servers
//! reached over HTTP — that leave the way sandboxed commands do.

use ferrule_sandbox::Egress;
use reqwest::{Certificate, Proxy};

/// A client builder whose requests go through the credential proxy and
/// trust its CA, when there is a proxy: HTTPS as a tunnel, plain HTTP
/// forwarded (the proxy honours `HTTP_PROXY` itself). Without a proxy this
/// is reqwest's default: direct, or the system proxy.
pub fn client_builder(egress: Option<&Egress>) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let builder = reqwest::Client::builder();
    let Some(egress) = egress else {
        return Ok(builder);
    };
    Ok(builder
        .proxy(Proxy::all(&egress.proxy_url)?)
        .add_root_certificate(Certificate::from_pem(egress.ca_cert_pem.as_bytes())?))
}
