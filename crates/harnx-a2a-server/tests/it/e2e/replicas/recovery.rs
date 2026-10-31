use super::*;
use std::os::unix::process::ExitStatusExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternating_process_sigkill_background_recovery_restart_rejects_stale_write_without_replay(
) -> Result<()> {
    let mut pair = Pair::start(Script::CountedTool).await?;
    let message = chat("killed-identity", None);
    let mut routes = pair.routes();
    let mut owner_stream = routes
        .stream("SendStreamingMessage", json!({"message":message}))
        .await?;
    let (original, _) = lifecycle::partial(&mut owner_stream).await?;
    let id = original["id"].clone();
    let context = original["contextId"].clone();
    let mut remote = routes.stream("SubscribeToTask", json!({"id":id})).await?;
    let (snapshot, _) = lifecycle::partial(&mut remote).await?;
    assert_eq!(snapshot["id"], id);
    assert_eq!(tool_count(&pair.a.h)?, 1);
    one_prompt(&pair.a.h, &context).await?;
    let (storage, write) = stale_ticket(&pair.a.h, &context).await?;
    let old_owner = write.document().owner.clone().context("old fence")?;
    let worker_key = format!("sessions/{storage}/lock");
    drop(routes);

    let terminal = kill_owner_and_await_background_settle(
        &mut pair,
        owner_stream,
        &mut remote,
        RecoveryTarget {
            worker_key: &worker_key,
            task_id: &id,
        },
    )
    .await?;
    let current = pair
        .b
        .h
        .store
        .read_context(&storage)
        .await?
        .context("successor")?;
    assert_ne!(current.document.owner, Some(old_owner));
    let error = pair.b.h.store.commit_context(&write).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<AuthorityError>(),
        Some(&AuthorityError::Conflict)
    );
    assert_eq!(tool_count(&pair.b.h)?, 1);
    assert_eq!(pair.b.h.llm.requests.lock().len(), 2);

    let old_pid = pair.a._process.child.id();
    pair.a = Server::start_replica(pair.b.h.clone(), "a2a-restarted.log").await?;
    assert_ne!(pair.a._process.child.id(), old_pid);
    let mut routes = pair.routes();
    retained_reads(&mut routes, &message, &id, &context).await?;
    assert_eq!(tool_count(&pair.a.h)?, 1);
    assert_eq!(pair.a.h.llm.requests.lock().len(), 2);
    one_prompt(&pair.a.h, &context).await?;
    routes.assert_routes(&["SendMessage", "GetTask", "ListTasks"]);
    complete_successor(&mut routes, &context, &id).await?;
    let retained = pair.a.h.task(id.as_str().unwrap()).await?;
    assert_eq!(retained.revision, terminal.revision);
    assert_eq!(retained.execution_id, terminal.execution_id);
    Ok(())
}

/// Kill the owning replica and wait for B's production background sweep to
/// settle the orphaned task — there is no HTTP polling or reconciliation to
/// do it. Where the runtime can prove the owner's pid is gone (a strong host
/// identity; see `nats_lease::holder_reclaim_supported`) the sweep reclaims
/// its dead-holder lease instead of waiting out the 30 s lease TTL; the
/// timer starts before the SIGKILL so the budget covers the whole path.
/// Elsewhere the sweep still waits the TTL, so the idle SSE stream must have
/// kept the connection alive with keep-alive comments in the meantime.
/// Returns the terminal task record B produced.
struct RecoveryTarget<'a> {
    worker_key: &'a str,
    task_id: &'a Value,
}

async fn kill_owner_and_await_background_settle(
    pair: &mut Pair,
    owner_stream: Frames,
    remote: &mut Frames,
    target: RecoveryTarget<'_>,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    let RecoveryTarget {
        worker_key,
        task_id,
    } = target;
    let leases = pair.a.h.jetstream.get_key_value("harnx_leases").await?;
    let before = harnx_nats_common::leader_reads::entry(&leases, worker_key)
        .await?
        .context("worker lease")?;
    let settle_started = std::time::Instant::now();
    kill_owner(pair, worker_key, before.revision).await?;
    drop(owner_stream);
    let terminal = settled(&pair.b.h, task_id).await?;
    assert_eq!(terminal.task.status.state, a2a_lf::TaskState::Failed);
    terminal_frames(remote, "TASK_STATE_FAILED").await?;
    if harnx_runtime::nats_lease::holder_reclaim_supported() {
        // kill_owner also waits for the shared worker's next renewal (up to
        // the 10 s renew interval) before returning, so the budget must
        // cover that too. A reclaim regression instead waits the holder's
        // full 30 s lease, far above this bound.
        assert!(
            settle_started.elapsed() < std::time::Duration::from_secs(20),
            "background sweep settled {:?} after the owner's death, not by \
             reclaiming its dead-holder lease",
            settle_started.elapsed()
        );
    } else {
        assert!(
            remote.comments > 0,
            "real idle HTTP stream must send keep-alives before lease expiry"
        );
    }
    Ok(terminal)
}

async fn kill_owner(pair: &mut Pair, worker_key: &str, revision: u64) -> Result<()> {
    let leases = pair.a.h.jetstream.get_key_value("harnx_leases").await?;
    pair.a._process.child.start_kill()?;
    let exit = poll_exit(REAP_DEADLINE, || pair.a._process.child.try_wait())?;
    assert_eq!(exit.signal(), Some(9));
    tokio::time::timeout(DEADLINE, async {
        loop {
            if harnx_nats_common::leader_reads::entry(&leases, worker_key)
                .await?
                .is_some_and(|entry| entry.revision > revision)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("worker renewal after SIGKILL")??;
    Ok(())
}

async fn retained_reads(
    routes: &mut Alternating<'_>,
    message: &Value,
    id: &Value,
    context: &Value,
) -> Result<()> {
    for _ in 0..2 {
        let retry = task(
            &routes
                .rpc(RpcCall::new(
                    "runner",
                    "alice",
                    "SendMessage",
                    immediate(message.clone()),
                ))
                .await?,
        )?;
        assert_eq!(retry["id"], *id);
        assert_eq!(retry["status"]["state"], "TASK_STATE_FAILED");
    }
    for _ in 0..2 {
        let read = routes
            .rpc(RpcCall::new("runner", "alice", "GetTask", json!({"id":id})))
            .await?;
        assert_eq!(read["result"]["status"]["state"], "TASK_STATE_FAILED");
    }
    for _ in 0..2 {
        let list = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "ListTasks",
                json!({"contextId":context}),
            ))
            .await?;
        assert_eq!(
            list["result"]["tasks"].as_array().context("tasks")?.len(),
            1
        );
    }
    Ok(())
}

async fn complete_successor(
    routes: &mut Alternating<'_>,
    context: &Value,
    id: &Value,
) -> Result<()> {
    let mut successor_stream = routes
        .stream(
            "SendStreamingMessage",
            json!({"message":chat("after-restart", Some(context))}),
        )
        .await?;
    let (successor, mut answer) = lifecycle::partial(&mut successor_stream).await?;
    assert_ne!(successor["id"], *id);
    let mut sibling_stream = routes
        .stream("SubscribeToTask", json!({"id":successor["id"]}))
        .await?;
    let (_, mut sibling_answer) = lifecycle::partial(&mut sibling_stream).await?;
    let delayed_cancel = routes
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "CancelTask",
            json!({"id":id}),
        ))
        .await?;
    assert_eq!(delayed_cancel["error"]["code"], -32002);
    let live = routes
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "GetTask",
            json!({"id":successor["id"]}),
        ))
        .await?;
    assert_eq!(live["result"]["status"]["state"], "TASK_STATE_WORKING");
    routes.pair.a.h.llm.release.notify_one();
    finish(&mut successor_stream, &mut answer).await?;
    finish(&mut sibling_stream, &mut sibling_answer).await?;
    assert_eq!(tool_count(&routes.pair.a.h)?, 2);
    assert_eq!(routes.pair.a.h.llm.requests.lock().len(), 4);
    Ok(())
}
