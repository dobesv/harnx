use super::*;

fn abort_returning_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, abort| {
        Box::pin(async move {
            harnx_core::abort::wait_abort_signal(&abort).await;
            anyhow::bail!("interrupted by user")
        })
    })
}

async fn wait_for_cancel(log: &NatsSessionLog) -> Result<Vec<(u64, SessionLogEntry)>> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = log.load_events_async().await?;
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            {
                return Ok::<_, anyhow::Error>(entries);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

/// Wait until a worker holds this session's lease, which is the only claim a
/// worker makes on a session now.
async fn wait_for_worker_session_claim(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    let lease_config = NatsLeaseConfig::default();
    let session_key = storage_key(session_id);
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if let Some(bucket) = open_lease_bucket(jetstream, &lease_config).await {
                if lease_holder_in(&bucket, &lease_config, &session_key)
                    .await?
                    .is_some()
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_signal_cancels_blocked_worker_and_persists_tombstone() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let entered = Arc::new(Notify::new());
    let model_dropped = tokio_util::sync::CancellationToken::new();
    let config = local_nats_runtime_config(server.url());
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-abort-cancel",
        abort_blocked_call_fn(Arc::clone(&entered), model_dropped.clone()),
    )
    .await?;

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = "abort-cancel";
    let abort = create_abort_signal();
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: harnx_runtime::SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        abort.clone(),
    )
    .await?;

    let run_turn = tokio::spawn(async move {
        session
            .run_turn("block until cancelled", Arc::new(NullSink), None)
            .await
    });
    tokio::time::timeout(CI_SAFE_TIMEOUT, entered.notified()).await?;
    abort.set_ctrlc();

    let result = tokio::time::timeout(CI_SAFE_TIMEOUT, run_turn).await???;
    assert!(
        result.was_cancelled,
        "NATS session turn should report cancellation"
    );

    let log = NatsSessionLog::new_with_replicas(jetstream, storage_key(session_id), 1);
    let entries = wait_for_cancel(&log).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, model_dropped.cancelled()).await?;
    let cancel_fence = entries.iter().find_map(|(_, entry)| match entry {
        SessionLogEntry::Cancel { fence_token, .. } => Some(*fence_token),
        _ => None,
    });
    assert!(
        cancel_fence.is_some(),
        "Cancel must carry worker fence token"
    );
    assert!(matches!(
        reconstruct_state_from_nats(&entries).turn_status,
        TurnStatus::Idle | TurnStatus::InterruptedPendingWindUp { .. }
    ));

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_immediately_after_activation_ack_is_not_lost() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-activation-cancel",
        abort_returning_call_fn(),
    )
    .await?;

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = "immediate-activation-cancel";
    // Raw log writers are an internal protocol and must initialize canonical
    // metadata before the first transcript entry.
    seed_session_metadata(&jetstream, session_id).await?;
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key(session_id), 1);
    log.append_event_async(&append_user_message_entry(
        "immediate-cancel-user",
        "block until cancelled",
    ))
    .await?;

    activate_session(&jetstream, session_id).await?;
    // The activation remains durable until shutdown. Observe the worker taking
    // the session's lease instead of waiting for the final acknowledgement.
    wait_for_worker_session_claim(&jetstream, session_id).await?;
    harnx_runtime::send_control_command(&client, &storage_key(session_id), ControlCommand::Cancel)
        .await?;

    // Cancellation can win before the model call starts. The durable tombstone,
    // rather than an observer inside the call, proves the control was not lost.
    let entries = wait_for_cancel(&log).await?;
    assert!(
        matches!(
            reconstruct_state_from_nats(&entries).turn_status,
            TurnStatus::Idle | TurnStatus::InterruptedPendingWindUp { .. }
        ),
        "turn must end cancelled when control follows activation ack; entries={entries:#?}"
    );

    wait_for_worker_session_cleanup(&jetstream, session_id).await?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
