//! Network address classification and DNS resolution guards for SSRF defense.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

/// A DNS diagnostic that never contains the queried hostname or address.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("{0}")]
pub struct DnsError(&'static str);

/// Whether the URL has an HTTP(S) scheme and a parsed host.
pub fn is_http_endpoint(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host().is_some()
}

/// Whether the URL carries nonempty user information or a password.
pub fn has_embedded_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

/// HTTPS or the typed loopback host accepted by client-credentials mode.
/// This is a plaintext transport check, not an address or DNS policy.
pub fn is_https_or_loopback(url: &Url) -> bool {
    url.scheme() == "https" || is_loopback_host_typed(url)
}

fn is_loopback_host_typed(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Resolve on the system resolver with a fixed timeout and static errors.
/// These addresses are not vetted. Check every answer before connecting.
pub async fn resolve_addresses(host: &str) -> Result<Vec<SocketAddr>, DnsError> {
    tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 0)))
        .await
        .map_err(|_| DnsError("dns lookup timed out"))?
        .map(|addresses| addresses.collect())
        .map_err(|_| DnsError("dns resolution failed"))
}

/// Coarse address classification, extracted from MCP's native OAuth policy.
/// `Private` is not a safety verdict: a consumer allowing private addresses
/// must also enforce its destination-specific exclusions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressClass {
    Public,
    Loopback,
    Private,
    Disallowed,
}

const CGNAT_FIRST_OCTET: u8 = 100;
const CGNAT_SECOND_OCTET_START: u8 = 64;
const CGNAT_SECOND_OCTET_END: u8 = 127;

fn is_cgnat_v4(v4: &std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == CGNAT_FIRST_OCTET
        && (CGNAT_SECOND_OCTET_START..=CGNAT_SECOND_OCTET_END).contains(&octets[1])
}

impl AddressClass {
    /// Classifies IPv4-mapped IPv6 as IPv4; blocks other transition ranges.
    pub fn classify(ip: IpAddr) -> Self {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            IpAddr::V4(_) => ip,
        };
        if ip.is_loopback() {
            return Self::Loopback;
        }
        let private = match ip {
            IpAddr::V4(v4) => v4.is_private() || is_cgnat_v4(&v4),
            IpAddr::V6(v6) => v6.is_unique_local(),
        };
        if private {
            return Self::Private;
        }
        if special_networks()
            .iter()
            .any(|network| network.contains(&ip))
        {
            Self::Disallowed
        } else {
            Self::Public
        }
    }
}

fn special_networks() -> &'static [ipnet::IpNet] {
    static NETWORKS: std::sync::OnceLock<Vec<ipnet::IpNet>> = std::sync::OnceLock::new();
    // Keep the special-purpose ranges aligned with harnx-fetch-tools/src/net.rs.
    // Explicit upstream private/loopback allowances are handled before this list.
    NETWORKS.get_or_init(|| {
        [
            "0.0.0.0/8",
            "10.0.0.0/8",
            "100.64.0.0/10",
            "127.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.0.0.0/24",
            "192.0.2.0/24",
            "192.31.196.0/24",
            "192.52.193.0/24",
            "192.88.99.0/24",
            "192.168.0.0/16",
            "192.175.48.0/24",
            "198.18.0.0/15",
            "198.51.100.0/24",
            "203.0.113.0/24",
            "224.0.0.0/4",
            "240.0.0.0/4",
            "::/96",
            "::/128",
            "::1/128",
            "64:ff9b::/96",
            "64:ff9b:1::/48",
            "100::/64",
            "2001::/23",
            "2001:db8::/32",
            "2002::/16",
            "5f00::/16",
            "fc00::/7",
            "fe80::/10",
            "fec0::/10",
            "ff00::/8",
        ]
        .into_iter()
        .map(|range| range.parse().expect("valid built-in network"))
        .collect()
    })
}

pub fn is_cloud_metadata_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(
        host.as_str(),
        "metadata"
            | "metadata.google.internal"
            | "metadata.azure.internal"
            | "169.254.169.254"
            | "instance-data"
    )
}

pub fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
        url::Host::Domain(_) => None,
    }
}

/// Custom DNS resolver enforcing SSRF address class checks and rejecting mixed DNS answers.
pub struct GuardedResolver {
    allowed_classes: HashSet<AddressClass>,
}

impl GuardedResolver {
    /// Use only for clients whose consumer separately validates URL literals
    /// and limits destinations. Used by MCP's native OAuth policy.
    pub fn new(allowed_classes: HashSet<AddressClass>) -> Self {
        Self { allowed_classes }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allowed = self.allowed_classes.clone();
        let host_str = name.as_str().to_ascii_lowercase();

        Box::pin(async move {
            if is_cloud_metadata_host(&host_str) {
                return Err(DnsError("dns resolution rejected: blocked host").into());
            }

            let addrs = resolve_addresses(&host_str).await?;

            verify_resolved_addrs(&addrs, &allowed)?;

            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Reject empty answers, disallowed classes, and mixed-class answers.
pub fn verify_resolved_addrs(
    addrs: &[SocketAddr],
    allowed: &HashSet<AddressClass>,
) -> Result<(), DnsError> {
    if addrs.is_empty() {
        return Err(DnsError("dns resolution rejected: disallowed address"));
    }
    let mut resolved_class: Option<AddressClass> = None;
    for addr in addrs {
        let class = AddressClass::classify(addr.ip());
        if class == AddressClass::Disallowed || !allowed.contains(&class) {
            return Err(DnsError("dns resolution rejected: disallowed address"));
        }
        match resolved_class {
            None => resolved_class = Some(class),
            Some(first) => {
                if first != class {
                    return Err(DnsError("dns resolution rejected: disallowed address"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
