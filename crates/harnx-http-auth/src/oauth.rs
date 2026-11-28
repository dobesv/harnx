//! OAuth constants and file-only client secret loading shared with MCP.

use anyhow::{anyhow, Result};

/// Maximum allowed HTTP response body size (1 MiB).
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

/// Standard OAuth 2.0 error codes (RFC 6749, RFC 7591, RFC 8414).
pub const STANDARD_OAUTH_ERRORS: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
    "unsupported_response_type",
    "server_error",
    "temporarily_unavailable",
    "invalid_client_metadata",
    "invalid_redirect_uri",
];

/// Read the client secret from one file, preserving native semantics: only
/// trailing CR/LF is stripped, interior whitespace and CR/LF are kept, and
/// an empty result is rejected.
pub fn read_client_secret_file(path: &std::path::Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| anyhow!("cannot read --oauth-client-secret-file"))?;
    let text = text.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(!text.is_empty(), "--oauth-client-secret-file is empty");
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_file_reading_strips_only_trailing_crlf() {
        let file = tempfile::NamedTempFile::new().unwrap();
        for (content, expected) in [
            ("abc\r\n", Some("abc")),
            ("abc\r\n\r\n", Some("abc")),
            ("a\r\nb", Some("a\r\nb")),
            ("  spaced  ", Some("  spaced  ")),
            ("\r\n", None),
        ] {
            std::fs::write(file.path(), content).unwrap();
            let read = read_client_secret_file(file.path());
            match expected {
                Some(secret) => assert_eq!(read.unwrap(), secret, "{content:?}"),
                None => assert!(read.is_err(), "{content:?}"),
            }
        }
    }
}
