use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_input_is_rejected_before_reservation_but_retained_retry_survives_lowered_limit(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut too_big = first_message();
    too_big["message"]["messageId"] = json!("oversize-input");
    too_big["message"]["parts"][0]["text"] = json!("x".repeat(524288));
    let rejected = t.a.rpc("alice", "SendMessage", too_big).await?;
    assert_eq!(rejected["error"]["code"], -32602);
    assert!(t.h.llm.requests.lock().is_empty());
    let mut cursor = 0;
    assert!(t
        .h
        .store
        .next_recovery_registration(&mut cursor)
        .await?
        .is_none());
    let admitted = t.a.rpc("alice", "SendMessage", first_message()).await?;
    t.h.llm.release.notify_one();
    let first = settled(&t).await?;
    let mut metadata_stream = t.h.jetstream.get_stream("KV_harnx_sessions").await?;
    let mut config = metadata_stream.info().await?.config.clone();
    config.max_message_size = 128;
    t.h.jetstream.update_stream(config).await?;
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], result_task(&admitted)?["id"]);
    assert_eq!(reservation(&t).await?.allocation, first.allocation);
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_worker_output_settles_failed_with_real_stop_and_no_stuck_outbox() -> Result<()> {
    let h = Harness::start(Script::Overflow).await?;
    let replica = Replica::new(&h, h.store.clone(), h.runner.clone())?;
    let admitted = replica.rpc("alice", "SendMessage", first_message()).await?;
    let id = result_task(&admitted)?["id"].as_str().context("id")?;
    let local = result_task(&admitted)?["contextId"]
        .as_str()
        .context("contextId")?;
    let storage = harnx_core::session_identity::session_key(Some("runner"), local);
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = h
                .store
                .read_context(&storage)
                .await?
                .context("overflow context")?;
            let active = context.document.state.active.as_ref().unwrap();
            if active.stop_confirmed
                && active.projections.archive
                && active.projections.message_mapping
                && active.projections.final_event
                && active.publication.pending.is_none()
                && context.document.owner.is_none()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(h.task(id).await?.task.status.state, TaskState::Failed);
    let retry = replica.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], id);
    assert_eq!(h.llm.requests.lock().len(), 1);
    assert!(h.logs.text().contains("payload budget"));
    h.runner.shutdown().await;
    Ok(())
}
