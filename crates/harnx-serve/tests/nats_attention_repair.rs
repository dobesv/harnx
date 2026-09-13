//! Integration test for server-read attention repair path.
//!
//! Tests that `load_nats_session_with_base` reconciles attention from log.

use anyhow::Result;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
    nats_worker::new_remote_session_id,
};

mod nats_test {
    use super::*;

    /// Test that load_nats_session_with_base repairs attention from log.
    /// Seeds a session log containing a TurnEnd without an attention bump in KV,
    /// then calls load_nats_session_with_base and asserts that last_attention_seq is repaired.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_nats_session_with_base_reconciles_attention() -> Result<()> {
        require_nextest();
        // Use test_support's ensure_test_nats for proper setup
        if !harnx_serve::test_support::ensure_test_nats().await {
            return Ok(());
        }

        // Get the local NATS URL from the environment/config
        let config = harnx_runtime::config::Config::default();
        let jetstream = config
            .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await?;
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let session_id = new_remote_session_id();

        // Create session metadata
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await?;

        // Append a TurnEnd without bumping attention (simulate lost bump)
        let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
        log.append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("test prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
        log.append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;

        // Verify not unread before load
        let state = store.get_read_state(&session_id).await?;
        assert!(
            !state.is_unread(),
            "should not be unread before load_nats_session_with_base"
        );

        // Call load_nats_session_with_base from harnx-serve - this should reconcile attention
        let (_session, _entries, _base) =
            harnx_serve::load_nats_session_with_base_for_test(&config, &session_id).await?;

        // Verify session is now unread (reconciled via server read path)
        let state = store.get_read_state(&session_id).await?;
        assert!(
            state.is_unread(),
            "session should be unread after load_nats_session_with_base repaired attention"
        );
        assert!(state.last_attention_seq >= 2);

        Ok(())
    }
}
