//! Integration tests for read-state live push and list() unread integration.

use crate::common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};

fn storage_key(session_id: &str) -> String {
    harnx_core::session_identity::session_key(Some("test-agent"), session_id)
}

fn new_remote_session_id() -> String {
    format!("unread-session-{}", uuid::Uuid::new_v4())
}

#[tokio::test]
async fn same_local_id_has_agent_scoped_read_state() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = new_remote_session_id();
    let alpha = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("alpha", Default::default()),
    );
    let beta = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("beta", Default::default()),
    );
    let alpha_key = alpha.storage_key();
    let beta_key = beta.storage_key();
    assert_ne!(alpha_key, beta_key);
    store.create(&alpha).await?;
    store.create(&beta).await?;

    store.bump_attention(&alpha_key, 10).await?;
    assert!(store.get_read_state(&alpha_key).await?.is_unread());
    assert!(!store.get_read_state(&beta_key).await?.is_unread());

    let listed = store.list().await?;
    let alpha = listed
        .iter()
        .find(|session| session.metadata.agent.name() == Some("alpha"))
        .expect("alpha session");
    let beta = listed
        .iter()
        .find(|session| session.metadata.agent.name() == Some("beta"))
        .expect("beta session");
    assert!(alpha.unread);
    assert!(!beta.unread);
    Ok(())
}

#[tokio::test]
async fn list_returns_unread_false_for_read_session() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id = new_remote_session_id();

    // Create session metadata
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Mark as read (by default it's read since attention=0)
    store.mark_read(&storage_key(&session_id)).await?;

    // List sessions
    let listed = store.list().await?;

    // Find our session
    let found = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id)
        .expect("session should be listed");

    assert!(
        !found.unread,
        "session with attention=0 and read should not be unread"
    );

    Ok(())
}

#[tokio::test]
async fn list_returns_unread_true_after_attention_bump() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id = new_remote_session_id();

    // Create session metadata
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Bump attention to seq 10
    store.bump_attention(&storage_key(&session_id), 10).await?;

    // List sessions
    let listed = store.list().await?;

    // Find our session
    let found = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id)
        .expect("session should be listed");

    assert!(
        found.unread,
        "session with attention > read should be unread"
    );

    Ok(())
}

#[tokio::test]
async fn list_returns_unread_true_after_manual_unread() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id = new_remote_session_id();

    // Create session metadata
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Bump attention and mark read
    store.bump_attention(&storage_key(&session_id), 10).await?;
    store.mark_read(&storage_key(&session_id)).await?;

    // List sessions - should be read
    let listed = store.list().await?;
    let found = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id)
        .expect("session should be listed");
    assert!(
        !found.unread,
        "session after mark_read should not be unread"
    );

    // Mark manual unread
    store.mark_unread(&storage_key(&session_id)).await?;

    // List again
    let listed = store.list().await?;
    let found = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id)
        .expect("session should be listed");

    assert!(
        found.unread,
        "session with manual_unread should be unread even if attention == read"
    );

    Ok(())
}

#[tokio::test]
async fn list_returns_mixed_unread_status() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id_read = new_remote_session_id();
    let session_id_unread = new_remote_session_id();
    let session_id_manual = new_remote_session_id();

    // Create sessions
    store
        .create(&SessionMetadata::new(
            &session_id_read,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;
    store
        .create(&SessionMetadata::new(
            &session_id_unread,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;
    store
        .create(&SessionMetadata::new(
            &session_id_manual,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Set read state
    store.mark_read(&storage_key(&session_id_read)).await?;
    // Bump attention on unread session
    store
        .bump_attention(&storage_key(&session_id_unread), 5)
        .await?;
    // Mark manual unread on the manual session after reading
    store
        .bump_attention(&storage_key(&session_id_manual), 5)
        .await?;
    store.mark_read(&storage_key(&session_id_manual)).await?;
    store.mark_unread(&storage_key(&session_id_manual)).await?;

    // List all sessions
    let listed = store.list().await?;

    // Find our sessions
    let found_read = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id_read)
        .expect("read session should be listed");
    let found_unread = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id_unread)
        .expect("unread session should be listed");
    let found_manual = listed
        .iter()
        .find(|s| s.metadata.session_id == session_id_manual)
        .expect("manual session should be listed");

    assert!(!found_read.unread, "read session should not be unread");
    assert!(found_unread.unread, "unread session should be unread");
    assert!(
        found_manual.unread,
        "manual unread session should be unread"
    );

    Ok(())
}

#[tokio::test]
async fn get_read_state_is_reusable_for_single_session() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id = new_remote_session_id();

    // Create session
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Get default state
    let state1 = store.get_read_state(&storage_key(&session_id)).await?;
    assert_eq!(state1.last_attention_seq, 0);
    assert_eq!(state1.last_read_seq, 0);
    assert!(!state1.manual_unread);

    // Bump attention
    store.bump_attention(&storage_key(&session_id), 42).await?;

    // Get state again
    let state2 = store.get_read_state(&storage_key(&session_id)).await?;
    assert_eq!(state2.last_attention_seq, 42);
    assert_eq!(state2.last_read_seq, 0);
    assert!(!state2.manual_unread);
    assert!(state2.is_unread());

    Ok(())
}

#[tokio::test]
async fn bulk_list_joins_latest_values_and_skips_deleted_sessions() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    // Empty snapshots must finish without waiting for a new bucket write.
    assert!(store.list().await?.is_empty());
    seed_bulk_list_fixture(&store).await?;
    // Observe the protocol so a fallback to serial lookups cannot make this
    // pass while hiding a broken bulk consumer.
    use futures_util::{FutureExt, StreamExt};
    let mut requests = client.subscribe("$JS.API.>").await?;
    client.flush().await?;
    // Activity takes priority, then creation time and descending session ID.
    let listed = store.list().await?;
    client.flush().await?;
    while let Some(Some(request)) = requests.next().now_or_never() {
        assert!(
            !request.subject.contains(".DIRECT.GET.")
                && !request.subject.contains(".STREAM.MSG.GET."),
            "unexpected point lookup: {}",
            request.subject
        );
    }
    assert_eq!(
        listed
            .iter()
            .map(|s| s.metadata.session_id.as_str())
            .collect::<Vec<_>>(),
        ["c", "b", "a"]
    );
    assert!(listed[0].unread);
    assert!(!listed[1].unread);
    assert!(listed[2].unread);
    assert_eq!(listed[1].metadata.agent.name(), Some("beta"));
    assert!(listed[1].activity.is_none());
    assert!(listed[2].activity.is_none());
    Ok(())
}

#[tokio::test]
async fn bulk_list_rejects_invalid_metadata_storage_key() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        "valid-id",
        SessionInitializer::named("alpha", Default::default()),
    );
    store
        .kv_store()
        .put(
            harnx_runtime::nats_session_metadata::metadata_key("wrong-key"),
            serde_json::to_vec(&metadata)?.into(),
        )
        .await?;
    assert!(store.list().await.is_err());
    Ok(())
}

async fn seed_bulk_list_fixture(store: &SessionMetadataStore) -> Result<()> {
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 123).unwrap();
    for (id, agent) in [
        ("a", "alpha"),
        ("b", "beta"),
        ("c", "alpha"),
        ("deleted", "alpha"),
        ("purged", "beta"),
    ] {
        let mut metadata =
            SessionMetadata::new(id, SessionInitializer::named(agent, Default::default()));
        metadata.created_at = timestamp;
        let revision = store.create(&metadata).await?.unwrap();
        let key = metadata.storage_key();
        let activity_key = harnx_runtime::nats_session_metadata::activity_key(&key);
        store.kv_store().delete(&activity_key).await?;
        match id {
            "a" => {
                store.bump_attention(&key, 42).await?;
                store.mark_read(&key).await?;
                store.mark_unread(&key).await?;
            }
            "b" => {
                // Malformed optional records must not hide otherwise valid metadata.
                store
                    .kv_store()
                    .put(&activity_key, "not json".into())
                    .await?;
                store
                    .kv_store()
                    .put(
                        harnx_runtime::nats_session_metadata::read_cursor_key(&key, "default"),
                        "not json".into(),
                    )
                    .await?;
            }
            "c" => {
                let activity = harnx_runtime::nats_session_metadata::SessionActivity {
                    first_activation_at: Some(timestamp),
                    last_activity_at: timestamp + chrono::Duration::seconds(10),
                };
                store
                    .kv_store()
                    .put(&activity_key, serde_json::to_vec(&activity)?.into())
                    .await?;
                store.bump_attention(&key, 5).await?;
            }
            "deleted" => {
                store
                    .kv_store()
                    .delete(harnx_runtime::nats_session_metadata::metadata_key(&key))
                    .await?;
            }
            "purged" => {
                store.purge_session_prefix(&key).await?;
            }
            _ => unreachable!(),
        }
        if id == "a" {
            let listed = store.list().await?;
            assert_eq!(listed[0].metadata_revision, revision);
            assert!(listed[0].activity.is_none());
            assert!(listed[0].unread);
        }
    }
    Ok(())
}
