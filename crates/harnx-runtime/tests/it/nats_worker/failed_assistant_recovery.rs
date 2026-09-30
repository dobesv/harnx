use super::*;

fn assert_recovered_log(raw: &[(u64, SessionLogEntry)]) -> Result<()> {
    assert!(raw.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::EditEntries {
            from: 2,
            to: 2,
            replacements,
        } if replacements.is_empty()
    )));
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(raw)?;
    assert!(!effective.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role, content, .. }
            if role.is_assistant() && content.to_text().is_empty()
    )));
    assert!(effective.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role, content, .. }
            if role.is_assistant() && content.to_text() == "folded:retry original prompt"
    )));
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unterminated_failed_assistant_redelivery_retries_original_prompt() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-failed-assistant-recovery",
        fold_capture_call_fn(Arc::clone(&calls), Arc::clone(&prompts)),
    )
    .await?;
    let jetstream = local_test_nats(server.url()).await?;
    let session_id = "failed-assistant-redelivery";
    let storage_key = storage_key(session_id);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    store
        .create(&SessionMetadata::new(
            session_id,
            SessionInitializer::inline(
                "recovery test agent",
                Default::default(),
                SessionOverrides::default(),
            ),
        ))
        .await?;
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1);
    log.append_event_async(&append_user_message_entry(
        "failed-assistant-user",
        "retry original prompt",
    ))
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text(String::new()),
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    publish_session_activate(&jetstream, "local", &SessionActivate::new(&storage_key), 1).await?;

    wait_until(CI_SAFE_TIMEOUT, || calls.load(Ordering::SeqCst) == 1).await?;
    wait_for_worker_session_cleanup(&jetstream, session_id).await?;
    assert_eq!(
        prompts.lock().await.as_slice(),
        &["retry original prompt".to_string()]
    );
    assert_recovered_log(&log.load_events_async().await?)?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
