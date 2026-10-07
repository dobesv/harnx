use super::*;

const TEST_CLUSTER: &str = "reservation-test";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_session_has_short_id_and_durable_reservation() {
    let Some((config, mut nats, _store_dir)) = isolated_session_config().await else {
        return;
    };
    let session_id = Config::reserve_new_session_id(&config).await.unwrap();
    let snapshot = config.read().clone();
    let storage_key = crate::SessionInitializer::from_config(&snapshot)
        .unwrap()
        .session_key(&session_id);
    let jetstream = snapshot.nats_jetstream(TEST_CLUSTER).await.unwrap();
    let metadata_store = crate::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .unwrap();
    let metadata = metadata_store
        .get(&storage_key)
        .await
        .unwrap()
        .expect("reservation creates complete metadata");
    assert_eq!(metadata.metadata.session_id, session_id);
    assert!(metadata_store
        .get_activity(&storage_key)
        .await
        .unwrap()
        .is_some());
    assert!(
        crate::nats_session_log::NatsSessionLog::new(jetstream, &storage_key)
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

fn session_test_config(
    session_id: Option<String>,
    initializer: crate::SessionInitializer,
) -> crate::NatsSessionConfig {
    crate::NatsSessionConfig {
        cluster: TEST_CLUSTER.into(),
        initializer,
        session_id,
        activation_route: crate::SessionActivationRoute::ClusterShared,
    }
}

async fn assert_reserved_sessions(
    config: &GlobalConfig,
    store: &crate::nats_session_metadata::SessionMetadataStore,
    initializer: &crate::SessionInitializer,
) -> (String, String) {
    let reserved = Config::reserve_new_session_id(config).await.unwrap();
    assert_stored_user_id(
        store,
        &initializer.session_key(&reserved),
        Some("cluster-owner"),
    )
    .await;
    let explicit = Config::reserve_new_session_id_with_initializer(
        config,
        initializer.clone().with_user_id("request-owner"),
    )
    .await
    .unwrap();
    assert_stored_user_id(
        store,
        &initializer.session_key(&explicit),
        Some("request-owner"),
    )
    .await;
    (reserved, explicit)
}

async fn assert_implicit_session_creations(
    config: &GlobalConfig,
    store: &crate::nats_session_metadata::SessionMetadataStore,
    initializer: &crate::SessionInitializer,
) {
    use crate::NatsSession;

    for (id, supplied, expected) in [
        (
            Some("implicit-default".into()),
            initializer.clone(),
            "cluster-owner",
        ),
        (None, initializer.clone(), "cluster-owner"),
        (
            Some("blank-explicit".into()),
            initializer.clone().with_user_id(" \t"),
            "cluster-owner",
        ),
        (
            Some("blank-inherited".into()),
            initializer.clone().with_properties(
                serde_json::from_value(serde_json::json!({
                    "user_id": {"value":" \t", "inherit":true}
                }))
                .unwrap(),
            ),
            "cluster-owner",
        ),
        (
            Some("implicit-explicit".into()),
            initializer.clone().with_user_id("request-owner"),
            "request-owner",
        ),
    ] {
        let session = NatsSession::from_global_config(
            session_test_config(id, supplied),
            config,
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .unwrap();
        assert_stored_user_id(store, session.storage_key(), Some(expected)).await;
    }
}

struct ExistingSessions<'a> {
    config: &'a GlobalConfig,
    store: &'a crate::nats_session_metadata::SessionMetadataStore,
    initializer: &'a crate::SessionInitializer,
    reserved: &'a str,
    explicit: &'a str,
}

async fn assert_existing_sessions_preserved_across_config_changes(sessions: ExistingSessions<'_>) {
    let ExistingSessions {
        config,
        store,
        initializer,
        reserved,
        explicit,
    } = sessions;
    use crate::nats_session_metadata::{session_properties, SessionMetadata};
    use crate::NatsSession;

    // Existing records without an identity stay anonymous even after defaults are enabled.
    let anonymous = SessionMetadata::new("existing-anonymous", initializer.clone());
    store.create(&anonymous).await.unwrap().unwrap();
    {
        let mut config = config.write();
        config.user_id = Some("changed-global".into());
        config.nats_servers[0].user_id = Some("changed-cluster".into());
    }
    for (id, expected) in [
        (reserved.to_string(), Some("cluster-owner")),
        (explicit.to_string(), Some("request-owner")),
        ("existing-anonymous".into(), None),
    ] {
        let key = initializer.session_key(&id);
        let before = store.get(&key).await.unwrap().unwrap();
        let session = NatsSession::from_global_config(
            session_test_config(Some(id), initializer.clone().with_user_id("replacement")),
            config,
            harnx_core::abort::create_abort_signal(),
        )
        .await
        .unwrap();
        let after = store.get(session.storage_key()).await.unwrap().unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(
            session_properties(&before.metadata).unwrap(),
            session_properties(&after.metadata).unwrap()
        );
        assert_stored_user_id(store, session.storage_key(), expected).await;
    }
}

async fn assert_blank_cluster_default_falls_back_to_global(
    config: &GlobalConfig,
    store: &crate::nats_session_metadata::SessionMetadataStore,
    initializer: &crate::SessionInitializer,
) {
    config.write().nats_servers[0].user_id = Some(" \t ".into());
    let fallback = Config::reserve_new_session_id(config).await.unwrap();
    assert_stored_user_id(
        store,
        &initializer.session_key(&fallback),
        Some("changed-global"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_id_defaults_cover_reservation_and_implicit_creation_without_overwrite() {
    harnx_core::require_nextest();
    use crate::nats_session_metadata::SessionMetadataStore;
    use crate::SessionInitializer;

    let Some((config, mut nats, _store_dir)) = isolated_session_config().await else {
        return;
    };
    {
        let mut config = config.write();
        config.user_id = Some("global-owner".into());
        config.nats_servers[0].user_id = Some("cluster-owner".into());
    }
    let snapshot = config.read().clone();
    let initializer = SessionInitializer::from_config(&snapshot).unwrap();
    let jetstream = snapshot.nats_jetstream(TEST_CLUSTER).await.unwrap();
    let store = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();

    let (reserved, explicit) = assert_reserved_sessions(&config, &store, &initializer).await;
    assert_implicit_session_creations(&config, &store, &initializer).await;
    assert_existing_sessions_preserved_across_config_changes(ExistingSessions {
        config: &config,
        store: &store,
        initializer: &initializer,
        reserved: &reserved,
        explicit: &explicit,
    })
    .await;
    assert_blank_cluster_default_falls_back_to_global(&config, &store, &initializer).await;
    let _ = nats.kill();
    let _ = nats.wait();
}
async fn assert_stored_user_id(
    store: &crate::nats_session_metadata::SessionMetadataStore,
    key: &str,
    expected: Option<&str>,
) {
    let record = store.get(key).await.unwrap().unwrap();
    let properties = crate::nats_session_metadata::session_properties(&record.metadata).unwrap();
    assert_eq!(properties.text("user_id"), expected);
    if expected.is_some() {
        assert!(properties.get("user_id").unwrap().inherit);
    }
}

async fn isolated_session_config() -> Option<(
    GlobalConfig,
    crate::nats_worker::tests::TestNatsServer,
    tempfile::TempDir,
)> {
    let (url, child, store_dir) = crate::nats_worker::tests::spawn_test_nats().await?;
    let mut config = Config {
        model: harnx_client::Model::new("test", "test-model"),
        ..Config::default()
    };
    config.nats_servers.push(NatsServerConfig {
        user_id: None,
        name: TEST_CLUSTER.to_string(),
        url,
        token: None,
        replicas: Some(1),
        tls: None,
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
        ignore_discovered_servers: None,
        agents: Vec::new(),
    });
    config.set_remote_agent("test-agent".to_string(), TEST_CLUSTER.to_string());
    Some((
        Arc::new(crate::config::ConfigLock::new(config)),
        child,
        store_dir,
    ))
}
