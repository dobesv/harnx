//! Session index metadata for remote NATS session enumeration.
//!
//! Session Header is canonical source of this metadata. This index stores a
//! denormalized copy for enumeration only.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

const DEFAULT_BUCKET_REPLICAS: usize = 1;

pub const SESSION_INDEX_BUCKET: &str = "harnx_sessions";

/// Denormalized session metadata copied from Session Header for enumeration.
/// Session Header remains canonical source of truth.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionIndexRecord {
    pub session_id: String,
    pub agent_name: String,
    pub working_dir: Option<String>,
    pub git_branch: Option<String>,
    pub git_remote: Option<String>,
    pub last_activity: u64,
}

pub fn session_index_key(session_id: &str) -> String {
    format!("sessions/{session_id}/meta")
}

pub async fn ensure_index_bucket(jetstream: &jetstream::Context) -> Result<kv::Store> {
    match jetstream
        .create_key_value(kv::Config {
            bucket: SESSION_INDEX_BUCKET.to_string(),
            history: 1,
            num_replicas: DEFAULT_BUCKET_REPLICAS,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(bucket) => Ok(bucket),
        Err(_) => jetstream
            .get_key_value(SESSION_INDEX_BUCKET)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!("Failed to open session index bucket '{SESSION_INDEX_BUCKET}'")
            }),
    }
}

pub async fn put_record(store: &kv::Store, record: &SessionIndexRecord) -> Result<u64> {
    let key = session_index_key(&record.session_id);
    let payload = serde_json::to_vec(record).with_context(|| {
        format!(
            "Failed to serialize session index record '{}'",
            record.session_id
        )
    })?;
    store
        .put(&key, payload.into())
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("Failed to put session index record for key '{key}'"))
}

pub async fn get_record(store: &kv::Store, session_id: &str) -> Result<Option<SessionIndexRecord>> {
    Ok(get_record_with_revision(store, session_id)
        .await?
        .map(|(record, _revision)| record))
}

pub async fn get_record_with_revision(
    store: &kv::Store,
    session_id: &str,
) -> Result<Option<(SessionIndexRecord, u64)>> {
    let key = session_index_key(session_id);
    match store.entry(key.clone()).await {
        Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
            let record: SessionIndexRecord =
                serde_json::from_slice(&entry.value).with_context(|| {
                    format!("Failed to deserialize session index record for key '{key}'")
                })?;
            Ok(Some((record, entry.revision)))
        }
        Ok(Some(_)) | Ok(None) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error))
            .with_context(|| format!("Failed to read session index record for key '{key}'")),
    }
}

pub async fn list_records(store: &kv::Store) -> Result<Vec<SessionIndexRecord>> {
    let mut keys = store
        .keys()
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!("Failed to list keys in session index bucket '{SESSION_INDEX_BUCKET}'")
        })?;
    let mut records = Vec::new();

    while let Some(key_result) = keys.next().await {
        let key = key_result.map_err(anyhow::Error::from).with_context(|| {
            format!("Failed to enumerate key in session index bucket '{SESSION_INDEX_BUCKET}'")
        })?;

        match store.entry(key.clone()).await {
            Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
                match serde_json::from_slice::<SessionIndexRecord>(&entry.value) {
                    Ok(record) => records.push(record),
                    Err(error) => warn!(
                        "skipping invalid session index record: bucket={} key={} error={error:#}",
                        SESSION_INDEX_BUCKET, key
                    ),
                }
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => {
                return Err(anyhow::Error::from(error)).with_context(|| {
                    format!("Failed to read session index record for key '{key}'")
                });
            }
        }
    }

    Ok(records)
}

pub async fn delete_record(store: &kv::Store, session_id: &str) -> Result<()> {
    let key = session_index_key(session_id);
    store
        .delete(&key)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("Failed to delete session index record for key '{key}'"))
}

pub async fn update_record_with_revision(
    store: &kv::Store,
    record: &SessionIndexRecord,
    revision: u64,
) -> Result<u64> {
    let key = session_index_key(&record.session_id);
    let payload = serde_json::to_vec(record).with_context(|| {
        format!(
            "Failed to serialize session index record '{}'",
            record.session_id
        )
    })?;
    store
        .update(&key, payload.into(), revision)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "Failed to CAS-update session index record for key '{key}' at revision {revision}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{
        get_record, get_record_with_revision, list_records, put_record, session_index_key,
        update_record_with_revision, SessionIndexRecord, SESSION_INDEX_BUCKET,
    };
    use async_nats::jetstream::kv::Store;

    // =============================================================================
    // Test helpers for NATS integration tests
    // =============================================================================

    /// Get NATS test URL from environment, returning None if not set.
    /// Tests should skip gracefully when this returns None.
    fn test_nats_url() -> Option<String> {
        std::env::var("HARNX_NATS_TEST_URL").ok()
    }

    /// Connect to JetStream using the test NATS URL.
    /// Panics if connection fails.
    async fn connect_jetstream(server_url: &str) -> async_nats::jetstream::Context {
        let client = async_nats::connect(server_url)
            .await
            .expect("connect to test nats");
        async_nats::jetstream::new(client)
    }

    /// Ensure the session index bucket exists for testing.
    async fn ensure_test_bucket(jetstream: &async_nats::jetstream::Context) -> Store {
        super::ensure_index_bucket(jetstream)
            .await
            .expect("ensure bucket")
    }

    fn sample_record(session_id: &str) -> SessionIndexRecord {
        SessionIndexRecord {
            session_id: session_id.to_string(),
            agent_name: "hephaestus".to_string(),
            working_dir: Some("/tmp/project".to_string()),
            git_branch: Some("main".to_string()),
            git_remote: Some("git@github.com:dobesv/harnx.git".to_string()),
            last_activity: 1_719_531_234,
        }
    }

    #[test]
    fn session_index_record_json_round_trip() {
        let record = sample_record("session-123");

        let json = serde_json::to_string(&record).expect("serialize record");
        let decoded: SessionIndexRecord = serde_json::from_str(&json).expect("deserialize record");

        assert_eq!(decoded, record);
    }

    #[test]
    fn session_index_key_format_matches_bucket_layout() {
        assert_eq!(SESSION_INDEX_BUCKET, "harnx_sessions");
        assert_eq!(session_index_key("abc123"), "sessions/abc123/meta");
    }

    #[tokio::test]
    async fn nats_session_index_kv_crud_round_trip() {
        let Some(server_url) = test_nats_url() else {
            eprintln!("skipping nats_session_index_kv_crud_round_trip: HARNX_NATS_TEST_URL unset");
            return;
        };

        let jetstream = connect_jetstream(&server_url).await;
        let store = ensure_test_bucket(&jetstream).await;

        let unique = format!("session-index-test-{}", std::process::id());
        let record = sample_record(&unique);
        let revision = put_record(&store, &record).await.expect("put record");
        assert!(revision > 0);

        let loaded = get_record(&store, &unique)
            .await
            .expect("get record")
            .expect("record exists");
        assert_eq!(loaded, record);

        let (mut cas_record, cas_revision) = get_record_with_revision(&store, &unique)
            .await
            .expect("get record with revision")
            .expect("record with revision exists");
        assert_eq!(cas_revision, revision);
        cas_record.last_activity += 1;
        let updated_revision = update_record_with_revision(&store, &cas_record, cas_revision)
            .await
            .expect("cas update record");
        assert!(updated_revision > cas_revision);

        let listed = list_records(&store).await.expect("list records");
        assert!(listed.iter().any(|entry| entry.session_id == unique));

        super::delete_record(&store, &unique)
            .await
            .expect("delete record");
        assert!(get_record(&store, &unique)
            .await
            .expect("get deleted record")
            .is_none());
    }

    /// Integration test for `list_remote_sessions_with_meta` via config.
    /// Gated on HARNX_NATS_TEST_URL like other NATS integration tests.
    #[tokio::test]
    async fn list_remote_sessions_with_meta_returns_records_from_bucket() {
        use crate::config::{Config, NatsServerConfig};

        let Some(server_url) = test_nats_url() else {
            eprintln!("skipping list_remote_sessions_with_meta test: HARNX_NATS_TEST_URL unset");
            return;
        };

        let jetstream = connect_jetstream(&server_url).await;
        let store = ensure_test_bucket(&jetstream).await;

        // Write a unique test record
        let unique = format!("list-remote-test-{}", std::process::id());
        let record = sample_record(&unique);
        let _revision = put_record(&store, &record).await.expect("put record");

        // Build a minimal Config with the test cluster
        let nats_server = NatsServerConfig {
            name: "test-cluster".into(),
            url: server_url.clone(),
            token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        };

        let config = Config {
            nats_servers: vec![nats_server],
            ..Config::default()
        };

        // Call list_remote_sessions_with_meta
        let metas = config
            .list_remote_sessions_with_meta("test-cluster")
            .await
            .expect("list remote sessions");

        // Verify our record appears
        let found: Vec<_> = metas.iter().filter(|m| m.id == unique).collect();
        assert_eq!(found.len(), 1, "expected exactly one matching session");
        let meta = &found[0];
        assert_eq!(meta.session_id.as_deref(), Some(unique.as_str()));
        assert!(meta.agent_name.is_some());
        assert!(meta.modified.is_some());

        // Cleanup
        super::delete_record(&store, &unique)
            .await
            .expect("delete test record");
    }

    /// Unit test: verify unreachable cluster returns Err (not Ok(empty)).
    /// This guards against regressions where network failures were silently
    /// converted to empty lists.
    ///
    /// Uses a 3-second timeout to ensure fast test completion even on CI.
    #[tokio::test]
    async fn list_remote_sessions_unreachable_cluster_returns_error() {
        use crate::config::{Config, NatsServerConfig};

        // Use a bogus URL that will definitely fail to connect
        // Using a non-routable IP address (198.51.100.1 is in TEST-NET-2, reserved for documentation)
        let nats_server = NatsServerConfig {
            name: "unreachable-cluster".into(),
            url: "nats://198.51.100.1:4222".into(), // non-routable, guaranteed to fail
            token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        };

        let config = Config {
            nats_servers: vec![nats_server],
            ..Config::default()
        };

        // Bound by timeout to ensure fast test completion
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            config.list_remote_sessions_with_meta("unreachable-cluster"),
        )
        .await;

        // Either the connection fails within the timeout (expected), or it times out.
        // Both are acceptable error outcomes for this test.
        match result {
            Ok(Err(_)) => {
                // Connection failed within timeout - expected behavior
            }
            Ok(Ok(sessions)) => {
                panic!(
                    "expected Err for unreachable cluster, got Ok({:?})",
                    sessions
                );
            }
            Err(_timeout) => {
                // Timed out - also acceptable as "couldn't fetch" outcome
            }
        }
    }

    /// Integration test: missing session index bucket maps to Ok via production API.
    /// Emptiness is not asserted because shared-server tests can recreate bucket concurrently.
    #[tokio::test]
    async fn list_remote_sessions_with_meta_on_missing_bucket_returns_ok() {
        let Some(server_url) = test_nats_url() else {
            eprintln!("skipping missing bucket test: HARNX_NATS_TEST_URL unset");
            return;
        };

        let jetstream = connect_jetstream(&server_url).await;
        let _ = jetstream.delete_key_value(SESSION_INDEX_BUCKET).await;

        let nats_server = crate::config::NatsServerConfig {
            name: "test-cluster".to_string(),
            url: server_url,
            token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        };
        let config = crate::config::Config {
            nats_servers: vec![nats_server],
            ..crate::config::Config::default()
        };

        config
            .list_remote_sessions_with_meta("test-cluster")
            .await
            .expect("missing bucket should map to ok result");
    }
}
