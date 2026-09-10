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
async fn reserve_short_session_id_retries_on_collision() {
    use crate::nats_session_metadata::{SessionInitializer, SessionMetadata};
    use crate::utils::session_name::{
        decode_timestamp_session_id, encode_timestamp_session_id, reserve_short_session_id,
    };

    let Some((config, mut nats, _store_dir)) = isolated_session_config().await else {
        return;
    };
    let snapshot = config.read().clone();
    let jetstream = snapshot.nats_jetstream(TEST_CLUSTER).await.unwrap();
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .unwrap();
    let initializer = SessionInitializer::from_config(&snapshot).unwrap();

    // Pre-seed the contiguous timestamp slots the reservation starts from so its
    // first candidate always collides. `reserve_short_session_id` begins at the
    // current second, so seeding `base ..= base + SEED_AHEAD` forces at least one
    // `None => retry` step regardless of the small delay before it reads the
    // clock, and the first free slot (`base + SEED_AHEAD + 1`) is deterministic
    // as long as fewer than `SEED_AHEAD` seconds elapse while seeding.
    const SEED_AHEAD: u64 = 4;
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let seeded: Vec<String> = (0..=SEED_AHEAD)
        .map(|offset| encode_timestamp_session_id(base + offset))
        .collect();
    for candidate in &seeded {
        let metadata = SessionMetadata::new(candidate, initializer.clone());
        assert!(
            store.create(&metadata).await.unwrap().is_some(),
            "seeding collision slot {candidate} should create fresh metadata"
        );
    }

    let reserved = reserve_short_session_id(&store, &initializer)
        .await
        .unwrap();

    assert_eq!(reserved.len(), 6);
    assert!(
        !seeded.contains(&reserved),
        "reservation must retry past every pre-seeded slot, got {reserved}"
    );
    assert_eq!(
        decode_timestamp_session_id(&reserved),
        Some(base + SEED_AHEAD + 1),
        "reservation must land on the first free slot after the seeded block"
    );
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
