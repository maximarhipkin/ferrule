//! HTTP clients for ferrule's own requests — `web_fetch`, MCP servers
//! reached over HTTP — that leave the way sandboxed commands do.

use ferrule_sandbox::Egress;
use reqwest::{Certificate, NoProxy, Proxy};

/// A client builder whose HTTPS goes through the credential proxy and
/// trusts its CA, when there is a proxy. The proxy only speaks HTTPS, so
/// plain HTTP goes direct, or through `HTTP_PROXY` if one is set. Without a
/// proxy this is reqwest's default: direct, or the system proxy.
pub fn client_builder(egress: Option<&Egress>) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let builder = reqwest::Client::builder();
    let Some(egress) = egress else {
        return Ok(builder);
    };
    let mut builder = builder
        .proxy(Proxy::https(&egress.proxy_url)?)
        .add_root_certificate(Certificate::from_pem(egress.ca_cert_pem.as_bytes())?);
    // A custom proxy switches off reqwest's own reading of the environment.
    let http_proxy = ["HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|v| !v.is_empty()));
    if let Some(url) = http_proxy {
        builder = builder.proxy(Proxy::http(url)?.no_proxy(NoProxy::from_env()));
    }
    Ok(builder)
}
