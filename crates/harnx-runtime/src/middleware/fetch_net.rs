//! Explicit host grants allow internal services, but never known cloud metadata addresses.
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

pub(super) struct Resolver;

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Return the checked answer itself, avoiding a second DNS lookup.
            let addresses = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let addresses = checked_addresses(addresses)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn checked_addresses(addresses: Vec<SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    addresses.first().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "fetch DNS returned no addresses")
    })?;
    addresses
        .iter()
        .try_for_each(|address| check_ip(address.ip()))?;
    Ok(addresses)
}

pub(super) fn check_ip(ip: IpAddr) -> io::Result<()> {
    match forbidden(ip) {
        true => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fetch address blocked: metadata, link-local, unspecified or multicast address",
        )),
        false => Ok(()),
    }
}

fn forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_v4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(forbidden_v4)
            .unwrap_or_else(|| forbidden_v6(ip)),
    }
}

const ALIBABA_METADATA: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 200);
const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

fn forbidden_v4(ip: Ipv4Addr) -> bool {
    ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip == ALIBABA_METADATA
}

fn forbidden_v6(ip: Ipv6Addr) -> bool {
    ip.is_unicast_link_local() || ip.is_unspecified() || ip.is_multicast() || ip == AWS_METADATA_V6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middleware_fetch_metadata_and_mixed_dns_answers_are_blocked() {
        for ip in [
            "169.254.169.254",
            "100.100.100.200",
            "::ffff:169.254.169.254",
            "fd00:ec2::254",
            "fe80::1",
            "::",
            "0.0.0.0",
            "224.0.0.1",
            "ff02::1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(check_ip(ip).is_err(), "{ip}");
            assert!(checked_addresses(vec![
                "127.0.0.1:80".parse().unwrap(),
                SocketAddr::new(ip, 80)
            ])
            .is_err());
        }
        assert!(checked_addresses(vec![]).is_err());
        for ip in ["127.0.0.1", "::1", "10.0.0.1", "fd00::1", "8.8.8.8"] {
            assert!(check_ip(ip.parse().unwrap()).is_ok(), "{ip}");
        }
    }
}
