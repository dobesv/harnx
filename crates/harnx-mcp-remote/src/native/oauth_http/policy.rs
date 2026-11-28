//! Address allowances for the configured native MCP upstream.

use std::{collections::HashSet, net::IpAddr};
use url::Url;

pub(crate) use harnx_http_auth::net::{is_cloud_metadata_host, literal_ip, AddressClass};
use harnx_http_auth::net::{resolve_addresses, verify_resolved_addrs};

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
