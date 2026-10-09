//! Blocking wait fallback and persistence failures use a real isolated broker.
use crate::support::{alice, Harness, Script, DEADLINE};
use a2a_lf::{
    Message, Part, Role, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
};
use anyhow::{Context, Result};
use harnx_a2a_server::runner::A2aEvent;
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    identity::{Identity, Principal},
    input_map::InputLimits,
    store::{new_task_id, TaskSeed},
};
use harnx_runtime::SessionActivationRoute;
use std::sync::Arc;

fn handler(h: &Harness) -> HarnxHandler {
    HarnxHandler::new(
        h.export.clone(),
        Identity::default(),
        Arc::new(Backend::new(
            h.runner.clone(),
            h.store.clone(),
            BackendConfig {
                config: h.config.clone(),
                route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            },
        )),
        InputLimits::default(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_terminal_without_runner_establishes_watch_and_reconciles_orphan() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let record = h
        .store
        .create_task(
            session.storage_key(),
            TaskSeed {
                task: Task {
                    id: new_task_id(session.session_id()),
                    context_id: session.session_id().into(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                },
                user_msg_id: String::new(),
                user_msg_seq: 0,
                execution_id: String::new(),
            },
        )
        .await?;
    let handler = handler(&h);
    let failed = tokio::time::timeout(
        DEADLINE,
        handler.wait_terminal(&alice().into(), &record.task.id),
    )
    .await??;
    assert_eq!(failed.task.status.state, TaskState::Failed);
    assert!(h.logs.text().contains("waiting for task via KV watch"));
    assert_eq!(
        h.task(&record.task.id).await?.task.status.state,
        TaskState::Failed
    );
    // The same direct API still checks ownership and the already-terminal race.
    let denied = handler
        .wait_terminal(&Principal::User("bob".into()).into(), &record.task.id)
        .await
        .unwrap_err();
    assert_eq!(denied.code, -32001);
    assert_eq!(
        handler
            .wait_terminal(&alice().into(), &record.task.id)
            .await?
            .task
            .status
            .state,
        TaskState::Failed
    );
    Ok(())
}

async fn wait_for_first_artifact(
    events: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(
                events.recv().await?.response,
                StreamResponse::ArtifactUpdate(_)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await?
}

async fn wait_for_log(h: &Harness, needle: &str) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        while !h.logs.text().contains(needle) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn set_stream_max_message_size(h: &Harness, max_size: i32) -> Result<i32> {
    let mut config = h.metadata.kv_store().status().await?.info.config;
    let original = config.max_message_size;
    config.max_message_size = max_size;
    h.jetstream.update_stream(&config).await?;
    Ok(original)
}

async fn restore_stream_max_message_size(h: &Harness, original_max: i32) -> Result<()> {
    let mut config = h.metadata.kv_store().status().await?.info.config;
    config.max_message_size = original_max;
    h.jetstream.update_stream(&config).await?;
    Ok(())
}

fn apply_artifact_update(accum: &mut String, update: &TaskArtifactUpdateEvent) {
    if update.append != Some(true) {
        accum.clear();
    }
    for part in &update.artifact.parts {
        if let a2a_lf::PartContent::Text(text) = &part.content {
            accum.push_str(text);
        }
    }
}

fn apply_stream_event(text: &mut String, event: &A2aEvent) -> bool {
    if let StreamResponse::ArtifactUpdate(update) = &event.response {
        apply_artifact_update(text, update);
    }
    event.is_terminal()
}

async fn assemble_artifact_until_terminal(
    events: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
    initial: &str,
) -> Result<String> {
    let mut final_artifact = initial.to_string();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let event = events.recv().await?;
            if apply_stream_event(&mut final_artifact, &event) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    Ok(final_artifact)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_terminal_returns_error_after_bounded_terminal_persistence_failure() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h
        .send(
            &session,
            Message::new(Role::User, vec![Part::text("persistence failure")]),
        )
        .await?;
    wait_for_first_artifact(&mut started.events).await?;
    let handler = handler(&h);
    let owner = alice().into();
    let mut waiting = Box::pin(handler.wait_terminal(&owner, &started.snapshot.task.id));
    // Wait for the actual local-watch branch before making persistence fail.
    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            result = &mut waiting => panic!("turn completed before release: {result:?}"),
            _ = wait_for_log(&h, "waiting on local task completion") => {}
        }
    })
    .await?;
    let original_max = set_stream_max_message_size(&h, 1).await?;
    h.llm.release.notify_one();
    let error = tokio::time::timeout(DEADLINE, &mut waiting)
        .await
        .context("blocking waiter hung after done")?
        .unwrap_err();
    assert_eq!(error.code, -32603);
    assert_eq!(
        h.logs
            .text()
            .matches("A2A terminal persistence failed")
            .count(),
        4
    );
    assert!(h
        .logs
        .text()
        .contains("A2A terminal persistence retries exhausted"));
    assert_eq!(
        h.task(&started.snapshot.task.id).await?.task.status.state,
        TaskState::Working
    );
    // Recovery resolves the durable worker answer before deciding to fail an
    // abandoned owner. No prompt is replayed when metadata storage returns.
    restore_stream_max_message_size(&h, original_max).await?;
    let failed = handler
        .wait_terminal(&owner, &started.snapshot.task.id)
        .await?;
    assert_eq!(failed.task.status.state, TaskState::Completed);
    assert_eq!(
        serde_json::to_value(&failed.task)?["status"]["message"]["parts"][0]["text"],
        "Hello world"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_persistence_retry_recovers_full_artifact_after_storage_returns() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h
        .send(
            &session,
            Message::new(
                Role::User,
                vec![Part::text("transient persistence failure")],
            ),
        )
        .await?;
    wait_for_first_artifact(&mut started.events).await?;
    let handler = handler(&h);
    let id = started.snapshot.task.id.clone();
    let waiter = tokio::spawn(async move { handler.wait_terminal(&alice().into(), &id).await });
    wait_for_log(&h, "waiting on local task completion").await?;
    let original_max = set_stream_max_message_size(&h, 1).await?;
    h.llm.release.notify_one();
    wait_for_log(&h, "A2A terminal persistence failed").await?;
    restore_stream_max_message_size(&h, original_max).await?;
    let completed = tokio::time::timeout(DEADLINE, waiter).await???;
    // A transient frontend write failure can't replace the durable worker answer.
    assert_eq!(completed.task.status.state, TaskState::Completed);
    let serialized = serde_json::to_value(&completed.task)?;
    assert_eq!(
        serialized["status"]["message"]["parts"][0]["text"],
        "Hello world"
    );
    assert_eq!(
        serialized["artifacts"][0]["parts"][0]["text"],
        "Hello world"
    );
    assert!(!h
        .logs
        .text()
        .contains("A2A terminal persistence retries exhausted"));
    // The first artifact was consumed before the fault was introduced.
    let final_artifact = assemble_artifact_until_terminal(&mut started.events, "Hello ").await?;
    assert_eq!(
        final_artifact, "Hello world",
        "retry must preserve exact artifact assembly before terminal status"
    );
    Ok(())
}
