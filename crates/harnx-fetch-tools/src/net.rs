use std::error::Error;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ipnet::{Ipv4Net, Ipv6Net};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use reqwest::Url;

const DNS_TIMEOUT: Duration = Duration::from_secs(5);

#[doc(hidden)]
pub type BoxError = Box<dyn Error + Send + Sync>;
#[doc(hidden)]
pub type LookupFuture = Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, BoxError>> + Send>>;

/// DNS backend used below the SSRF-checking resolver.
///
/// Public for security integration tests. Production always uses `SystemLookup`.
#[doc(hidden)]
pub trait Lookup: Send + Sync {
    fn lookup(&self, host: String) -> LookupFuture;
}

#[derive(Debug)]
pub struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: String) -> LookupFuture {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|address| address.ip())
                .collect();
            Ok(addresses)
        })
    }
}

#[derive(Clone)]
pub struct GuardedResolver {
    lookup: Arc<dyn Lookup>,
    allow_private_ip: bool,
}

impl GuardedResolver {
    pub fn new(allow_private_ip: bool) -> Self {
        Self::with_lookup(allow_private_ip, Arc::new(SystemLookup))
    }

    #[doc(hidden)]
    pub fn with_lookup(allow_private_ip: bool, lookup: Arc<dyn Lookup>) -> Self {
        Self {
            lookup,
            allow_private_ip,
        }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let lookup = Arc::clone(&self.lookup);
        let allow_private_ip = self.allow_private_ip;
        Box::pin(async move {
            let addresses = tokio::time::timeout(DNS_TIMEOUT, lookup.lookup(host.clone()))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS lookup timed out"))??;
            if addresses.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("DNS lookup for {host} returned no addresses"),
                )
                .into());
            }
            if !allow_private_ip {
                if let Some(blocked) = addresses.iter().find(|ip| !is_allowed_ip(**ip)) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("DNS lookup for {host} returned blocked address {blocked}"),
                    )
                    .into());
                }
            }
            let addrs: Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addrs)
        })
    }
}

pub fn check_url(url: &Url, allow_private_ip: bool) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "URL scheme '{}' is not allowed; use http or https",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are not allowed".to_owned());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL must include a host".to_owned())?;
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>();
    if let Ok(ip) = literal {
        if !allow_private_ip && !is_allowed_ip(ip) {
            return Err(format!("URL address {ip} is blocked by private-IP policy"));
        }
    }
    Ok(())
}

pub fn redirect_policy(allow_private_ip: bool) -> Policy {
    let limited = Policy::limited(10);
    Policy::custom(move |attempt| {
        if let Err(error) = check_url(attempt.url(), allow_private_ip) {
            return attempt.error(io::Error::new(io::ErrorKind::PermissionDenied, error));
        }
        limited.redirect(attempt)
    })
}

fn ipv4_blocked_ranges() -> &'static [Ipv4Net] {
    static RANGES: std::sync::OnceLock<Vec<Ipv4Net>> = std::sync::OnceLock::new();
    RANGES.get_or_init(|| {
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
        ]
        .into_iter()
        .map(|range| range.parse().expect("valid built-in IPv4 network"))
        .collect()
    })
}

fn ipv6_blocked_ranges() -> &'static [Ipv6Net] {
    static RANGES: std::sync::OnceLock<Vec<Ipv6Net>> = std::sync::OnceLock::new();
    RANGES.get_or_init(|| {
        [
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
        .map(|range| range.parse().expect("valid built-in IPv6 network"))
        .collect()
    })
}

pub fn is_allowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_allowed_ipv4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => is_allowed_ipv4(mapped),
            None => !ipv6_blocked_ranges()
                .iter()
                .any(|range| range.contains(&ip)),
        },
    }
}

fn is_allowed_ipv4(ip: Ipv4Addr) -> bool {
    !ipv4_blocked_ranges()
        .iter()
        .any(|range| range.contains(&ip))
        && ip != Ipv4Addr::BROADCAST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_all_special_ranges_and_mapped_forms() {
        for ip in [
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.31.1.1",
            "192.0.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "64:ff9b::0808:0808",
            "2001::1",
            "2002:0808:0808::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_allowed_ip(ip), "{ip} must be blocked");
        }
        assert!(is_allowed_ip("8.8.8.8".parse().unwrap()));
        assert!(is_allowed_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn rejects_schemes_credentials_and_literal_spellings() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/file",
            "http://user:pass@example.com/",
            "http://127.1/",
            "http://2130706433/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert!(check_url(&parsed, false).is_err(), "{url} must be rejected");
        }
        assert!(check_url(&Url::parse("https://example.com").unwrap(), false).is_ok());
        assert!(check_url(&Url::parse("http://127.0.0.1").unwrap(), true).is_ok());
    }
}
