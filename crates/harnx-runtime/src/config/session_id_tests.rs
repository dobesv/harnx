use super::*;

const TEST_CLUSTER: &str = "reservation-test";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_session_has_short_id_and_durable_reservation() {
    let Some((config, mut nats, _store_dir)) = isolated_session_config().await else {
        return;
    };
    let session_id = Config::reserve_new_session_id(&config).await.unwrap();
    let snapshot = config.read().clone();
    let jetstream = snapshot.nats_jetstream(TEST_CLUSTER).await.unwrap();
    let metadata_store = crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .unwrap();
    let metadata = metadata_store
        .get(&session_id)
        .await
        .unwrap()
        .expect("reservation creates complete metadata");
    assert_eq!(metadata.metadata.session_id, session_id);
    assert!(metadata_store
        .get_activity(&session_id)
        .await
        .unwrap()
        .is_some());
    assert!(
        crate::nats_session_log::NatsSessionLog::new(jetstream, &session_id)
            .load_events_async()
            .await
            .unwrap()
            .is_empty()
    );
    config.write().use_session(Some(&session_id)).unwrap();

    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(
        session.id.len(),
        6,
        "anonymous session ID should be 6-char short ID"
    );
    assert!(
        crate::utils::session_name::decode_timestamp_session_id(&session.id).is_some(),
        "anonymous session ID should be a valid base64url timestamp short ID"
    );
    let _ = nats.kill();
    let _ = nats.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_session_id_collision_retries() {
    let Some((config, mut nats, _store_dir)) = isolated_session_config().await else {
        return;
    };
    let id1 = Config::reserve_new_session_id(&config).await.unwrap();
    let id2 = Config::reserve_new_session_id(&config).await.unwrap();
    assert_ne!(
        id1, id2,
        "concurrent anonymous sessions must get unique IDs"
    );
    assert_eq!(id1.len(), 6);
    assert_eq!(id2.len(), 6);
    let _ = nats.kill();
    let _ = nats.wait();
}

async fn isolated_session_config() -> Option<(GlobalConfig, std::process::Child, tempfile::TempDir)>
{
    let (url, child, store_dir) = crate::nats_worker::tests::spawn_test_nats().await?;
    let mut config = Config {
        model: harnx_client::Model::new("test", "test-model"),
        ..Config::default()
    };
    config.nats_servers.push(NatsServerConfig {
        name: TEST_CLUSTER.to_string(),
        url,
        token: None,
        replicas: Some(1),
        tls: None,
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
        agents: Vec::new(),
    });
    config.set_remote_agent("test-agent".to_string(), TEST_CLUSTER.to_string());
    Some((Arc::new(RwLock::new(config)), child, store_dir))
}
