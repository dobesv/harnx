use super::*;
use anyhow::{Context, Result};
use harnx_core::{message::MessageRole, session::SessionLogEntry};

#[tokio::test]
async fn expired_replay_returns_completed_child_without_cancelling_or_readmitting() -> Result<()> {
    let (url, mut nats, _) = crate::nats_worker::tests::spawn_test_nats()
        .await
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(&url).await?);
    let session = completed_child(&js).await?;
    let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(
        js.clone(),
        session.storage_key(),
        1,
    );
    let buffer = Arc::new(InvocationBufferingSink::new(Arc::new(
        harnx_core::event::NullSink,
    )));
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(&js, 1).await?;
    let record = crate::nats_session_metadata::RunLimitsRecord::admit_root(
        crate::nats_session_metadata::RunIdentity::new(),
        crate::nats_session_metadata::InvocationIdentity::from_string("invocation".into()),
        chrono::Utc::now() - chrono::Duration::seconds(30),
        Default::default(),
        None,
        crate::nats_session_metadata::CallTimeoutOverride::from_optional(Some(1)),
    )?;
    store
        .put_invocation_limits(session.storage_key(), &record)
        .await?;
    assert!(record.is_expired_at(chrono::Utc::now()));
    let before = log.load_events_async().await?;
    let turn = await_prompt_turn(
        &session,
        &buffer,
        AwaitTurnParams {
            content: MessageContent::Text("original work".to_string()),
            cancel: CancellationToken::new(),
        },
    )
    .await;
    let PromptTurn::Completed(Ok(result)) = turn else {
        panic!("expired replay lost a durable result");
    };
    assert_eq!(result.response.as_deref(), Some("already finished"));
    assert!(!result.was_cancelled);
    assert_eq!(log.load_events_async().await?.len(), before.len());
    assert!(
        !log.load_events_async()
            .await?
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        "an expired replay of a completed child must not interrupt it"
    );
    let _ = nats.kill();
    let _ = nats.wait();
    Ok(())
}

async fn completed_child(js: &async_nats::jetstream::Context) -> Result<NatsSession> {
    let session = NatsSession::new(
        crate::NatsSessionConfig {
            cluster: "local".into(),
            initializer: crate::SessionInitializer::inline(
                "",
                Default::default(),
                Default::default(),
            ),
            session_id: Some("completed-child".into()),
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        js.client().clone(),
        js.clone(),
        crate::utils::create_abort_signal(),
    )
    .await?;
    let session = session
        .with_external_admission()
        .with_execution_parent("parent".into(), "invocation".into());
    let session = session.with_admission_timeout(Some(1));
    let prompt = session.enqueue_text("original work").await?;
    let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(
        js.clone(),
        session.storage_key(),
        1,
    );
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("answer".into()),
        role: MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("already finished".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: prompt.user_msg_seq(),
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await?;
    Ok(session)
}
