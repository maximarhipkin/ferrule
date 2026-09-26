//! HTTP clients for ferrule's own requests — `web_fetch`, MCP servers
//! reached over HTTP — that leave the way sandboxed commands do.

use ferrule_sandbox::Egress;
use reqwest::{Certificate, Proxy};

/// The header the credential proxy sets on a response it made up because
/// the egress policy refused the request (M33); the body says why and what
/// to change. The same as `ferrule_proxy::EGRESS_HEADER`.
pub const DENIED_HEADER: &str = "x-ferrule-egress";

/// Whether `resp` is the proxy's egress refusal rather than the site's.
pub fn is_denial(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(DENIED_HEADER)
        .is_some_and(|v| v.as_bytes() == b"denied")
}

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
