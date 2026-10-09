use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_gc_refuses_live_work_then_removes_global_identity_events_and_scoped_lease(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut stream = a2a_events::ensure(&t.h.jetstream, 1).await?;
    let mut policy = stream.info().await?.config.clone();
    policy.duplicate_window = std::time::Duration::from_millis(100);
    t.h.jetstream.update_stream(policy).await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    let first = reservation(&t).await?;
    let config = t.h.config.read().clone();
    assert!(nats_admin::delete_remote_session(
        &config,
        "runner",
        "runner",
        &first.allocation.local_id
    )
    .await
    .is_err());
    assert!(t
        .h
        .metadata
        .get(&first.allocation.storage_key)
        .await?
        .is_some());
    assert!(reservation(&t).await?.allocation == first.allocation);
    let sibling = t.h.session(None, &alice()).await?;
    t.h.llm.release.notify_one();
    let first = settled(&t).await?;
    let stream = t.h.jetstream.get_stream(a2a_events::STREAM).await?;
    let before = stream.cached_info().state.messages;
    let old_first = stream.get_raw_message(1).await?;
    let first_event: harnx_a2a_server::store::context::PendingEvent =
        serde_json::from_slice(&old_first.payload)?;
    let lease = harnx_runtime::nats_lease::NatsSessionLease::acquire_scoped(
        harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: t.h.jetstream.clone(),
            session_id: &first.allocation.storage_key,
            worker_id: "delayed-gc-writer".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("GC race lease")?;
    let claim =
        t.h.store
            .prepare_context_claim(
                harnx_a2a_server::store::context::ContextIdentity {
                    storage_key: &first.allocation.storage_key,
                    local_id: &first.allocation.local_id,
                },
                &lease,
                "old-gc-owner",
            )
            .await?;
    let context = t.h.store.commit_context(&claim).await?;
    let old_write =
        t.h.store
            .prepare_context_update(
                &first.allocation.storage_key,
                &context.version()?,
                "delayed-before-gc",
                |state| state.active = None,
            )
            .await?;
    lease.release().await?;
    let result =
        nats_admin::delete_remote_session(&config, "runner", "runner", &first.allocation.local_id)
            .await?;
    assert!(
        t.h.store.commit_context(&old_write).await.is_err(),
        "GC revision purge fences a previously prepared owner CAS"
    );
    assert!(result.stream_deleted && result.metadata_keys_deleted > 0);
    assert!(t
        .h
        .store
        .first_message_reservation(&first.identity, &first.fingerprint)
        .await?
        .is_none());
    assert!(t
        .h
        .metadata
        .get(&first.allocation.storage_key)
        .await?
        .is_none());
    assert!(t.h.metadata.get(sibling.storage_key()).await?.is_some());
    let kv = t.h.metadata.kv_store();
    assert!(harnx_nats_common::leader_reads::get(
        kv,
        &format!(
            "a2a.registry.{}",
            harnx_core::crypto::sha256(&first.allocation.storage_key)
        )
    )
    .await?
    .is_none());
    let leases = t.h.jetstream.get_key_value("harnx_leases").await?;
    assert!(harnx_nats_common::leader_reads::get(
        &leases,
        &format!("sessions/{}/a2a/lock", first.allocation.storage_key)
    )
    .await?
    .is_none());
    let mut stream = t.h.jetstream.get_stream(a2a_events::STREAM).await?;
    assert!(before > 0);
    assert_eq!(stream.info().await?.state.messages, 0);
    let ghost =
        t.h.jetstream
            .send_publish(
                old_first.subject.clone(),
                async_nats::jetstream::message::PublishMessage::build()
                    .message_id(first_event.commit_id)
                    .expected_last_subject_sequence(0)
                    .payload(old_first.payload),
            )
            .await?
            .await?;
    assert!(!ghost.duplicate, "actual broker dedupe window has elapsed");
    assert_eq!(stream.info().await?.state.messages, 1);
    let mut cursor = 0;
    t.h.store
        .cleanup_terminal_events_for_test(&mut cursor, chrono::Utc::now().timestamp() + 31)
        .await?;
    assert_eq!(stream.info().await?.state.messages, 0, "late first publication is cleaned using retained authority tombstone, without resurrecting task");
    assert!(
        t.h.store
            .read_context(&first.allocation.storage_key)
            .await
            .is_err(),
        "purge tombstone fences stale claim/recovery"
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    nats_admin::delete_remote_session(&config, "runner", "runner", &first.allocation.local_id)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_refuses_unallocated_first_identity_after_lease_loss_then_recovery_closes_it(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    pause.reached().await;
    let first = reservation(&t).await?;
    remove_owner_lease(&t, &first.allocation.storage_key).await?;
    let config = t.h.config.read().clone();
    let error =
        nats_admin::delete_remote_session(&config, "runner", "runner", &first.allocation.local_id)
            .await
            .unwrap_err();
    assert!(
        error.to_string().contains("unallocated or unsettled"),
        "{error:#}"
    );
    assert!(reservation(&t).await?.allocation == first.allocation);
    assert!(t
        .h
        .metadata
        .get(&first.allocation.storage_key)
        .await?
        .is_none());
    let recovered = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&recovered)?["id"], first.allocation.task_id);
    drop(pause);
    let _ = tokio::time::timeout(DEADLINE, send).await??;
    let _ = settled(&t).await?;
    assert!(t.h.llm.requests.lock().is_empty());
    nats_admin::delete_remote_session(&config, "runner", "runner", &first.allocation.local_id)
        .await?;
    assert!(t
        .h
        .store
        .first_message_reservation(&first.identity, &first.fingerprint)
        .await?
        .is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_sweep_compacts_only_after_durable_checkpoints_and_keeps_latest_predecessor(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    t.h.llm.release.notify_one();
    let first = settled(&t).await?;
    let mut stream = t.h.jetstream.get_stream(a2a_events::STREAM).await?;
    let info = stream.info().await?.state.clone();
    assert!(info.messages >= 3);
    let mut cursor = 0;
    t.h.store
        .cleanup_terminal_events_for_test(&mut cursor, chrono::Utc::now().timestamp())
        .await?;
    assert_eq!(
        stream.info().await?.state.messages,
        info.messages,
        "grace retains readers' handoff history"
    );
    let mut cursor = 0;
    t.h.store
        .cleanup_terminal_events_for_test(&mut cursor, chrono::Utc::now().timestamp() + 31)
        .await?;
    assert_eq!(stream.info().await?.state.messages, 1);
    assert!(stream.get_raw_message(info.last_sequence).await.is_ok());
    assert_eq!(
        t.h.store
            .get_task(&first.allocation.storage_key, &first.allocation.task_id)
            .await?
            .unwrap()
            .task
            .status
            .state,
        TaskState::Completed
    );
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], first.allocation.task_id);
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_sweep_prunes_old_losing_candidates_without_expiring_live_winner() -> Result<()> {
    let t = TwoBackends::start().await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    let winner = reservation(&t).await?;
    let mut loser = winner.clone();
    loser.allocation =
        harnx_a2a_server::store::TaskAllocation::new("runner", "losing-candidate".into());
    loser.allocation.created_at = chrono::Utc::now() - chrono::Duration::seconds(301);
    loser.message.parts = vec![a2a_lf::Part::text("mismatched contender")];
    loser.fingerprint = harnx_a2a_server::store::message_fingerprint(&loser.message.parts);
    t.h.store
        .register_recovery(
            &t.h.export,
            &alice(),
            &loser.allocation,
            Some(loser.clone()),
        )
        .await?;
    t.b.backend
        .runner
        .start_supervision(harnx_a2a_server::runner::SupervisionConfig {
            exports: vec![t.h.export.clone()],
            config: t.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: t.b.backend.abort.clone(),
        });
    let key = format!(
        "a2a.registry.{}",
        harnx_core::crypto::sha256(&loser.allocation.storage_key)
    );
    tokio::time::timeout(DEADLINE, async {
        while harnx_nats_common::leader_reads::get(t.h.metadata.kv_store(), &key)
            .await?
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert_eq!(reservation(&t).await?.allocation, winner.allocation);
    assert!(t
        .h
        .store
        .read_context(&winner.allocation.storage_key)
        .await?
        .is_some());
    t.h.llm.release.notify_one();
    let _ = settled(&t).await?;
    t.b.backend.abort.set_ctrlc();
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}
