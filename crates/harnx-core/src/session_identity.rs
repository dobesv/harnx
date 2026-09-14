//! User-visible session IDs are local to an agent. All shared storage and
//! execution protocols use the derived key, never the local ID alone.

/// A bounded, case-sensitive identity suitable for NATS subjects, KV keys,
/// execution references, and filesystem-backed stream names.
///
/// `None` is the inline-agent namespace. It is distinct from every named
/// agent, including a named agent whose name happens to be "inline".
pub fn session_key(agent: Option<&str>, session_id: &str) -> String {
    // JSON framing keeps components unambiguous even when they contain
    // separators, Unicode, or other NATS-significant characters.
    let identity =
        serde_json::to_string(&(agent, session_id)).expect("serializing strings cannot fail");
    crate::crypto::sha256(&identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_and_exact_local_id_define_the_storage_identity() {
        let key = session_key(Some("pantheon/athena"), "review-12345");
        assert_eq!(key, session_key(Some("pantheon/athena"), "review-12345"));
        for (agent, id) in [
            (Some("pantheon/aeacus"), "review-12345"),
            (Some("pantheon/Athena"), "review-12345"),
            (Some("pantheon/athena"), "Review-12345"),
            (None, "review-12345"),
        ] {
            assert_ne!(key, session_key(agent, id));
        }
        assert_ne!(session_key(None, "id"), session_key(Some("inline"), "id"));
        assert_ne!(session_key(Some("a/b"), "c"), session_key(Some("a"), "b/c"));
        assert_eq!(key.len(), 64);
        assert!(key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }
}
