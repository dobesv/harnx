pub mod ca;
pub mod cli;
pub mod filter;
pub mod hook;
pub mod proxy;

/// Decode a standard base64 string to bytes.
/// Used by integration tests to decode the CA cert from the `CA_CERT_PEM_B64` readiness line.
pub fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .unwrap_or_default()
}
