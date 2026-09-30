//! Integration tests for SSE read-invalidation seq-gate bypass.
//!
//! Tests that a continuously-connected SSE subscriber sees mark-read THEN mark-unread
//! reflected through the SSE event stream, proving the after_seq gate bypass for read-state.

mod nats_test {
    use anyhow::Result;
    use futures_util::StreamExt;
    use harnx_core::require_nextest;
    use harnx_runtime::nats_event_sink::SessionEventStream;
    use harnx_runtime::nats_session_metadata::{
        read_invalidation_subject, SessionInitializer, SessionMetadata, SessionMetadataStore,
    };
    use harnx_serve::test_support::TestConfigSandbox;
    use std::time::Duration;

    fn new_remote_session_id() -> String {
        format!("sse-session-{}", uuid::Uuid::new_v4())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sse_read_unread_cycle() -> Result<()> {
        require_nextest();
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("test-agent", "You are a test agent.");
        let config = sandbox.config();
        if !harnx_serve::test_support::ensure_test_nats().await {
            return Ok(());
        }

        let client = config
            .nats_client(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

        let session_id = new_remote_session_id();

        // Create session
        let metadata = SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        );
        let storage_key = metadata.storage_key();
        store.create(&metadata).await?;

        // Bump attention to seq 10
        store.bump_attention(&storage_key, 10).await?;

        // First, establish the subscription BEFORE any mark operations
        // Subscribe to read-invalidation for this session
        let read_invalidation_sub = client
            .subscribe(read_invalidation_subject(&storage_key))
            .await
            .map_err(|e| anyhow::anyhow!("failed to subscribe to read-invalidation: {e}"))?;

        // Attach event stream using the production method
        let event_stream =
            SessionEventStream::attach(jetstream.clone(), client.clone(), &storage_key).await?;

        // Use the production SSE stream function
        let mut sse_stream =
            std::pin::pin!(harnx_serve::session_routes::session_updates_with_read(
                event_stream,
                read_invalidation_sub,
            ));

        // Let NATS subscription propagate (async subscription needs time to register)
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Mark read first
        store.mark_read(&storage_key).await?;

        // Should receive read-updated event
        let event = tokio::time::timeout(Duration::from_secs(15), sse_stream.next())
            .await?
            .expect("should receive first read-updated event");

        let event_str = String::from_utf8_lossy(&event);
        assert!(
            event_str.contains("event: read-updated"),
            "expected first read-updated event, got: {event_str}"
        );

        // Mark unread
        store.mark_unread(&storage_key).await?;

        // Should receive second read-updated event
        let event = tokio::time::timeout(Duration::from_secs(15), sse_stream.next())
            .await?
            .expect("should receive second read-updated event");

        let event_str = String::from_utf8_lossy(&event);
        assert!(
            event_str.contains("event: read-updated"),
            "expected second read-updated event, got: {event_str}"
        );

        // Verify final state
        let state = store.get_read_state(&storage_key).await?;
        assert!(state.manual_unread, "session should have manual_unread set");
        assert!(state.is_unread(), "session should be unread overall");

        Ok(())
    }
}
