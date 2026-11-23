//! Network address classification and DNS resolution guards for SSRF defense.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

use super::static_error;

async fn resolve_addresses(host: &str) -> Result<Vec<SocketAddr>, super::OAuthHttpClientError> {
    tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 0)))
        .await
        .map_err(|_| static_error("dns lookup timed out"))?
        .map(|addresses| addresses.collect())
        .map_err(|_| static_error("dns resolution failed"))
}

/// Address classification for SSRF protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AddressClass {
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
    pub(crate) fn classify(ip: IpAddr) -> Self {
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

pub(crate) fn is_cloud_metadata_host(host: &str) -> bool {
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

pub(crate) fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
        url::Host::Domain(_) => None,
    }
}

pub(crate) fn determine_allowed_classes(upstream_url: &Url) -> HashSet<AddressClass> {
    let mut allowed = HashSet::new();
    allowed.insert(AddressClass::Public);

    match upstream_url.host() {
        Some(url::Host::Domain(domain)) => {
            let lower = domain.to_ascii_lowercase();
            if lower == "localhost" || lower.ends_with(".localhost") {
                allowed.insert(AddressClass::Loopback);
            }
        }
        Some(url::Host::Ipv4(v4)) => {
            let class = AddressClass::classify(IpAddr::V4(v4));
            if class == AddressClass::Loopback || class == AddressClass::Private {
                allowed.insert(class);
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            let class = AddressClass::classify(IpAddr::V6(v6));
            if class == AddressClass::Loopback || class == AddressClass::Private {
                allowed.insert(class);
            }
        }
        None => {}
    }
    allowed
}

/// Custom DNS resolver enforcing SSRF address class checks and rejecting mixed DNS answers.
pub(crate) struct GuardedResolver {
    pub(crate) allowed_classes: HashSet<AddressClass>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allowed = self.allowed_classes.clone();
        let host_str = name.as_str().to_ascii_lowercase();

        Box::pin(async move {
            if is_cloud_metadata_host(&host_str) {
                return Err(static_error("dns resolution rejected: blocked host"));
            }

            let addrs = resolve_addresses(&host_str).await?;

            verify_resolved_addrs(&addrs, &allowed)
                .map_err(|_| static_error("dns resolution rejected: disallowed address"))?;

            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(crate) fn verify_resolved_addrs(
    addrs: &[SocketAddr],
    allowed: &HashSet<AddressClass>,
) -> Result<(), ()> {
    if addrs.is_empty() {
        return Err(());
    }
    let mut resolved_class: Option<AddressClass> = None;
    for addr in addrs {
        let class = AddressClass::classify(addr.ip());
        if class == AddressClass::Disallowed || !allowed.contains(&class) {
            return Err(());
        }
        match resolved_class {
            None => resolved_class = Some(class),
            Some(first) => {
                if first != class {
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn prevalidate_endpoint_dns(
    url: &Url,
    allowed: &HashSet<AddressClass>,
) -> anyhow::Result<()> {
    if literal_ip(url).is_none() {
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let addrs = resolve_addresses(&host)
            .await
            .map_err(|_| anyhow::anyhow!("authorization DNS lookup failed"))?;

        verify_resolved_addrs(&addrs, allowed)
            .map_err(|_| anyhow::anyhow!("dns resolution rejected for authorization endpoint"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn transition_and_special_purpose_addresses_are_not_public() {
        for ip in [
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "::192.168.1.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "100::1",
            "2001::1",
            "2002:a00:1::",
            "5f00::1",
            "fec0::1",
        ] {
            assert_eq!(
                AddressClass::classify(ip.parse().unwrap()),
                AddressClass::Disallowed
            );
        }
    }

    #[test]
    fn mixed_and_private_dns_answers_fail_closed() {
        let public =
            determine_allowed_classes(&Url::parse("https://upstream.example/mcp").unwrap());
        let local = determine_allowed_classes(&Url::parse("http://127.0.0.1/mcp").unwrap());
        let addresses = |values: &[&str]| -> Vec<SocketAddr> {
            values
                .iter()
                .map(|ip| SocketAddr::new(ip.parse().unwrap(), 443))
                .collect()
        };
        assert!(verify_resolved_addrs(&addresses(&["8.8.8.8"]), &public).is_ok());
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::ffff:10.0.0.1",
            "fe80::1",
        ] {
            assert!(verify_resolved_addrs(&addresses(&[ip]), &public).is_err());
        }
        assert!(verify_resolved_addrs(&[], &public).is_err());
        assert!(verify_resolved_addrs(&addresses(&["8.8.8.8", "127.0.0.1"]), &local).is_err());
        assert!(verify_resolved_addrs(&addresses(&["127.0.0.1", "::1"]), &local).is_ok());
    }

    #[tokio::test]
    async fn guarded_dns_rejects_localhost_for_public_upstream() {
        use reqwest::dns::Resolve;
        let resolver = GuardedResolver {
            allowed_classes: determine_allowed_classes(
                &Url::parse("https://upstream.example/mcp").unwrap(),
            ),
        };
        // localhost resolves without an external DNS or HTTP dependency.
        assert!(resolver
            .resolve("localhost".parse().unwrap())
            .await
            .is_err());
        assert!(resolver
            .resolve("metadata.google.internal".parse().unwrap())
            .await
            .is_err());
    }

    #[test]
    fn test_address_classification_ipv4() {
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))),
            AddressClass::Public
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            AddressClass::Loopback
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 2))),
            AddressClass::Private
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))),
            AddressClass::Private
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            AddressClass::Private
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))),
            AddressClass::Private
        );

        // Disallowed
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
            AddressClass::Disallowed
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))),
            AddressClass::Disallowed
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255))),
            AddressClass::Disallowed
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))),
            AddressClass::Disallowed
        );
    }

    #[test]
    fn test_address_classification_ipv6_and_mapped() {
        assert_eq!(
            AddressClass::classify(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddressClass::Loopback
        );
        assert_eq!(
            AddressClass::classify(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            AddressClass::Disallowed
        );
        assert_eq!(
            AddressClass::classify("fc00::1".parse().unwrap()),
            AddressClass::Private
        );
        assert_eq!(
            AddressClass::classify("fe80::1".parse().unwrap()),
            AddressClass::Disallowed
        );
        assert_eq!(
            AddressClass::classify("ff02::1".parse().unwrap()),
            AddressClass::Disallowed
        );

        // IPv4-mapped
        assert_eq!(
            AddressClass::classify("::ffff:127.0.0.1".parse().unwrap()),
            AddressClass::Loopback
        );
        assert_eq!(
            AddressClass::classify("::ffff:10.0.0.1".parse().unwrap()),
            AddressClass::Private
        );
        assert_eq!(
            AddressClass::classify("::ffff:169.254.169.254".parse().unwrap()),
            AddressClass::Disallowed
        );
    }
}
