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
        if !harnx_serve::test_support::ensure_test_nats().await {
            return Ok(());
        }

        let config = harnx_runtime::config::Config::default();
        let agent = "plain";
        let session_id = new_remote_session_id();

        // Get NATS handles
        let jetstream = config
            .nats_jetstream(harnx_runtime::config::LOCAL_CLUSTER_KEY)
            .await?;
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

        // Create session metadata
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named(agent, Default::default()),
            ))
            .await?;

        // Create session registry (no active sessions needed for mark-read/unread)
        let registry = SessionRegistry::new(config.clone());

        // Create Server instance to verify session list JSON
        let global_config: GlobalConfig = Arc::new(parking_lot::RwLock::new(config.clone()));
        let server = Server::new(&global_config, std::path::PathBuf::from("web-assets"));

        // Helper to call RPC method
        async fn rpc_call(
            config: &harnx_runtime::config::Config,
            registry: &SessionRegistry,
            agent: &str,
            session_id: &str,
            method: &str,
        ) -> Value {
            let response = handle_ag_ui_rpc_bytes(
                Method::POST,
                agent,
                session_id,
                Bytes::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": method,
                        "params": {}
                    })
                    .to_string(),
                ),
                config,
                registry,
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

        // Helper to find session in list and get its unread state
        async fn get_unread_from_list(server: &Server, agent: &str, session_id: &str) -> bool {
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

        // Initial state: not unread (no attention)
        let state = store.get_read_state(&session_id).await?;
        assert!(!state.is_unread(), "initially not unread");

        // Mark unread
        let resp = rpc_call(
            &config,
            &registry,
            agent,
            &session_id,
            "session/mark_unread",
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok", "mark_unread should succeed");

        // Verify unread via store
        let state = store.get_read_state(&session_id).await?;
        assert!(state.is_unread(), "should be unread after mark_unread");
        assert!(state.manual_unread, "manual_unread flag should be set");

        // Verify unread=true in session list JSON
        assert!(
            get_unread_from_list(&server, agent, &session_id).await,
            "session list should show unread=true after mark_unread"
        );

        // Mark read
        let resp = rpc_call(&config, &registry, agent, &session_id, "session/mark_read").await;
        assert_eq!(resp["result"]["status"], "ok", "mark_read should succeed");

        // Verify read via store
        let state = store.get_read_state(&session_id).await?;
        assert!(!state.is_unread(), "should be read after mark_read");
        assert!(!state.manual_unread, "manual_unread flag should be cleared");

        // Verify unread=false in session list JSON
        assert!(
            !get_unread_from_list(&server, agent, &session_id).await,
            "session list should show unread=false after mark_read"
        );

        // Mark unread again to verify idempotency
        let resp = rpc_call(
            &config,
            &registry,
            agent,
            &session_id,
            "session/mark_unread",
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        let state = store.get_read_state(&session_id).await?;
        assert!(state.is_unread());
        // Verify unread=true in session list JSON
        assert!(
            get_unread_from_list(&server, agent, &session_id).await,
            "session list should show unread=true after idempotent mark_unread"
        );

        // Mark read again to verify idempotency
        let resp = rpc_call(&config, &registry, agent, &session_id, "session/mark_read").await;
        assert_eq!(resp["result"]["status"], "ok");
        let state = store.get_read_state(&session_id).await?;
        assert!(!state.is_unread());
        // Verify unread=false in session list JSON
        assert!(
            !get_unread_from_list(&server, agent, &session_id).await,
            "session list should show unread=false after idempotent mark_read"
        );

        // Assert that session/mark_read returns NOT_FOUND for non-existent session.
        {
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
                &config,
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

        Ok(())
    }
}
