//! Integration test for session/mark_read and session/mark_unread RPC methods.
//!
//! Tests round-trip: mark-read toggles state, publishes invalidations, and is reflected in the session list.

use anyhow::Result;
use bytes::Bytes;
use harnx_core::require_nextest;
use harnx_runtime::{
    config::GlobalConfig,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
    nats_worker::new_remote_session_id,
};
use harnx_serve::{
    ag_ui_rpc::{handle_ag_ui_rpc_bytes, PersistenceKind},
    session_actor::SessionRegistry,
    test_support::TestConfigSandbox,
    Server,
};
use http::Method;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;

mod nats_test {
    use super::*;

    /// Test that session/mark_read and session/mark_unread toggle unread state
    /// and are reflected in the session list JSON.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mark_read_and_mark_unread_toggle_state() -> Result<()> {
        require_nextest();
        let sandbox = TestConfigSandbox::new();
        let agent = "plain";
        sandbox.write_agent(agent, "You are plain.");
        let config = sandbox.config();
        if !harnx_serve::test_support::ensure_test_nats().await {
            return Ok(());
        }

        let session_id = new_remote_session_id();

        // Get NATS handles
        let jetstream = config
            .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await?;
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

        // Create session metadata
        let metadata = SessionMetadata::new(
            &session_id,
            SessionInitializer::named(agent, Default::default()),
        );
        let storage_key = metadata.storage_key();
        store.create(&metadata).await?;

        // Create session registry (no active sessions needed for mark-read/unread)
        let registry = SessionRegistry::new(config.clone());

        // Create Server instance to verify session list JSON
        let global_config: GlobalConfig = Arc::new(parking_lot::RwLock::new(config.clone()));
        let server = Server::new(&global_config, std::path::PathBuf::from("web-assets"));

        // Initial state: not unread (no attention)
        let state = store.get_read_state(&storage_key).await?;
        assert!(!state.is_unread(), "initially not unread");

        // Toggle: unread -> read -> unread -> read and verify each state
        let ctx = ToggleCtx {
            config: &config,
            registry: &registry,
            store: &store,
            server: &server,
            agent,
            session_id: &session_id,
            storage_key: &storage_key,
        };
        ctx.toggle("session/mark_unread", true).await?;
        ctx.toggle("session/mark_read", false).await?;
        ctx.toggle("session/mark_unread", true).await?;
        ctx.toggle("session/mark_read", false).await?;

        // Assert that session/mark_read returns NOT_FOUND for non-existent session.
        assert_mark_not_found_for_nonexistent_session(&config, agent).await;

        Ok(())
    }

    struct ToggleCtx<'a> {
        config: &'a harnx_runtime::config::Config,
        registry: &'a SessionRegistry,
        store: &'a SessionMetadataStore,
        server: &'a Server,
        agent: &'a str,
        session_id: &'a str,
        storage_key: &'a str,
    }

    impl ToggleCtx<'_> {
        async fn rpc_call(&self, method: &str) -> Value {
            let response = handle_ag_ui_rpc_bytes(
                Method::POST,
                self.agent,
                self.session_id,
                Bytes::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": method,
                        "params": {}
                    })
                    .to_string(),
                ),
                self.config,
                self.registry,
                PersistenceKind::Nats,
            )
            .await
            .expect("rpc response");
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("collect rpc body")
                .to_bytes();
            serde_json::from_slice::<Value>(&bytes).expect("rpc json")
        }

        async fn toggle(&self, method: &str, expected_unread: bool) -> Result<()> {
            let resp = self.rpc_call(method).await;
            assert_eq!(resp["result"]["status"], "ok", "{} should succeed", method);

            let state = self.store.get_read_state(self.storage_key).await?;
            assert_eq!(
                state.is_unread(),
                expected_unread,
                "is_unread after {}",
                method
            );
            assert_eq!(
                state.manual_unread, expected_unread,
                "manual_unread after {}",
                method
            );
            assert_eq!(
                get_unread_from_list_inner(self.server, self.agent, self.session_id).await,
                expected_unread,
                "session list after {}",
                method
            );
            Ok(())
        }
    }

    /// Get unread state from session list JSON.
    async fn get_unread_from_list_inner(server: &Server, agent: &str, session_id: &str) -> bool {
        let sessions = server
            .list_sessions_json(agent)
            .await
            .expect("sessions list");
        let sessions_arr = sessions.as_array().expect("sessions array");
        let session = sessions_arr
            .iter()
            .find(|s| s["session_id"] == session_id)
            .expect("session should be in list");
        session["unread"].as_bool().unwrap_or(false)
    }
}

/// Verify that mark_read/mark_unread return NOT_FOUND for non-existent sessions.
async fn assert_mark_not_found_for_nonexistent_session(
    config: &harnx_runtime::config::Config,
    agent: &str,
) {
    let session_id = "nonexistent-session-12345";
    let registry = SessionRegistry::new(config.clone());

    let response = handle_ag_ui_rpc_bytes(
        Method::POST,
        agent,
        session_id,
        Bytes::from(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/mark_read",
                "params": {}
            })
            .to_string(),
        ),
        config,
        &registry,
        PersistenceKind::Nats,
    )
    .await
    .expect("rpc response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect rpc body")
        .to_bytes();
    let resp: Value = serde_json::from_slice(&bytes).expect("rpc json");

    assert_eq!(
        resp["error"]["code"], -32001,
        "should return unknown session error"
    );
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
}
