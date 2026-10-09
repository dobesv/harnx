use crate::support::{alice, Harness, Script, DEADLINE};
use a2a_lf::{Message, Part, PartContent, Role, StreamResponse, Task, TaskState, TaskStatus};
use anyhow::{Context, Result};
use harnx_a2a_server::store::{TaskAccess, TaskSeed, TaskVersion};
use harnx_a2a_server::{
    runner::{A2aEvent, ContextKey, RunnerError},
    store::{new_task_id, StoreError},
};
use harnx_core::{session::SessionLogEntry, session_identity::session_key};
use harnx_runtime::{nats_session_log::NatsSessionLog, nats_session_metadata::session_properties};
use serde_json::json;
use tokio::sync::broadcast;

fn prompt() -> Message {
    Message::new(Role::User, vec![Part::text("test runner")])
}
fn text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
async fn event(rx: &mut broadcast::Receiver<A2aEvent>) -> Result<A2aEvent> {
    tokio::time::timeout(DEADLINE, rx.recv())
        .await
        .context("runner event deadline")?
        .context("runner stream closed/lagged")
}
async fn first_artifact(rx: &mut broadcast::Receiver<A2aEvent>) -> Result<A2aEvent> {
    loop {
        let next = event(rx).await?;
        if matches!(next.response, StreamResponse::ArtifactUpdate(_)) {
            return Ok(next);
        }
        anyhow::ensure!(
            !next.is_terminal(),
            "turn ended before streaming artifact: {next:?}"
        );
    }
}
async fn terminal(rx: &mut broadcast::Receiver<A2aEvent>) -> Result<A2aEvent> {
    loop {
        let next = event(rx).await?;
        if next.is_terminal() {
            return Ok(next);
        }
    }
}

fn assert_admitted_snapshot(record: &harnx_a2a_server::store::TaskRecord, message: Message) {
    for (field, valid) in [
        ("user_msg_id", !record.user_msg_id.is_empty()),
        ("user_msg_seq", record.user_msg_seq > 0),
        ("execution_id", !record.execution_id.is_empty()),
    ] {
        assert!(valid, "invalid admission field: {field}");
    }
    assert_eq!(record.task.history.as_ref().unwrap(), &[message]);
}

async fn assert_working_update(
    h: &Harness,
    started: &mut harnx_a2a_server::runner::StartTurnResult,
) -> Result<()> {
    let working = event(&mut started.events).await?;
    let StreamResponse::StatusUpdate(update) = &working.response else {
        panic!("first update must be Working")
    };
    assert_eq!(update.status.state, TaskState::Working);
    assert!(working.sequence <= started.snapshot.stream_seq);
    assert_eq!(
        h.task(&started.snapshot.task.id).await?.task.status.state,
        TaskState::Working
    );
    Ok(())
}

async fn assert_first_chunk(
    h: &Harness,
    started: &mut harnx_a2a_server::runner::StartTurnResult,
) -> Result<(String, A2aEvent)> {
    let first = first_artifact(&mut started.events).await?;
    let assembled = match &first.response {
        StreamResponse::ArtifactUpdate(update) => {
            assert_eq!(update.artifact.artifact_id, "answer");
            assert_eq!(update.append, Some(false));
            assert_eq!(update.last_chunk, Some(false));
            text(&update.artifact.parts)
        }
        _ => unreachable!(),
    };
    assert_eq!(assembled, "Hello ");
    // Every emitted chunk commits its snapshot/cursor before transport publication.
    assert_eq!(
        h.task(&started.snapshot.task.id).await?.revision,
        started.snapshot.revision + 1
    );
    assert!(first.sequence > started.snapshot.stream_seq);
    Ok((assembled, first))
}

fn append_artifact(assembled: &mut String, update: &a2a_lf::TaskArtifactUpdateEvent) {
    if update.append != Some(true) {
        assembled.clear();
    }
    assembled.push_str(&text(&update.artifact.parts));
}

async fn assert_remaining_chunks(
    h: &Harness,
    started: &mut harnx_a2a_server::runner::StartTurnResult,
    first: &A2aEvent,
    mut assembled: String,
) -> Result<(String, A2aEvent)> {
    h.llm.release.notify_one();
    let mut last_revision = first.sequence;
    let mut last_chunk = false;
    let finished = loop {
        let next = event(&mut started.events).await?;
        assert!(next.sequence > last_revision);
        last_revision = next.sequence;

        if let StreamResponse::ArtifactUpdate(update) = &next.response {
            assert_eq!(update.artifact.artifact_id, "answer");
            append_artifact(&mut assembled, update);
            last_chunk |= update.last_chunk == Some(true);
        }
        if next.is_terminal() {
            break next;
        }
    };
    assert!(last_chunk);
    assert_eq!(assembled, "Hello world");
    Ok((assembled, finished))
}

fn assert_completed_summary(
    persisted: &harnx_a2a_server::store::TaskRecord,
    assembled: &str,
    finished: &A2aEvent,
) {
    let StreamResponse::StatusUpdate(update) = &finished.response else {
        panic!("terminal status required")
    };
    assert_eq!(persisted.task.status, update.status);
    assert_eq!(persisted.task.status.state, TaskState::Completed);
    assert_eq!(
        text(&persisted.task.status.message.as_ref().unwrap().parts),
        assembled
    );
}

async fn assert_completed_snapshot(
    h: &Harness,
    id: &str,
    assembled: String,
    finished: A2aEvent,
) -> Result<()> {
    let persisted = h.task(id).await?;
    assert_completed_summary(&persisted, &assembled, &finished);
    assert_eq!(text(&persisted.task.artifacts.unwrap()[0].parts), assembled);
    let StreamResponse::StatusUpdate(update) = finished.response else {
        unreachable!()
    };
    assert_eq!(persisted.task.status, update.status);
    // Completion won: a late cancel reads the real state, never sends a remote cancel.
    assert_eq!(
        h.runner
            .cancel_task(&h.export, &alice().into(), &persisted.task.id)
            .await?
            .task
            .status
            .state,
        TaskState::Completed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_turn_streams_artifacts_completes_and_persists() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let message = prompt();
    let mut started = h.send(&session, message.clone()).await?;
    assert_admitted_snapshot(&started.snapshot, message);
    assert_working_update(&h, &mut started).await?;
    let (assembled, first) = assert_first_chunk(&h, &mut started).await?;
    let (assembled, finished) =
        assert_remaining_chunks(&h, &mut started, &first, assembled).await?;
    assert_completed_snapshot(&h, &started.snapshot.task.id, assembled, finished).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_cancel_mid_stream_is_canceled() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    first_artifact(&mut started.events).await?;
    let canceled = tokio::time::timeout(
        DEADLINE,
        h.runner
            .cancel_task(&h.export, &alice().into(), &started.snapshot.task.id),
    )
    .await
    .context("cancel deadline")??;
    assert_eq!(canceled.task.status.state, TaskState::Canceled);
    let finished = terminal(&mut started.events).await?;
    let StreamResponse::StatusUpdate(update) = finished.response else {
        panic!("terminal status required")
    };
    assert_eq!(canceled.task.status, update.status);
    assert_eq!(
        h.task(&canceled.task.id).await?.task.status.state,
        TaskState::Canceled
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_confirmation_tool_is_denied_and_turn_terminates() -> Result<()> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    terminal(&mut started.events).await?;
    let task = h.task(&started.snapshot.task.id).await?;
    assert_eq!(task.task.status.state, TaskState::Completed);
    let entries = NatsSessionLog::new_with_replicas(h.jetstream.clone(), session.storage_key(), 1)
        .load_events_async()
        .await?;
    // Routed confirmation uses a per-turn callback, not durable HITL entries.
    // Its info log proves the ask reached our deny handler (rather than a hook
    // startup failure or a deny decision made by the hook itself).
    assert!(h.logs.text().contains("INFO harnx_a2a_server::runner: A2A tool confirmation denied tool=target_session_handoff tool_call_id=Some(\"runner-handoff\")"), "{}", h.logs.text());
    assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { results, .. }
        if results.iter().any(|result| result.name == "target_session_handoff" && result.output["blocked_by_hook"] == json!(true)))));
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::HandoffCommitted { .. })));
    let requests = h.llm.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("blocked_by_hook")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_jira_assignment_data_reaches_recorded_llm_request() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/jira/assignment.json"))?;
    let message: Message = serde_json::from_value(fixture["params"]["message"].clone())?;
    let expected = harnx_a2a_server::input_map::message_to_input(&message, Default::default())?;
    let mut started = h.send(&session, message.clone()).await?;
    first_artifact(&mut started.events).await?;
    let request = h.llm.requests.lock()[0].clone();
    let received = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .context("recorded user prompt")?["content"]
        .as_str()
        .context("prompt text")?;
    assert_eq!(received, expected.text());
    for expected_part in [
        "--- A2A data part (mediaType: application/json) ---\n```json\n",
        "\"invocationType\": \"ISSUE_ASSIGNMENT\"",
        "\"key\": \"AW26-11\"",
    ] {
        assert!(received.contains(expected_part));
    }
    assert_eq!(started.snapshot.task.history.as_ref().unwrap(), &[message]);
    h.llm.release.notify_one();
    terminal(&mut started.events).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_busy_context_rejects_second_start_and_stale_cancel() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let mut first = h.send(&session, prompt()).await?;
    first_artifact(&mut first.events).await?;
    let error = h
        .send(&session, prompt())
        .await
        .err()
        .context("second start must fail")?;
    assert_eq!(
        error.downcast_ref::<RunnerError>(),
        Some(&RunnerError::Busy)
    );
    assert_eq!(
        crate::support::list_all_tasks_for_test(
            &h.store,
            &h.export,
            &alice(),
            session.session_id()
        )
        .await?
        .unwrap()
        .len(),
        1
    );
    h.llm.release.notify_one();
    terminal(&mut first.events).await?;
    let mut second = h.send(&session, prompt()).await?;
    first_artifact(&mut second.events).await?;
    let late = h
        .runner
        .cancel_task(&h.export, &alice().into(), &first.snapshot.task.id)
        .await?;
    assert_eq!(late.task.status.state, TaskState::Completed);
    assert!(
        h.runner
            .is_busy(&ContextKey::new(&h.export, session.session_id()))
            .await
    );
    assert_eq!(
        h.task(&second.snapshot.task.id).await?.task.status.state,
        TaskState::Working
    );
    h.llm.release.notify_one();
    terminal(&mut second.events).await?;
    assert_eq!(
        h.task(&second.snapshot.task.id).await?.task.status.state,
        TaskState::Completed
    );
    Ok(())
}

async fn admit_orphan_task(
    h: &Harness,
    session: &harnx_runtime::NatsSession,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    // Simulate a crashed frontend after durable admission, with a live worker.
    let message = prompt();
    let input = harnx_a2a_server::input_map::message_to_input(&message, Default::default())?;
    let task = Task {
        id: new_task_id(session.session_id()),
        context_id: session.session_id().into(),
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: Some(vec![message]),
        metadata: None,
    };
    let record = h
        .store
        .create_task(
            session.storage_key(),
            TaskSeed {
                task,
                user_msg_id: String::new(),
                user_msg_seq: 0,
                execution_id: String::new(),
            },
        )
        .await?;
    let admitted = session
        .clone()
        .with_external_admission()
        .admit_input(&input, None)
        .await?;
    let record = h
        .store
        .update_admission(
            TaskVersion {
                storage_key: session.storage_key(),
                task_id: &record.task.id,
                revision: record.revision,
            },
            &admitted,
        )
        .await?;
    Ok(record)
}

async fn activate_orphan_worker(h: &Harness, session: &harnx_runtime::NatsSession) -> Result<()> {
    // Admission doesn't activate. Durable activation models the old frontend
    // exiting after publishing work, before observing its final result.
    harnx_runtime::nats_worker::publish_session_activate(
        &h.jetstream,
        "runner",
        &harnx_runtime::nats_worker::SessionActivate::new(session.storage_key()),
        1,
    )
    .await?;
    tokio::time::timeout(DEADLINE, h.llm.requested.notified())
        .await
        .context("orphan worker didn't start")?;
    Ok(())
}

fn assert_orphan_failure(failed: &harnx_a2a_server::store::TaskRecord) {
    assert_eq!(failed.task.status.state, TaskState::Failed);
    assert_eq!(
        text(&failed.task.status.message.as_ref().unwrap().parts),
        "interrupted by server restart"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_orphan_reconciliation_marks_failed() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let record = admit_orphan_task(&h, &session).await?;
    activate_orphan_worker(&h, &session).await?;
    let failed = h
        .runner
        .reconcile_orphan(
            TaskAccess {
                export: &h.export,
                owner: &alice().into(),
                task_id: &record.task.id,
            },
            &session,
        )
        .await?;
    assert_orphan_failure(&failed);
    assert_eq!(h.task(&record.task.id).await?.revision, failed.revision);
    assert_eq!(
        h.runner
            .reconcile_orphan(
                TaskAccess {
                    export: &h.export,
                    owner: &alice().into(),
                    task_id: &record.task.id
                },
                &session
            )
            .await?
            .revision,
        failed.revision
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_new_session_stamps_user_binding_and_foreign_resume_creates_nothing() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    assert!(!session.session_id().contains('.'));
    let metadata = h.metadata.get(session.storage_key()).await?.unwrap();
    assert_eq!(
        session_properties(&metadata.metadata)?.text("user_id"),
        Some("alice")
    );
    let binding = h.store.get_binding(session.storage_key()).await?.unwrap();
    assert_eq!(binding.export, h.export.public_name);
    assert_eq!(binding.owner.as_deref(), Some("alice"));
    let foreign = h
        .session(
            Some(session.session_id()),
            &harnx_a2a_server::identity::Principal::User("bob".into()),
        )
        .await;
    assert_eq!(
        foreign.err().unwrap().downcast_ref::<StoreError>(),
        Some(&StoreError::NotFound)
    );
    let missing = "client-made-id";
    let error = h.session(Some(missing), &alice()).await.err().unwrap();
    assert_eq!(
        error.downcast_ref::<StoreError>(),
        Some(&StoreError::NotFound)
    );
    assert!(h
        .metadata
        .get(&session_key(Some(&h.export.agent), missing))
        .await?
        .is_none());
    assert_eq!(
        h.metadata
            .get(session.storage_key())
            .await?
            .unwrap()
            .revision,
        metadata.revision
    );
    let resumed = h.session(Some(session.session_id()), &alice()).await?;
    assert_eq!(resumed.storage_key(), session.storage_key());
    let mut started = h.send(&resumed, prompt()).await?;
    first_artifact(&mut started.events).await?;
    h.llm.release.notify_one();
    terminal(&mut started.events).await?;
    Ok(())
}

async fn resumed_answer(
    events: &mut broadcast::Receiver<A2aEvent>,
    revision: u64,
    mut assembled: String,
) -> Result<String> {
    loop {
        let next = event(events).await?;
        if next.sequence <= revision {
            continue;
        }
        if let StreamResponse::ArtifactUpdate(update) = &next.response {
            append_artifact(&mut assembled, update);
        }
        if next.is_terminal() {
            break;
        }
    }
    Ok(assembled)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_subscribe_snapshot_then_deltas_survives_disconnect() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    first_artifact(&mut started.events).await?;
    let id = started.snapshot.task.id.clone();
    drop(started);
    let mut sub = h.runner.subscribe(&h.export, &alice().into(), &id).await?;
    let assembled = text(&sub.snapshot.task.artifacts.as_ref().unwrap()[0].parts);
    let revision = sub.snapshot.stream_seq;
    h.llm.release.notify_one();
    let assembled = resumed_answer(&mut sub.events, revision, assembled).await?;
    assert_eq!(assembled, "Hello world");
    assert_eq!(h.task(&id).await?.task.status.state, TaskState::Completed);
    assert_eq!(
        h.runner
            .subscribe(&h.export, &alice().into(), &id)
            .await
            .err()
            .unwrap()
            .downcast_ref::<RunnerError>(),
        Some(&RunnerError::Terminal)
    );
    Ok(())
}

fn assert_failed_snapshot(task: &harnx_a2a_server::store::TaskRecord, finished: &A2aEvent) {
    let StreamResponse::StatusUpdate(update) = &finished.response else {
        panic!("terminal status required")
    };
    assert_eq!(task.task.status, update.status);
    assert_eq!(task.task.status.state, TaskState::Failed);
    assert_eq!(
        text(&task.task.status.message.as_ref().unwrap().parts),
        "agent turn failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_failure_message_is_sanitized_and_persisted() -> Result<()> {
    let h = Harness::start(Script::Fail).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    let finished = terminal(&mut started.events).await?;
    let task = h.task(&started.snapshot.task.id).await?;
    assert_failed_snapshot(&task, &finished);
    assert!(!serde_json::to_string(&task)?.contains("secret-token"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_lagging_subscriber_errors_without_stopping_turn() -> Result<()> {
    let h = Harness::start(Script::Many).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    first_artifact(&mut started.events).await?;
    let mut slow = h
        .runner
        .subscribe(&h.export, &alice().into(), &started.snapshot.task.id)
        .await?;
    for _ in 1..harnx_a2a_server::runner::EVENT_CAPACITY + 5 {
        h.llm.release.notify_one();
        first_artifact(&mut started.events).await?;
    }
    assert!(matches!(
        slow.events.recv().await,
        Err(broadcast::error::RecvError::Lagged(_))
    ));
    drop(slow);
    h.llm.release.notify_one();
    terminal(&mut started.events).await?;
    let task = h.task(&started.snapshot.task.id).await?;
    assert_eq!(task.task.status.state, TaskState::Completed);
    assert_eq!(
        text(&task.task.artifacts.unwrap()[0].parts),
        "x".repeat(harnx_a2a_server::runner::EVENT_CAPACITY + 5)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_durable_publication_keeps_midstream_snapshots_exact() -> Result<()> {
    check_stream_snapshots(Script::Many, 1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_large_stream_commits_each_coalesced_snapshot() -> Result<()> {
    check_stream_snapshots(Script::Large, 1024).await
}

async fn check_stream_snapshots(script: Script, chunk_size: usize) -> Result<()> {
    let h = Harness::start(script).await?;
    let session = h.session(None, &alice()).await?;
    let mut started = h.send(&session, prompt()).await?;
    let initial_revision = started.snapshot.revision;
    first_artifact(&mut started.events).await?;
    let id = &started.snapshot.task.id;
    let mut sub = h.runner.subscribe(&h.export, &alice().into(), id).await?;
    let mut assembled = text(&sub.snapshot.task.artifacts.as_ref().unwrap()[0].parts);
    let mut sequence = sub.snapshot.stream_seq;
    assert_eq!(assembled, "x".repeat(chunk_size));
    let chunks = harnx_a2a_server::runner::EVENT_CAPACITY + 5;
    for count in 2..=chunks {
        h.llm.release.notify_one();
        let delta = first_artifact(&mut started.events).await?;
        let streamed = event(&mut sub.events).await?;
        assert_eq!(delta.sequence, streamed.sequence);
        assert!(streamed.sequence > sequence);
        sequence = streamed.sequence;
        let StreamResponse::ArtifactUpdate(update) = &streamed.response else {
            panic!("expected incremental artifact");
        };
        append_artifact(&mut assembled, update);
        assert_eq!(assembled, "x".repeat(count * chunk_size));
        // Repeated late subscribers must see every already-published delta once.
        let late = h.runner.subscribe(&h.export, &alice().into(), id).await?;
        assert_eq!(late.snapshot.stream_seq, sequence);
        assert_eq!(
            text(&late.snapshot.task.artifacts.as_ref().unwrap()[0].parts),
            assembled
        );
        let durable = h.task(id).await?;
        assert_eq!(durable.stream_seq, sequence);
        assert_eq!(durable.revision, initial_revision + count as u64);
    }
    h.llm.release.notify_one();
    let assembled = resumed_answer(&mut sub.events, sequence, assembled).await?;
    let durable = h.task(id).await?;
    assert_eq!(assembled, "x".repeat(chunks * chunk_size));
    assert_eq!(
        text(&durable.task.artifacts.as_ref().unwrap()[0].parts),
        assembled
    );
    assert_eq!(durable.task.status.state, TaskState::Completed);
    assert_eq!(durable.revision, initial_revision + chunks as u64 + 2);
    assert_eq!(durable.stream_seq, sequence + 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_new_context_first_task_does_not_migrate_legacy_keys() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let key = session.storage_key();
    let (index, _) = h
        .metadata
        .get_a2a_task_index(key)
        .await?
        .expect("bound index");
    assert!(index.entries.is_empty());

    // A legacy scan would parse this record and fail. New contexts must not scan it.
    let hidden_id = new_task_id(session.session_id());
    let (_, uuid) = harnx_a2a_server::store::parse_task_id(&hidden_id)?;
    let record_key = harnx_runtime::nats_session_metadata::a2a_task_key(key, uuid);
    h.metadata
        .kv_store()
        .put(record_key, "invalid legacy json".into())
        .await?;
    let started = h.send(&session, prompt()).await?;
    let (index, _) = h.metadata.get_a2a_task_index(key).await?.unwrap();
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].task_id, started.snapshot.task.id);
    assert!(!h.logs.text().contains("migrated legacy tasks to index"));
    h.runner.shutdown().await;
    Ok(())
}
