//! Session ID generation and reservation.
//!
//! Harnx session IDs have two formats:
//! - **Canonical short IDs**: 6-character base64url strings encoding a Unix-seconds
//!   timestamp (e.g., `"A1b2C3"`). Created via [`reserve_short_session_id`] for
//!   collision safety against the canonical NATS metadata store.
//! - **Legacy UUID v7**: 36-character UUIDs (e.g., `"01948a3f-7b1c-7123-8901-abcdef123456"`).
//!   Readers must tolerate both; see `config::session_meta::session_recency_key` for
//!   dual-format decoding.
//!
//! New sessions must reserve a short ID through the canonical metadata store.
//! Do **not** use `nats_worker::new_remote_session_id()` — it returns a raw UUID v7
//! and is test-only. All production paths route through `Config::reserve_new_session_id`,
//! `NatsSession::new` (when `config.session_id` is None), or the HTTP POST endpoint.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn git_branch() -> String {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn git_remote() -> Option<String> {
    Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Encode a Unix timestamp (seconds) as a 6-char base64url session ID.
pub fn encode_timestamp_session_id(seconds: u64) -> String {
    URL_SAFE_NO_PAD.encode((seconds as u32).to_be_bytes())
}

/// Decode a 6-char base64url session ID back to Unix seconds. Returns None if not a valid short ID.
pub fn decode_timestamp_session_id(id: &str) -> Option<u64> {
    if id.len() != 6 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(id).ok()?;
    let bytes: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(bytes) as u64)
}

/// Atomically reserve a collision-safe short session ID in canonical metadata.
pub async fn reserve_short_session_id(
    store: &crate::nats_session_metadata::SessionMetadataStore,
    initializer: &crate::nats_session_metadata::SessionInitializer,
) -> anyhow::Result<String> {
    let mut seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    loop {
        let candidate = encode_timestamp_session_id(seconds);
        let metadata =
            crate::nats_session_metadata::SessionMetadata::new(&candidate, initializer.clone());
        match store.create(&metadata).await? {
            Some(_) => return Ok(candidate),
            None => seconds = seconds.saturating_add(1),
        }
    }
}

/// Generate a unique session ID starting from current time, retrying +1 second until exists(candidate) is false.
pub fn generate_session_id(exists: impl Fn(&str) -> bool) -> String {
    let mut seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    loop {
        let candidate = encode_timestamp_session_id(seconds);
        if !exists(&candidate) {
            return candidate;
        }
        seconds = seconds.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_session_id_roundtrip() {
        let seconds = 1_735_689_600_u64;
        let id = encode_timestamp_session_id(seconds);
        assert_eq!(id.len(), 6);
        assert_eq!(decode_timestamp_session_id(&id), Some(seconds));
    }

    #[test]
    fn timestamp_session_id_decode_rejects_invalid_inputs() {
        assert_eq!(decode_timestamp_session_id("short"), None);
        assert_eq!(decode_timestamp_session_id("toolong7"), None);
        assert_eq!(decode_timestamp_session_id("!!!!!!"), None);
    }

    #[test]
    fn generate_session_id_retries_on_collision() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let first = encode_timestamp_session_id(now);
        let second = encode_timestamp_session_id(now + 1);
        let generated = generate_session_id(|candidate| candidate == first);

        assert_eq!(generated, second);
        assert!(generated
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
