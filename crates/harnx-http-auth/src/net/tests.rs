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
    let public = HashSet::from([AddressClass::Public]);
    let local = HashSet::from([AddressClass::Public, AddressClass::Loopback]);
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
    let resolver = GuardedResolver::new(HashSet::from([AddressClass::Public]));
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
