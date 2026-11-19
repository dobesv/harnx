use std::net::{Ipv4Addr, Ipv6Addr};

pub(super) fn parse_fetch_hosts(hosts: Vec<String>) -> Result<Vec<String>, String> {
    hosts.into_iter().map(parse_host).collect()
}

fn parse_host(host: String) -> Result<String, String> {
    if let Some(ip) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return ip
            .parse::<Ipv6Addr>()
            .map(|ip| format!("[{ip}]"))
            .map_err(|_| format!("invalid fetch host '{host}': expected bracketed IPv6"));
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip.to_string());
    }
    if valid_dns_name(&host) {
        return Ok(host.to_ascii_lowercase());
    }
    Err(format!(
        "invalid fetch host '{host}': expected an exact ASCII hostname or IP, without scheme, port, path or wildcard"
    ))
}

const MAX_DNS_NAME_BYTES: usize = 253;
const MAX_DNS_LABEL_BYTES: usize = 63;

fn valid_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_DNS_NAME_BYTES
        && host.bytes().any(|b| b.is_ascii_alphabetic())
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= MAX_DNS_LABEL_BYTES
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middleware_fetch_host_syntax() {
        for (host, expected) in [
            ("Example.COM", "example.com"),
            ("localhost", "localhost"),
            ("127.0.0.1", "127.0.0.1"),
            ("[0:0:0:0:0:0:0:1]", "[::1]"),
        ] {
            assert_eq!(parse_host(host.into()).unwrap(), expected);
        }
        for host in [
            "",
            "*",
            "*.example.com",
            "https://example.com",
            "example.com:443",
            "example.com/a",
            "example.com.",
            "x@y.com",
            " a.com",
            "::1",
            "2130706433",
            "-a.com",
            "a_.com",
            "é.com",
        ] {
            assert!(parse_host(host.into()).is_err(), "{host}");
        }
    }
}
