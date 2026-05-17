pub mod ca;
pub mod cli;
pub mod filter;
pub mod hook;
pub mod proxy;

/// Decode a standard base64 string to bytes.
/// Used by integration tests to decode the CA cert from the `CA_CERT_PEM_B64` readiness line.
pub fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_valid_base64() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn returns_error_on_invalid_base64() {
        assert!(base64_decode("not valid base64!!!").is_err());
    }
}
