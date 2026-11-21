//! Steered drains under owner loss, cancel and provider failure (issue
//! #2774): recovery must settle with the steered continuation's answer or
//! state, the drain gate must release on a cancel intent or a Cancel/Error
//! terminator, and a never-completing drain must fall through after the
//! bounded hold instead of wedging the task.

use super::*;

/// Admit the root task, steer a distinct follow-up into its live turn and
/// release the root round. Returns the task plus its durable allocation; on
/// return the steered row is outstanding with a continuation round in flight
/// (held under `Script::SteeringHeld`, provider-failed under
/// `Script::SteeringFail`).
async fn steered_drain(
    t: &TwoBackends,
) -> Result<(Value, harnx_a2a_server::store::FirstMessageReservation)> {
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&first)?.clone();
    requested(t, 1).await?;
    let mut steer = first_message();
    steer["message"]["messageId"] = json!("steer-drain");
    steer["message"]["contextId"] = task["contextId"].clone();
    steer["message"]["parts"][0]["text"] = json!("steered question");
    let steered = t.a.rpc("alice", "SendMessage", steer).await?;
    assert_eq!(
        result_task(&steered)?["id"],
        task["id"],
        "steering must join the running task"
    );
    // The root round's TurnEnd lands here; the steered row opens a held
    // continuation round, so the drain is outstanding from this point on.
    t.h.llm.release.notify_one();
    requested(t, 2).await?;
    let saved = reservation(t).await?;
    Ok((task, saved))
}

fn task_id(task: &Value) -> Result<String> {
    Ok(task["id"].as_str().context("task id")?.to_owned())
}

/// A fresh follow-up message on the same context under a new id.
fn follow_up_to(task: &Value, message_id: &str) -> Value {
    let mut next = first_message();
    next["message"]["messageId"] = json!(message_id);
    next["message"]["contextId"] = task["contextId"].clone();
    next
}

/// A follow-up after the steered drain must open a new task that completes;
/// returns that task.
async fn fresh_task_completes(t: &TwoBackends, task: &Value, message_id: &str) -> Result<Value> {
    let next_response =
        t.b.rpc("alice", "SendMessage", follow_up_to(task, message_id))
            .await?;
    let next_task = result_task(&next_response)?.clone();
    assert_ne!(next_task["id"], task["id"]);
    let next_id = task_id(&next_task)?;
    assert_eq!(
        terminal(t, &next_id).await?.task.status.state,
        TaskState::Completed
    );
    Ok(next_task)
}

/// Freeze A's publisher so only B's background sweep can settle the task.
/// The guard releases A when dropped, including on a failed assertion.
fn freeze_publisher(t: &TwoBackends) -> harnx_a2a_server::fault_injection::Pause {
    let hooks = t.a.backend.runner.fault_hooks();
    hooks.pause(
        Boundary::Publication,
        hooks.count(Boundary::Publication) + 1,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_loss_during_steered_drain_settles_with_the_steered_answer() -> Result<()> {
    let t = TwoBackends::start_script(Script::SteeringHeld).await?;
    let (task, saved) = steered_drain(&t).await?;
    let id = task_id(&task)?;
    let freeze = freeze_publisher(&t);
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    // The drain is outstanding: B's sweep must hold instead of archiving the
    // root round's answer while the steered row is still owed.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    if let Some(record) =
        t.b.backend
            .store
            .get_task_for_export(&t.h.export, &alice().into(), &id)
            .await?
    {
        assert!(
            !record.task.status.state.is_terminal(),
            "settled early on the root answer: {:?}",
            record.task.status
        );
    }
    // The live worker answers the steered row; the sweep settles through the
    // extended terminator with the continuation's answer, not the root's.
    t.h.llm.release.notify_one();
    let settled = terminal(&t, &id).await?;
    assert_eq!(settled.task.status.state, TaskState::Completed);
    // The status message carries the settled answer text.
    let text = settled
        .task
        .status
        .message
        .as_ref()
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(|part| match &part.content {
                    a2a_lf::PartContent::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default();
    assert_eq!(
        text, "steered answer",
        "recovery must archive the steered answer, not the root round's"
    );
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    drop(freeze);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_steered_continuation_settles_and_frees_the_context() -> Result<()> {
    let t = TwoBackends::start_script(Script::SteeringHeld).await?;
    let (task, _saved) = steered_drain(&t).await?;
    let id = task_id(&task)?;
    let canceled = t.b.rpc("alice", "CancelTask", json!({"id": id})).await?;
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(
        terminal(&t, &id).await?.task.status.state,
        TaskState::Canceled
    );
    // The interrupted steered row must not wedge the context: fresh work
    // opens a new task whose round streams past the held rounds.
    fresh_task_completes(&t, &task, "after-steered-cancel").await?;
    assert_eq!(t.h.llm.requests.lock().len(), 3);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_loss_with_cancel_intent_settles_without_waiting_out_the_drain_hold() -> Result<()> {
    let t = TwoBackends::start_script(Script::SteeringHeld).await?;
    let (task, saved) = steered_drain(&t).await?;
    let id = task_id(&task)?;
    let freeze = freeze_publisher(&t);
    // Durable cancel intent without the owner reacting: the recovery sweep
    // must not let the outstanding drain preempt the cancel.
    t.b.backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, &id)
        .await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let started = std::time::Instant::now();
    supervise(&t, &t.b);
    let settled = terminal(&t, &id).await?;
    assert_eq!(settled.task.status.state, TaskState::Canceled);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "cancel intent must release the drain gate promptly; took {:?}",
        started.elapsed()
    );
    drop(freeze);
    // Unwind the still-held continuation round the interrupt orphaned.
    t.h.llm.release.notify_one();
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_during_steered_continuation_settles_failed() -> Result<()> {
    let t = TwoBackends::start_script(Script::SteeringFail).await?;
    let (task, _saved) = steered_drain(&t).await?;
    // steered_drain's release completed the root round; the steered
    // continuation round failed at the provider, writing the durable Error
    // that ends the invocation.
    let id = task_id(&task)?;
    let settled = terminal(&t, &id).await?;
    assert_eq!(
        settled.task.status.state,
        TaskState::Failed,
        "the failed continuation must decide the task, not the root round's success"
    );
    // The context frees for the next task, which completes normally.
    fresh_task_completes(&t, &task, "after-steered-failure").await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hold_expires_and_settles_when_the_drain_never_completes() -> Result<()> {
    let t = TwoBackends::start_script(Script::SteeringHeld).await?;
    let (task, saved) = steered_drain(&t).await?;
    let id = task_id(&task)?;
    // Freeze A's publisher: A's own follow loop would settle on lease loss
    // instead of B's sweep, bypassing the drain hold under test.
    let freeze = freeze_publisher(&t);
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    let started = std::time::Instant::now();
    // The held continuation round never answers: recovery must hold the
    // drain window, then fall through to the owner-loss interrupt+settle
    // instead of wedging the task, its cancel confirmations and the context.
    let settled = tokio::time::timeout(std::time::Duration::from_secs(100), async {
        loop {
            if let Some(record) =
                t.b.backend
                    .store
                    .get_task_for_export(&t.h.export, &alice().into(), &id)
                    .await?
            {
                if record.task.status.state.is_terminal() {
                    return Ok::<_, anyhow::Error>(record);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("bounded drain-hold settle deadline")??;
    let elapsed = started.elapsed();
    assert_eq!(settled.task.status.state, TaskState::Failed);
    assert!(
        elapsed >= std::time::Duration::from_secs(25),
        "the sweep must hold the steered drain before falling through; settled in {elapsed:?}"
    );
    // Unwind the abandoned continuation round for a clean shutdown.
    t.h.llm.release.notify_one();
    drop(freeze);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}
