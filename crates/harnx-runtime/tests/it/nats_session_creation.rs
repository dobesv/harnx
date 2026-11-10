use crate::common::spawn_nats_server;
use anyhow::Result;
use async_nats::jetstream::{self, context::GetStreamErrorKind, stream::Config as StreamConfig};
use futures_util::TryStreamExt;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_session_log::{stream_name_for_session, NatsSessionLog},
    nats_session_metadata::{SessionMetadataStore, SESSION_METADATA_BUCKET},
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};

fn config(id: &str) -> NatsSessionConfig {
    NatsSessionConfig {
        cluster: "test".into(),
        initializer: SessionInitializer::named("test-agent", Default::default()),
        session_id: Some(id.into()),
        activation_route: SessionActivationRoute::ClusterShared,
    }
}

fn entry(id: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.into()),
        role: MessageRole::User,
        content: MessageContent::Text(id.into()),
        timestamp: None,
        fence_token: None,
    }
}

async fn assert_no_stream(jetstream: &jetstream::Context, key: &str) -> Result<()> {
    let name = stream_name_for_session(key);
    let error = match jetstream.get_stream(&name).await {
        Ok(_) => panic!("session stream '{name}' exists without a metadata reservation"),
        Err(error) => error,
    };
    match error.kind() {
        GetStreamErrorKind::JetStream(error) => {
            assert_eq!(error.kind(), jetstream::ErrorCode::STREAM_NOT_FOUND);
        }
        kind => panic!("expected STREAM_NOT_FOUND, got {kind:?}"),
    }
    Ok(())
}

async fn reject_metadata_writes(jetstream: &jetstream::Context) -> Result<StreamConfig> {
    let stream = jetstream
        .get_stream(format!("KV_{SESSION_METADATA_BUCKET}"))
        .await?;
    let original = stream.cached_info().config.clone();
    let mut rejecting = original.clone();
    // Reject the actual KV publish, after transcript inspection and metadata
    // validation. Unlike sealing the stream, this can be undone for a retry.
    rejecting.max_message_size = 1;
    jetstream.update_stream(rejecting).await?;
    Ok(original)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_session_metadata_write_failure_does_not_create_a_stream() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let config = config("explicit-new-session");
    let key = config.initializer.session_key("explicit-new-session");
    let original = reject_metadata_writes(&jetstream).await?;

    let result = NatsSession::new(
        config.clone(),
        client.clone(),
        jetstream.clone(),
        create_abort_signal(),
    )
    .await;
    let error = result.err().expect("KV must reject metadata creation");
    assert!(
        format!("{error:#}").contains("Failed to create session metadata"),
        "{error:#}"
    );
    assert!(store.get(&key).await?.is_none());
    assert_no_stream(&jetstream, &key).await?;

    jetstream.update_stream(original).await?;
    let session =
        NatsSession::new(config, client, jetstream.clone(), create_abort_signal()).await?;
    assert!(store.get(session.storage_key()).await?.is_some());
    let stream = jetstream.get_stream(stream_name_for_session(&key)).await?;
    assert_eq!(stream.cached_info().state.messages, 0);
    assert_eq!(stream.cached_info().config.num_replicas, 1);
    NatsSessionLog::new_with_replicas(jetstream.clone(), session.storage_key(), 1)
        .append_event_async(&entry("first-prompt"))
        .await?;
    assert_eq!(
        jetstream
            .get_stream(stream_name_for_session(&key))
            .await?
            .cached_info()
            .state
            .messages,
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_legacy_stream_survives_metadata_write_failure_and_retry() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let config = config("empty-legacy-session");
    let key = config.initializer.session_key("empty-legacy-session");
    NatsSessionLog::new_with_replicas(jetstream.clone(), &key, 1)
        .load_events_async()
        .await?;
    let name = stream_name_for_session(&key);
    let before = jetstream.get_stream(&name).await?.cached_info().clone();
    let original = reject_metadata_writes(&jetstream).await?;

    let result = NatsSession::new(
        config.clone(),
        client.clone(),
        jetstream.clone(),
        create_abort_signal(),
    )
    .await;
    let error = result.err().expect("KV must reject metadata creation");
    assert!(
        format!("{error:#}").contains("Failed to create session metadata"),
        "{error:#}"
    );
    assert!(store.get(&key).await?.is_none());
    let after = jetstream.get_stream(&name).await?.cached_info().clone();
    assert_eq!(before.config, after.config);
    assert_eq!(before.created, after.created);
    assert_eq!(before.state, after.state);

    jetstream.update_stream(original).await?;
    let session =
        NatsSession::new(config, client, jetstream.clone(), create_abort_signal()).await?;
    assert!(store.get(session.storage_key()).await?.is_some());
    let reopened = jetstream.get_stream(&name).await?.cached_info().clone();
    assert_eq!(before.config, reopened.config);
    assert_eq!(before.created, reopened.created);
    assert_eq!(before.state, reopened.state);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_explicit_creators_share_one_metadata_record_and_stream() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let create_and_append = |prompt: &'static str| {
        let client = client.clone();
        let jetstream = jetstream.clone();
        async move {
            let session = NatsSession::new(
                config("racing-explicit-session"),
                client,
                jetstream.clone(),
                create_abort_signal(),
            )
            .await?;
            NatsSessionLog::new_with_replicas(jetstream, session.storage_key(), 1)
                .append_event_async(&entry(prompt))
                .await?;
            Ok::<_, anyhow::Error>(session)
        }
    };
    let (first, second) = tokio::join!(create_and_append("first"), create_and_append("second"));
    let (first, second) = (first?, second?);
    assert_eq!(first.storage_key(), second.storage_key());
    let records = store.list().await?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].metadata.storage_key(), first.storage_key());
    let names: Vec<_> = jetstream.stream_names().try_collect().await?;
    let session_streams: Vec<_> = names
        .into_iter()
        .filter(|name| name.starts_with("SESSION_"))
        .collect();
    assert_eq!(
        session_streams,
        vec![stream_name_for_session(first.storage_key())]
    );
    assert_eq!(
        NatsSessionLog::new(jetstream, first.storage_key())
            .load_events_async()
            .await?
            .len(),
        2
    );
    Ok(())
}
