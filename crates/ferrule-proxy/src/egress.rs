//! M33: the egress policy. Which hosts sandboxed commands and ferrule's own
//! tools may reach through the proxy, and the private-range guard that keeps
//! a fetched page from steering the model at loopback, the LAN or a cloud
//! metadata endpoint.
//!
//! Names are resolved once, here; every address is checked, and the proxy
//! connects to the addresses that were checked, so a rebinding DNS server
//! can't swap in a private address between the check and the connect.

use crate::hosts::HostPattern;
use anyhow::{bail, Context, Result};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Who is asking, from the proxy credential's user name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Sandboxed commands and the helpers they start (`ferrule`).
    Command,
    /// Ferrule's own clients acting for the model: `web_fetch`, search, MCP
    /// over HTTP, plugins (`ferrule-tool`).
    Tool,
}

impl Source {
    pub fn user(self) -> &'static str {
        match self {
            Self::Command => "ferrule",
            Self::Tool => "ferrule-tool",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Command => "command",
            Self::Tool => "tool",
        })
    }
}

/// What one rule names: a host pattern or an address range.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Host(HostPattern),
    Net(IpNet),
}

/// One `[egress]` entry: `api.github.com`, `*.example.com`, `10.0.0.0/8`,
/// `2001:db8::1`, each optionally with a port (`localhost:11434`,
/// `[::1]:8080`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    target: Target,
    port: Option<u16>,
}

impl Rule {
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            bail!("empty egress rule");
        }
        if s.contains("://") {
            bail!("`{raw}`: give a host, IP or CIDR, not a URL");
        }
        // `[v6]` or `[v6]:port`.
        if let Some(rest) = s.strip_prefix('[') {
            let (ip, after) = rest
                .split_once(']')
                .with_context(|| format!("`{raw}`: unclosed `[`"))?;
            let ip: Ipv6Addr = ip
                .parse()
                .with_context(|| format!("`{raw}`: not an IPv6 address"))?;
            let port = match after {
                "" => None,
                p => Some(parse_port(raw, p.strip_prefix(':').unwrap_or(p))?),
            };
            return Ok(Self {
                target: Target::Net(IpNet::host(IpAddr::V6(ip))),
                port,
            });
        }
        if s.contains('/') {
            return Ok(Self {
                target: Target::Net(IpNet::parse(s).with_context(|| format!("`{raw}`"))?),
                port: None,
            });
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(Self {
                target: Target::Net(IpNet::host(ip)),
                port: None,
            });
        }
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) => (h, Some(parse_port(raw, p)?)),
            None => (s, None),
        };
        let target = match host.parse::<Ipv4Addr>() {
            Ok(ip) => Target::Net(IpNet::host(IpAddr::V4(ip))),
            Err(_) => Target::Host(HostPattern::parse(host)?),
        };
        Ok(Self { target, port })
    }

    /// An exact `host:port`, for the endpoints ferrule's config names.
    pub fn endpoint(host: &str, port: u16) -> Result<Self> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let target = match host.parse::<IpAddr>() {
            Ok(ip) => Target::Net(IpNet::host(ip)),
            Err(_) => Target::Host(HostPattern::parse(host)?),
        };
        Ok(Self {
            target,
            port: Some(port),
        })
    }

    fn port_ok(&self, port: u16) -> bool {
        self.port.is_none_or(|p| p == port)
    }

    /// Matches the requested name (never an IP rule: a name isn't an
    /// address until it's resolved).
    fn matches_name(&self, host: &str, port: u16) -> bool {
        self.port_ok(port) && matches!(&self.target, Target::Host(h) if h.matches(host))
    }

    fn matches_ip(&self, ip: IpAddr, port: u16) -> bool {
        self.port_ok(port) && matches!(&self.target, Target::Net(n) if n.contains(ip))
    }

    /// Names exactly this one address, the only kind of rule that opens a
    /// cloud metadata endpoint.
    fn is_exact_ip(&self, ip: IpAddr) -> bool {
        matches!(&self.target, Target::Net(n) if n.prefix == n.max() && n.contains(ip))
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.target, self.port) {
            (Target::Host(h), None) => write!(f, "{h}"),
            (Target::Host(h), Some(p)) => write!(f, "{h}:{p}"),
            (Target::Net(n), None) => write!(f, "{n}"),
            (Target::Net(n), Some(p)) if n.addr.is_ipv6() => write!(f, "[{}]:{p}", n.addr),
            (Target::Net(n), Some(p)) => write!(f, "{}:{p}", n.addr),
        }
    }
}

fn parse_port(raw: &str, p: &str) -> Result<u16> {
    p.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .with_context(|| format!("`{raw}`: bad port `{p}`"))
}

/// An address range in CIDR form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    fn host(addr: IpAddr) -> Self {
        let addr = canonical(addr);
        let prefix = if addr.is_ipv4() { 32 } else { 128 };
        Self { addr, prefix }
    }

    fn parse(s: &str) -> Result<Self> {
        let (ip, len) = s.split_once('/').context("expected address/length")?;
        let addr = canonical(ip.parse::<IpAddr>().context("not an IP address")?);
        let prefix: u8 = len.parse().context("bad prefix length")?;
        let net = Self { addr, prefix };
        if prefix > net.max() {
            bail!("prefix /{prefix} is too long");
        }
        Ok(net)
    }

    fn max(&self) -> u8 {
        if self.addr.is_ipv4() {
            32
        } else {
            128
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix == self.max() {
            write!(f, "{}", self.addr)
        } else {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }
}

/// An IPv6 address that carries an IPv4 one (mapped, compatible or
/// NAT64) is judged as that IPv4 address.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            // ::a.b.c.d (deprecated "compatible"), but not :: or ::1.
            if s[..6] == [0; 6] && (s[6] != 0 || s[7] > 1) {
                return IpAddr::V4(Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])));
            }
            // 64:ff9b::/96, the NAT64 well-known prefix.
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return IpAddr::V4(Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])));
            }
            ip
        }
        v4 => v4,
    }
}

/// Cloud instance metadata endpoints: AWS/GCP/Azure/OCI (`169.254.169.254`),
/// AWS over IPv6 and Alibaba.
pub fn is_metadata(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            v4 == Ipv4Addr::new(169, 254, 169, 254) || v4 == Ipv4Addr::new(100, 100, 100, 200)
        }
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    }
}

/// Why `ip` isn't a public internet address, or `None` when it is.
pub fn private_kind(ip: IpAddr) -> Option<&'static str> {
    if is_metadata(ip) {
        return Some("cloud metadata");
    }
    match canonical(ip) {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let kind = match o {
                [127, ..] => "loopback",
                [0, ..] => "unspecified",
                [10, ..] | [192, 168, ..] => "private network",
                [172, b, ..] if (16..32).contains(&b) => "private network",
                [100, b, ..] if (64..128).contains(&b) => "carrier-grade NAT",
                [169, 254, ..] => "link-local",
                [192, 0, 0, _] => "IETF protocol assignment",
                [198, b, ..] if b == 18 || b == 19 => "benchmarking",
                [a, ..] if a >= 224 => "multicast or reserved",
                _ => return None,
            };
            Some(kind)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                Some("loopback")
            } else if v6.is_unspecified() {
                Some("unspecified")
            } else if s[0] & 0xfe00 == 0xfc00 {
                Some("unique local")
            } else if s[0] & 0xffc0 == 0xfe80 {
                Some("link-local")
            } else if s[0] & 0xff00 == 0xff00 {
                Some("multicast")
            } else {
                None
            }
        }
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A `deny` rule matched.
    Rule,
    /// A private address, and nothing opens it.
    Private,
    /// `default = "deny"` and no `allow` rule matched.
    NotAllowed,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "denied_rule",
            Self::Private => "private_address",
            Self::NotAllowed => "not_allowed",
        }
    }
}

/// One refused request, for the ledger, the audit log and the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub source: Source,
    pub host: String,
    pub port: u16,
    /// `"CONNECT"` or the plain-HTTP method.
    pub method: String,
    pub reason: Reason,
    /// For the message: the rule that matched, or the address and its kind.
    pub detail: String,
}

impl Denial {
    /// What the client (and so the model) reads.
    pub fn message(&self, scheme: &str) -> String {
        let target = if self.host.contains(':') {
            format!("{scheme}://[{}]:{}/", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}/", self.host, self.port)
        };
        let (why, fix) = match self.reason {
            Reason::Rule => (
                format!("it matches the deny rule `{}`", self.detail),
                "If this host is needed, ask the owner to remove that rule from [egress] deny."
                    .to_string(),
            ),
            Reason::Private => (
                format!("private address {}", self.detail),
                "If this host is needed, ask the owner to add it to [egress] private_allow."
                    .to_string(),
            ),
            Reason::NotAllowed => (
                "the policy only allows listed hosts, and this one isn't listed".to_string(),
                "If this host is needed, ask the owner to add it to [egress] allow.".to_string(),
            ),
        };
        format!(
            "ferrule egress policy: blocked {target} ({why}).\n\
             This is the owner's network policy, not a network error; retrying won't help.\n\
             {fix} See docs/egress.md.\n"
        )
    }
}

/// Where the proxy may send a request once it's vetted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Connect to these (checked) addresses.
    Addrs(Vec<SocketAddr>),
    /// Hand the name to the upstream proxy.
    Upstream,
    Deny(Denial),
    /// The name doesn't resolve: a plain network error, not a denial.
    Unresolved(String),
}

#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    /// `default = "deny"`: only what `allow` names goes.
    pub default_deny: bool,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
    /// `private = "block"` (the default).
    pub block_private: bool,
    private_allow: Vec<Rule>,
    /// Endpoints ferrule's own config names (model servers, MCP, the
    /// collector): through the private guard and `default = "deny"`, but
    /// not through `deny`.
    implicit: Vec<Rule>,
}

impl EgressPolicy {
    /// The default policy: public hosts allowed, private ranges blocked.
    pub fn standard() -> Self {
        Self {
            block_private: true,
            ..Self::default()
        }
    }

    pub fn new(
        default_deny: bool,
        allow: &[String],
        deny: &[String],
        block_private: bool,
        private_allow: &[String],
    ) -> Result<Self> {
        let parse = |key: &str, list: &[String]| {
            list.iter()
                .map(|r| Rule::parse(r).with_context(|| format!("[egress] {key}")))
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            default_deny,
            allow: parse("allow", allow)?,
            deny: parse("deny", deny)?,
            block_private,
            private_allow: parse("private_allow", private_allow)?,
            implicit: Vec::new(),
        })
    }

    /// Opens `host:port` through the private guard and `default = "deny"`.
    pub fn allow_endpoint(&mut self, host: &str, port: u16) -> Result<()> {
        let rule = Rule::endpoint(host, port)?;
        if !self.implicit.contains(&rule) {
            self.implicit.push(rule);
        }
        Ok(())
    }

    /// Whether this policy says anything beyond the default: shell commands
    /// only get the proxy env for it when it does.
    pub fn has_rules(&self) -> bool {
        self.default_deny
            || !self.allow.is_empty()
            || !self.deny.is_empty()
            || !self.private_allow.is_empty()
    }

    pub fn allow(&self) -> &[Rule] {
        &self.allow
    }
    pub fn deny(&self) -> &[Rule] {
        &self.deny
    }
    pub fn private_allow(&self) -> &[Rule] {
        &self.private_allow
    }
    pub fn implicit(&self) -> &[Rule] {
        &self.implicit
    }

    /// The checks that need no DNS: `deny` on the name and, for an IP
    /// literal, everything.
    fn check_ip(
        &self,
        source: Source,
        host: &str,
        ip: IpAddr,
        port: u16,
    ) -> Result<(), (Reason, String)> {
        if let Some(r) = self.deny.iter().find(|r| r.matches_ip(ip, port)) {
            return Err((Reason::Rule, r.to_string()));
        }
        let implicit = self
            .implicit
            .iter()
            .any(|r| r.matches_name(host, port) || r.matches_ip(ip, port));
        if self.block_private {
            if let Some(kind) = private_kind(ip) {
                let command_loopback = source == Source::Command && canonical(ip).is_loopback();
                let opened = if is_metadata(ip) {
                    self.private_allow
                        .iter()
                        .any(|r| r.is_exact_ip(ip) && r.port_ok(port))
                } else {
                    command_loopback
                        || implicit
                        || self
                            .private_allow
                            .iter()
                            .any(|r| r.matches_name(host, port) || r.matches_ip(ip, port))
                };
                if !opened {
                    return Err((Reason::Private, format!("{} ({kind})", canonical(ip))));
                }
            }
        }
        Ok(())
    }

    fn allowed_by_default(&self, source: Source, host: &str, ips: &[IpAddr], port: u16) -> bool {
        if !self.default_deny {
            return true;
        }
        let listed =
            |r: &Rule| r.matches_name(host, port) || ips.iter().any(|ip| r.matches_ip(*ip, port));
        self.allow.iter().any(listed)
            || self.implicit.iter().any(listed)
            || (source == Source::Command
                && !ips.is_empty()
                && ips.iter().all(|ip| canonical(*ip).is_loopback()))
    }

    /// Decides one request. `via_upstream`: the connection would go through
    /// ferrule's own upstream proxy, which then does the connecting.
    pub async fn vet(
        &self,
        source: Source,
        method: &str,
        host: &str,
        port: u16,
        via_upstream: bool,
    ) -> Verdict {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let deny = |reason, detail| {
            Verdict::Deny(Denial {
                source,
                host: host.clone(),
                port,
                method: method.to_string(),
                reason,
                detail,
            })
        };
        if let Some(r) = self.deny.iter().find(|r| r.matches_name(&host, port)) {
            return deny(Reason::Rule, r.to_string());
        }
        let literal = host.parse::<IpAddr>().ok();
        let addrs: Vec<SocketAddr> = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(it) => it.collect(),
                // Only the upstream proxy may be able to resolve it (split
                // DNS behind a corporate proxy): let it, see docs/egress.md.
                Err(_) if via_upstream => {
                    if self.default_deny && !self.allowed_by_default(source, &host, &[], port) {
                        return deny(Reason::NotAllowed, String::new());
                    }
                    tracing::debug!(
                        "egress: {host} doesn't resolve here; letting the upstream proxy try"
                    );
                    return Verdict::Upstream;
                }
                Err(e) => return Verdict::Unresolved(format!("resolving {host}: {e}")),
            },
        };
        if addrs.is_empty() {
            return Verdict::Unresolved(format!("{host} has no addresses"));
        }
        for a in &addrs {
            if let Err((reason, detail)) = self.check_ip(source, &host, a.ip(), port) {
                return deny(reason, detail);
            }
        }
        let ips: Vec<IpAddr> = addrs.iter().map(SocketAddr::ip).collect();
        // An IP literal is judged by its address only: a host allowlist
        // mustn't be sidestepped by resolving the name yourself.
        let name = if literal.is_some() { "" } else { host.as_str() };
        if !self.allowed_by_default(source, name, &ips, port) {
            return deny(Reason::NotAllowed, String::new());
        }
        if via_upstream {
            Verdict::Upstream
        } else {
            Verdict::Addrs(addrs)
        }
    }

    /// For `ferrule doctor`: one line per part.
    pub fn describe(&self) -> Vec<String> {
        let list = |rules: &[Rule]| {
            if rules.is_empty() {
                "none".to_string()
            } else {
                rules
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let mut lines = vec![format!(
            "default: {}",
            if self.default_deny {
                "deny (only `allow` goes)"
            } else {
                "allow public hosts"
            }
        )];
        if self.default_deny || !self.allow.is_empty() {
            lines.push(format!("allow: {}", list(&self.allow)));
        }
        lines.push(format!("deny: {}", list(&self.deny)));
        lines.push(format!(
            "private ranges: {}",
            if self.block_private {
                "blocked (loopback stays open to shell commands)"
            } else {
                "allowed"
            }
        ));
        if !self.private_allow.is_empty() {
            lines.push(format!("private_allow: {}", list(&self.private_allow)));
        }
        if !self.implicit.is_empty() {
            lines.push(format!("configured endpoints: {}", list(&self.implicit)));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(
        default_deny: bool,
        allow: &[&str],
        deny: &[&str],
        private_allow: &[&str],
    ) -> EgressPolicy {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        EgressPolicy::new(default_deny, &v(allow), &v(deny), true, &v(private_allow)).unwrap()
    }

    fn reason(v: Verdict) -> Option<Reason> {
        match v {
            Verdict::Deny(d) => Some(d.reason),
            _ => None,
        }
    }

    #[test]
    fn rules_parse_hosts_ips_cidrs_and_ports() {
        for (raw, shown) in [
            ("api.github.com", "api.github.com"),
            ("*.example.com", "*.example.com"),
            ("localhost:11434", "localhost:11434"),
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("192.168.1.5", "192.168.1.5"),
            ("192.168.1.5:8080", "192.168.1.5:8080"),
            ("2001:db8::1", "2001:db8::1"),
            ("[::1]:8080", "[::1]:8080"),
            ("fd00::/8", "fd00::/8"),
            ("::ffff:10.0.0.1", "10.0.0.1"),
        ] {
            assert_eq!(Rule::parse(raw).unwrap().to_string(), shown, "{raw}");
        }
        for bad in [
            "",
            "https://x.com",
            "*.com",
            "10.0.0.0/33",
            "host:0",
            "host:99999",
            "[::1",
            "a..b",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cidrs_contain_what_they_say() {
        let net = IpNet::parse("10.1.0.0/16").unwrap();
        assert!(net.contains("10.1.200.3".parse().unwrap()));
        assert!(!net.contains("10.2.0.1".parse().unwrap()));
        assert!(
            net.contains("::ffff:10.1.0.9".parse().unwrap()),
            "mapped v4"
        );
        let all = IpNet::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        let v6 = IpNet::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::5".parse().unwrap()));
        assert!(!v6.contains("2001:db9::".parse().unwrap()));
    }

    #[test]
    fn private_ranges_and_their_disguises_are_recognised() {
        for ip in [
            "127.0.0.1",
            "127.8.9.10",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "169.254.1.1",
            "100.64.0.1",
            "100.100.100.200",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::10.0.0.1",
        ] {
            assert!(
                private_kind(ip.parse().unwrap()).is_some(),
                "{ip} is private"
            );
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
            "64:ff9b::8.8.8.8",
        ] {
            assert!(
                private_kind(ip.parse().unwrap()).is_none(),
                "{ip} is public"
            );
        }
        assert!(is_metadata("169.254.169.254".parse().unwrap()));
        assert!(is_metadata("::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_metadata("fd00:ec2::254".parse().unwrap()));
    }

    #[tokio::test]
    async fn default_policy_allows_public_and_blocks_private() {
        let p = EgressPolicy::standard();
        assert!(!p.has_rules());
        assert!(matches!(
            p.vet(Source::Tool, "GET", "8.8.8.8", 443, false).await,
            Verdict::Addrs(_)
        ));
        for host in [
            "127.0.0.1",
            "169.254.169.254",
            "10.1.2.3",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert_eq!(
                reason(p.vet(Source::Tool, "GET", host, 80, false).await),
                Some(Reason::Private),
                "{host}"
            );
        }
        // A name that resolves to loopback is judged by its address.
        assert_eq!(
            reason(p.vet(Source::Tool, "GET", "localhost", 80, false).await),
            Some(Reason::Private)
        );
    }

    #[tokio::test]
    async fn loopback_stays_open_to_commands_but_not_metadata_or_the_lan() {
        let p = EgressPolicy::standard();
        assert!(matches!(
            p.vet(Source::Command, "GET", "127.0.0.1", 3000, false)
                .await,
            Verdict::Addrs(_)
        ));
        assert!(matches!(
            p.vet(Source::Command, "GET", "localhost", 3000, false)
                .await,
            Verdict::Addrs(_)
        ));
        assert_eq!(
            reason(
                p.vet(Source::Command, "GET", "169.254.169.254", 80, false)
                    .await
            ),
            Some(Reason::Private)
        );
        assert_eq!(
            reason(
                p.vet(Source::Command, "GET", "192.168.1.1", 80, false)
                    .await
            ),
            Some(Reason::Private)
        );
    }

    #[tokio::test]
    async fn configured_endpoints_pass_the_guard_on_their_port_only() {
        let mut p = EgressPolicy::standard();
        p.allow_endpoint("localhost", 11434).unwrap();
        p.allow_endpoint("127.0.0.1", 8080).unwrap();
        assert!(matches!(
            p.vet(Source::Tool, "GET", "localhost", 11434, false).await,
            Verdict::Addrs(_)
        ));
        assert!(matches!(
            p.vet(Source::Tool, "GET", "127.0.0.1", 8080, false).await,
            Verdict::Addrs(_)
        ));
        assert_eq!(
            reason(p.vet(Source::Tool, "GET", "127.0.0.1", 2375, false).await),
            Some(Reason::Private)
        );
    }

    #[tokio::test]
    async fn metadata_needs_an_exact_ip_entry() {
        let broad = policy(false, &[], &[], &["169.254.0.0/16"]);
        assert_eq!(
            reason(
                broad
                    .vet(Source::Tool, "GET", "169.254.169.254", 80, false)
                    .await
            ),
            Some(Reason::Private)
        );
        assert!(matches!(
            broad
                .vet(Source::Tool, "GET", "169.254.9.9", 80, false)
                .await,
            Verdict::Addrs(_)
        ));
        let exact = policy(false, &[], &[], &["169.254.169.254"]);
        assert!(matches!(
            exact
                .vet(Source::Tool, "GET", "169.254.169.254", 80, false)
                .await,
            Verdict::Addrs(_)
        ));
        let off = EgressPolicy::new(false, &[], &[], false, &[]).unwrap();
        assert!(matches!(
            off.vet(Source::Tool, "GET", "169.254.169.254", 80, false)
                .await,
            Verdict::Addrs(_)
        ));
    }

    #[tokio::test]
    async fn deny_wins_and_wildcards_match_subdomains() {
        let p = policy(
            false,
            &["*.example.com"],
            &["*.example.com", "8.8.4.0/24"],
            &[],
        );
        assert_eq!(
            reason(
                p.vet(Source::Tool, "GET", "a.example.com", 443, false)
                    .await
            ),
            Some(Reason::Rule)
        );
        assert_eq!(
            reason(p.vet(Source::Tool, "GET", "8.8.4.4", 443, false).await),
            Some(Reason::Rule)
        );
        assert!(matches!(
            p.vet(Source::Tool, "GET", "8.8.8.8", 443, false).await,
            Verdict::Addrs(_)
        ));
        // Deny even beats the configured-endpoint exception.
        let mut q = policy(false, &[], &["localhost"], &[]);
        q.allow_endpoint("localhost", 11434).unwrap();
        assert_eq!(
            reason(q.vet(Source::Tool, "GET", "localhost", 11434, false).await),
            Some(Reason::Rule)
        );
    }

    #[tokio::test]
    async fn default_deny_needs_a_listed_name_or_address() {
        let p = policy(true, &["localhost", "8.8.8.0/24"], &[], &["localhost"]);
        assert!(p.has_rules());
        assert_eq!(
            reason(p.vet(Source::Tool, "GET", "1.1.1.1", 443, false).await),
            Some(Reason::NotAllowed)
        );
        assert!(matches!(
            p.vet(Source::Tool, "GET", "8.8.8.8", 443, false).await,
            Verdict::Addrs(_)
        ));
        assert!(matches!(
            p.vet(Source::Tool, "GET", "localhost", 80, false).await,
            Verdict::Addrs(_)
        ));
        // A command reaching its own dev server doesn't need a rule.
        let q = policy(true, &[], &[], &[]);
        assert!(matches!(
            q.vet(Source::Command, "GET", "127.0.0.1", 5173, false)
                .await,
            Verdict::Addrs(_)
        ));
        // An IP literal isn't allowed by a host rule for some other name.
        let r = policy(true, &["dns.google"], &[], &[]);
        assert_eq!(
            reason(r.vet(Source::Tool, "GET", "8.8.8.8", 443, false).await),
            Some(Reason::NotAllowed)
        );
    }

    #[tokio::test]
    async fn unresolvable_names_go_upstream_or_fail_plainly() {
        let p = EgressPolicy::standard();
        let host = "no-such-host.invalid";
        assert_eq!(
            p.vet(Source::Tool, "GET", host, 443, true).await,
            Verdict::Upstream
        );
        assert!(matches!(
            p.vet(Source::Tool, "GET", host, 443, false).await,
            Verdict::Unresolved(_)
        ));
        let strict = policy(true, &[], &[], &[]);
        assert_eq!(
            reason(strict.vet(Source::Tool, "GET", host, 443, true).await),
            Some(Reason::NotAllowed)
        );
        // Behind an upstream, a resolvable private name is still refused.
        assert_eq!(
            reason(p.vet(Source::Tool, "GET", "localhost", 80, true).await),
            Some(Reason::Private)
        );
    }

    #[test]
    fn denial_messages_say_what_to_do() {
        let d = Denial {
            source: Source::Tool,
            host: "169.254.169.254".into(),
            port: 80,
            method: "GET".into(),
            reason: Reason::Private,
            detail: "169.254.169.254 (cloud metadata)".into(),
        };
        let m = d.message("http");
        assert!(m.contains("blocked http://169.254.169.254:80/"), "{m}");
        assert!(m.contains("cloud metadata"));
        assert!(m.contains("private_allow"));
        assert!(m.contains("retrying won't help"));
    }
}
