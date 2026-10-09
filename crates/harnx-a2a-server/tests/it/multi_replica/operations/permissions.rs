use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_backend_and_real_worker_complete_one_turn_and_close_missing_admission(
) -> Result<()> {
    let profile = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/it/operations/nats-permissions.conf"
    ))?;
    let h = Harness::start_with_broker(Script::Text, None, Some(&profile)).await?;
    a2a_events::ensure(&h.jetstream, 1).await?;
    let replica = Replica::new(&h, h.store.clone(), h.runner.clone())?;
    let first = replica.rpc("alice", "SendMessage", first_message()).await?;
    let id = result_task(&first)?["id"].as_str().context("id")?;
    h.llm.release.notify_one();
    tokio::time::timeout(DEADLINE, async {
        loop {
            if h.task(id).await?.task.status.state.is_terminal() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(
        h.task(id).await?.task.status.state,
        TaskState::Completed,
        "{}",
        h.logs.text()
    );
    assert_eq!(h.llm.requests.lock().len(), 1);
    let mut missing = first_message();
    missing["message"]["messageId"] = json!("restricted-missing");
    let hooks = h.runner.fault_hooks();
    let mut pause = hooks.pause(
        Boundary::FirstReservation,
        hooks.count(Boundary::FirstReservation) + 1,
    );
    let send = tokio::spawn({
        let app = replica.app.clone();
        let params = missing.clone();
        async move { rpc(app, "alice", "SendMessage", params).await }
    });
    pause.reached().await;
    let message: a2a_lf::Message = serde_json::from_value(missing["message"].clone())?;
    let key = harnx_a2a_server::store::DedupeKey {
        cluster: "runner".into(),
        export: "runner".into(),
        owner: Some("alice".into()),
        message_id: "restricted-missing".into(),
    };
    let reserved = h
        .store
        .first_message_reservation(
            &key,
            &harnx_a2a_server::store::message_fingerprint(&message.parts),
        )
        .await?
        .context("restricted first")?;
    h.jetstream
        .get_key_value("harnx_leases")
        .await?
        .delete(format!(
            "sessions/{}/a2a/lock",
            reserved.allocation.storage_key
        ))
        .await?;
    let store_b = Arc::new(A2aStore::new(h.metadata.clone()));
    let b = Replica::new(&h, store_b.clone(), Runner::new(store_b))?;
    let closed = b.rpc("alice", "SendMessage", missing).await?;
    assert_eq!(result_task(&closed)?["id"], reserved.allocation.task_id);
    drop(pause);
    let _ = tokio::time::timeout(DEADLINE, send).await??;
    let closed_task = h
        .store
        .get_task(
            &reserved.allocation.storage_key,
            &reserved.allocation.task_id,
        )
        .await?
        .context("closed task")?;
    assert_eq!(closed_task.task.status.state, TaskState::Failed);
    assert_eq!(
        h.llm.requests.lock().len(),
        1,
        "missing admission isn't executed"
    );
    let log = NatsSessionLog::new(h.jetstream.clone(), &reserved.allocation.storage_key);
    assert!(prompts(&log).await?.is_empty());
    let info = h.jetstream.client().server_info();
    std::fs::write(
        h.config_dir().join("nats_servers/runner.yaml"),
        format!(
            "url: {:?}\n",
            format!(
                "nats://worker:worker_test_password@{}:{}",
                info.host, info.port
            )
        ),
    )?;
    let worker_config =
        harnx_runtime::config::Config::load_from_file(&h.config_dir().join("config.yaml"))?;
    nats_admin::delete_remote_session(
        &worker_config,
        "runner",
        "runner",
        &reserved.allocation.local_id,
    )
    .await?;
    assert!(h
        .metadata
        .get(&reserved.allocation.storage_key)
        .await?
        .is_none());
    assert!(h
        .store
        .first_message_reservation(&key, &reserved.fingerprint)
        .await?
        .is_none());
    assert_eq!(
        h.task(id).await?.task.status.state,
        TaskState::Completed,
        "worker GC preserves sibling context"
    );
    let mut events = h.jetstream.get_stream(a2a_events::STREAM).await?;
    assert_eq!(events.info().await?.state.consumer_count, 0);
    h.runner.shutdown().await;
    Ok(())
}
