use super::*;
use crate::runner_test_support::{alice, Harness, Script, DEADLINE};
use a2a_lf::{Part, Role};

fn prune_idle_with_running_turn(
    runner: &Runner,
    export: &Export,
    active_key: &ContextKey,
    active: &std::sync::Weak<ContextSlot>,
) -> (ContextKey, Arc<ContextSlot>) {
    let idle_key = ContextKey::new(export, "idle");
    let idle_slot = runner.slot(&idle_key);
    let idle = Arc::downgrade(&idle_slot);
    drop(idle_slot);
    assert_eq!(idle.strong_count(), 1);
    let probe_key = ContextKey::new(export, "probe");
    let probe = runner.slot(&probe_key);
    {
        let contexts = runner.contexts.lock();
        assert_eq!(contexts.len(), 2);
        assert!(
            !contexts.contains_key(&idle_key),
            "idle registry entry must be pruned"
        );
        assert!(
            active.ptr_eq(&Arc::downgrade(contexts.get(active_key).unwrap())),
            "pruning must preserve the running turn's original slot"
        );
        assert!(Arc::ptr_eq(contexts.get(&probe_key).unwrap(), &probe));
    }
    assert!(
        idle.upgrade().is_none(),
        "idle slot must be freed, not only hidden"
    );

    (probe_key, probe)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_slot_pruning_retains_running_turn_and_removes_idle_slots() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let started = h
        .send(
            &session,
            Message::new(Role::User, vec![Part::text("Hello, agent")]),
        )
        .await?;
    tokio::time::timeout(DEADLINE, h.llm.requested.notified()).await?;
    assert_eq!(
        h.task(&started.snapshot.task.id).await?.task.status.state,
        TaskState::Working
    );
    let active_key = ContextKey::new(&h.export, session.session_id());
    // A Weak observer doesn't change the strong-count pruning decision.
    let active = Arc::downgrade(h.runner.contexts.lock().get(&active_key).unwrap());
    assert!(
        active.strong_count() > 1,
        "RunningTurn must retain its delivery slot"
    );

    let (probe_key, probe) =
        prune_idle_with_running_turn(&h.runner, &h.export, &active_key, &active);

    h.llm.release.notify_one();
    tokio::time::timeout(DEADLINE, async {
        loop {
            if h.task(&started.snapshot.task.id).await?.task.status.state == TaskState::Completed
                && active.strong_count() == 1
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let next_key = ContextKey::new(&h.export, "next");
    let next = h.runner.slot(&next_key);
    {
        let contexts = h.runner.contexts.lock();
        assert_eq!(contexts.len(), 2);
        assert!(
            !contexts.contains_key(&active_key),
            "completed turn's idle slot must be pruned on the next lookup"
        );
        assert!(
            Arc::ptr_eq(contexts.get(&probe_key).unwrap(), &probe),
            "concurrent caller reference must also retain its slot"
        );
        assert!(Arc::ptr_eq(contexts.get(&next_key).unwrap(), &next));
    }
    assert!(active.upgrade().is_none());
    drop((probe, next));
    h.runner.shutdown().await;
    Ok(())
}
