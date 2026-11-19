use super::*;
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::{SessionLogEntry, ToolOutput},
    tool::{SwitchAgentData, ToolCall},
};
use harnx_runtime::{
    fault_injection::simulate_t2_1_worker,
    interaction_checkpoint::{round_start_key, RoundStart, ROUND_START_BUCKET},
};

fn admission_closed(entries: &[(u64, SessionLogEntry)], invocation: &str, prompt: &str) -> bool {
    entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::AdmissionClosed {
                invocation_id,
                prompt_id,
                ..
            } if invocation_id == invocation || prompt_id == prompt
        )
    })
}

struct InteractionOwnedSetup {
    task_id: String,
    session: NatsSession,
    before: ContextSnapshot,
    tool_round_seq: u64,
    target_storage: Option<String>,
    hook: Pause,
    send: tokio::task::JoinHandle<Result<Value>>,
}

fn teardown(t: &TwoBackends, setup: InteractionOwnedSetup) {
    setup.send.abort();
    drop(setup.hook);
    t.b.backend.abort.set_ctrlc();
}

async fn start_active_task_session(
    t: &TwoBackends,
    boundary: Boundary,
) -> Result<(
    String,
    NatsSession,
    ContextSnapshot,
    Pause,
    tokio::task::JoinHandle<Result<Value>>,
)> {
    let mut hook = t.a.backend.runner.fault_hooks().pause(boundary, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, hook.reached()).await?;
    let saved = reservation(t).await?;
    let session = session_for(&t.b, &t.h, &saved.allocation.local_id).await?;
    let before =
        t.h.store
            .read_context(session.storage_key())
            .await?
            .context("context")?;
    remove_owner_lease(t, session.storage_key()).await?;
    Ok((saved.allocation.task_id, session, before, hook, send))
}

async fn setup_interaction_owned_with_call(
    t: &TwoBackends,
    call_name: &str,
    call_args: Value,
) -> Result<InteractionOwnedSetup> {
    let (task_id, session, before, hook, send) =
        start_active_task_session(t, Boundary::Activation).await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let tool_round_seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: format!("calling {call_name}"),
            thought: None,
            calls: vec![ToolCall {
                id: Some("call-1".into()),
                name: call_name.into(),
                arguments: call_args,
                thought_signature: None,
                reasoning_provenance: None,
            }],
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;

    Ok(InteractionOwnedSetup {
        task_id,
        session,
        before,
        tool_round_seq,
        target_storage: None,
        hook,
        send,
    })
}

async fn setup_interaction_owned(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    setup_interaction_owned_with_call(t, "test_tool", json!({})).await
}

async fn assert_interaction_owned_preserved(
    t: &TwoBackends,
    setup: &InteractionOwnedSetup,
) -> Result<()> {
    let current =
        t.h.store
            .read_context(setup.session.storage_key())
            .await?
            .context("context")?;
    let active = current
        .document
        .state
        .active
        .as_ref()
        .context("active task missing")?;

    assert_ne!(
        active.snapshot.task.status.state,
        TaskState::Failed,
        "task must not fail"
    );
    assert_eq!(
        active.snapshot.task.status.state,
        TaskState::Working,
        "task must remain working"
    );
    assert!(active.cancel.is_none(), "task must not have cancel intent");

    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    assert!(
        !entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })),
        "session log must not contain owner-loss Cancel"
    );

    let before_active = setup.before.document.state.active.as_ref().unwrap();
    let tick = ticket(&setup.before, setup.session.storage_key())?;
    assert!(
        !admission_closed(&entries, tick.invocation_id(), tick.prompt_id()),
        "fixed admission must not be closed"
    );

    assert_eq!(active.snapshot.task.id, before_active.snapshot.task.id);
    assert_eq!(
        active.snapshot.task.context_id,
        before_active.snapshot.task.context_id
    );
    assert_eq!(
        active.admission.invocation_id,
        before_active.admission.invocation_id
    );
    assert_eq!(
        active.admission.prompt_id,
        before_active.admission.prompt_id
    );
    assert_eq!(
        active.admission.closure_id,
        before_active.admission.closure_id
    );
    assert_eq!(
        active.admission.fixed_predecessor,
        before_active.admission.fixed_predecessor
    );
    assert_eq!(
        active.admission.prompt_sequence,
        before_active.admission.prompt_sequence
    );

    if let Some(target_storage) = &setup.target_storage {
        let target_log = NatsSessionLog::new(t.h.jetstream.clone(), target_storage);
        let target_entries = target_log.load_events_latest_required_async().await?;
        assert_eq!(target_entries.len(), 1, "target log must remain unchanged");
        assert!(matches!(
            &target_entries[0].1,
            SessionLogEntry::Message { .. }
        ));
    }

    Ok(())
}

async fn run_recovery(
    t: &TwoBackends,
    session: &NatsSession,
    task_id: &str,
    foreground: bool,
) -> Result<()> {
    if foreground {
        let response = t.b.rpc("alice", "GetTask", json!({"id": task_id})).await?;
        assert!(
            response.get("error").is_none(),
            "GetTask failed: {response}"
        );
        assert_eq!(
            response["result"]["status"]["state"], "TASK_STATE_WORKING",
            "foreground recovery must keep task in working state"
        );
    } else {
        let before_rev =
            t.h.store
                .read_context(session.storage_key())
                .await?
                .context("context")?
                .revision;
        supervise(t, &t.b);
        tokio::time::timeout(DEADLINE, async {
            loop {
                let context =
                    t.h.store
                        .read_context(session.storage_key())
                        .await?
                        .context("context")?;
                if context.revision > before_rev
                    && context.document.owner.is_none()
                    && !t
                        .b
                        .backend
                        .runner
                        .is_busy(&ContextKey::new(&t.h.export, session.session_id()))
                        .await
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .context("supervision finish deadline")??;
    }
    Ok(())
}

enum FixtureKind {
    Fixture1,
    Fixture2,
    Fixture3CorruptJson,
    Fixture3InvalidStream,
    Fixture4a,
    Fixture4b,
}

async fn build_fixture(t: &TwoBackends, kind: FixtureKind) -> Result<InteractionOwnedSetup> {
    match kind {
        FixtureKind::Fixture1 => fixture_1_setup(t).await,
        FixtureKind::Fixture2 => fixture_2_setup(t).await,
        FixtureKind::Fixture3CorruptJson => fixture_3_corrupt_json_setup(t).await,
        FixtureKind::Fixture3InvalidStream => fixture_3_invalid_stream_setup(t).await,
        FixtureKind::Fixture4a => fixture_4a_setup(t).await,
        FixtureKind::Fixture4b => fixture_4b_setup(t).await,
    }
}

async fn run_preserved_test(kind: FixtureKind, foreground: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let setup = build_fixture(&t, kind).await?;
    run_recovery(&t, &setup.session, &setup.task_id, foreground).await?;
    assert_interaction_owned_preserved(&t, &setup).await?;
    teardown(&t, setup);
    Ok(())
}

async fn get_or_create_checkpoint_kv(
    js: &async_nats::jetstream::Context,
) -> Result<async_nats::jetstream::kv::Store> {
    match js.get_key_value(ROUND_START_BUCKET).await {
        Ok(kv) => Ok(kv),
        Err(_) => Ok(js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: ROUND_START_BUCKET.to_string(),
                ..Default::default()
            })
            .await?),
    }
}

async fn put_stale_checkpoint(
    js: &async_nats::jetstream::Context,
    storage_key: &str,
    tool_round_seq: u64,
) -> Result<()> {
    let kv = get_or_create_checkpoint_kv(js).await?;
    let start = RoundStart {
        version: 1,
        session_storage_key: storage_key.to_string(),
        tool_round_seq,
        fence_token: 1,
    };
    let key = round_start_key(storage_key, tool_round_seq);
    kv.put(&key, serde_json::to_vec(&start)?.into()).await?;
    Ok(())
}

async fn append_handoff_tool_results(
    log: &NatsSessionLog,
    handoff_selection: Option<String>,
    target_session_id: Option<String>,
) -> Result<()> {
    log.append_event_async(&SessionLogEntry::ToolResults {
        results: vec![ToolOutput {
            id: Some("call-1".into()),
            name: "atlas_session_handoff".into(),
            output: json!({"status": "handoff"}),
            markdown: None,
            content: vec![],
            switch_agent: Some(SwitchAgentData {
                agent: "atlas".into(),
                prompt: "execute plan".into(),
                session_id: target_session_id,
                tool_call_id: Some("call-1".into()),
            }),
        }],
        handoff_selection,
        timestamp: None,
    })
    .await?;
    Ok(())
}

async fn fixture_1_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = setup_interaction_owned(t).await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    log.append_event_async(&SessionLogEntry::ToolCallCompleted {
        tool_round_seq: setup.tool_round_seq,
        tool_call_id: "call-1".into(),
        result: ToolOutput {
            id: Some("call-1".into()),
            name: "test_tool".into(),
            output: json!({"status": "ok"}),
            markdown: None,
            content: vec![],
            switch_agent: None,
        },
        private_snapshot: "snap-1".into(),
        fence_token: 1,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::InteractionRequested {
        tool_round_seq: setup.tool_round_seq,
        tool_call_id: "call-1".into(),
        interaction_id: "inter-1".into(),
        producer: "middleware:test".into(),
        step_path: vec![],
        key: "confirm-1".into(),
        kind: "confirm".into(),
        payload: json!({"message": "confirm action"}),
        summary: "confirm action".into(),
        chain_version: "v1".into(),
        checkpoint: "chk-1".into(),
        fence_token: 1,
    })
    .await?;
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_1_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture1, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_1_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture1, false).await
}

async fn fixture_2_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = setup_interaction_owned(t).await?;
    put_stale_checkpoint(
        &t.h.jetstream,
        setup.session.storage_key(),
        setup.tool_round_seq,
    )
    .await?;
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_2_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture2, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_2_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture2, false).await
}

async fn fixture_3_corrupt_json_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = setup_interaction_owned(t).await?;
    let kv = get_or_create_checkpoint_kv(&t.h.jetstream).await?;
    let key = round_start_key(setup.session.storage_key(), setup.tool_round_seq);
    kv.put(&key, b"corrupt-non-json-record".as_slice().into())
        .await?;
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_3_corrupt_json_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture3CorruptJson, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_3_corrupt_json_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture3CorruptJson, false).await
}

async fn fixture_3_invalid_stream_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = setup_interaction_owned(t).await?;
    t.h.jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: format!("KV_{ROUND_START_BUCKET}"),
            subjects: vec![format!("$KV.{ROUND_START_BUCKET}.>")],
            max_messages_per_subject: 0,
            ..Default::default()
        })
        .await?;
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_3_invalid_stream_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture3InvalidStream, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_3_invalid_stream_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture3InvalidStream, false).await
}

async fn fixture_4a_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = setup_interaction_owned(t).await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    append_handoff_tool_results(&log, Some("call-1".into()), None).await?;
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4a_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture4a, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4a_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture4a, false).await
}

async fn fixture_4b_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let mut setup = setup_interaction_owned(t).await?;
    let target_session_id = "target-1";
    let target_storage =
        harnx_core::session_identity::session_key(Some("atlas"), target_session_id);
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    append_handoff_tool_results(&log, Some("call-1".into()), Some(target_session_id.into()))
        .await?;
    let target_log = NatsSessionLog::new_with_replicas(t.h.jetstream.clone(), &target_storage, 1);
    target_log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("target-msg-1".into()),
            role: MessageRole::User,
            content: MessageContent::Text("execute plan".into()),
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;
    setup.target_storage = Some(target_storage);
    Ok(setup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4b_foreground() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture4b, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_fixture_4b_background_sweep() -> Result<()> {
    run_preserved_test(FixtureKind::Fixture4b, false).await
}

async fn run_absent_round_start_control_test(foreground: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let setup = setup_interaction_owned(&t).await?;
    if foreground {
        let response =
            t.b.rpc("alice", "GetTask", json!({"id": setup.task_id}))
                .await?;
        assert_eq!(
            response["result"]["status"]["state"], "TASK_STATE_FAILED",
            "unowned non-parked work settles as failed"
        );
        assert_eq!(
            response["result"]["status"]["message"]["parts"][0]["text"],
            "interrupted by server restart"
        );
    } else {
        supervise(&t, &t.b);
        let record = terminal(&t, &setup.task_id).await?;
        assert_eq!(record.task.status.state, TaskState::Failed);
        assert_eq!(
            record.task.status.message.as_ref().unwrap().parts[0].content,
            a2a_lf::PartContent::Text("interrupted by server restart".into())
        );
    }
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    assert!(
        entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })),
        "must emit Cancel on settle"
    );
    teardown(&t, setup);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_absent_round_start_foreground() -> Result<()> {
    run_absent_round_start_control_test(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_absent_round_start_background_sweep() -> Result<()>
{
    run_absent_round_start_control_test(false).await
}

async fn setup_stale_control(t: &TwoBackends) -> Result<(InteractionOwnedSetup, NatsSessionLog)> {
    let setup = setup_interaction_owned(t).await?;
    put_stale_checkpoint(
        &t.h.jetstream,
        setup.session.storage_key(),
        setup.tool_round_seq,
    )
    .await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    Ok((setup, log))
}

async fn setup_terminal_control(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let (setup, log) = setup_stale_control(t).await?;
    append_handoff_tool_results(&log, None, None).await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("assistant-done".into()),
        role: MessageRole::Assistant,
        content: MessageContent::Text("All tasks done.".into()),
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: setup.tool_round_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await?;
    Ok(setup)
}

async fn setup_cancelled_control(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let (setup, _log) = setup_stale_control(t).await?;
    let prompt_seq = setup
        .before
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .admission
        .prompt_sequence;
    setup
        .session
        .interrupt_prompt(
            prompt_seq.context("prompt_sequence")?,
            "user requested cancellation",
        )
        .await?;
    t.h.store
        .request_task_cancel(setup.session.storage_key(), &setup.task_id)
        .await?;
    Ok(setup)
}

enum ControlKind {
    Terminal,
    Cancelled,
}

async fn run_settled_control_test(kind: ControlKind, foreground: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let (setup, expected_state, expected_rpc) = match kind {
        ControlKind::Terminal => (
            setup_terminal_control(&t).await?,
            TaskState::Completed,
            "TASK_STATE_COMPLETED",
        ),
        ControlKind::Cancelled => (
            setup_cancelled_control(&t).await?,
            TaskState::Canceled,
            "TASK_STATE_CANCELED",
        ),
    };
    if foreground {
        let response =
            t.b.rpc("alice", "GetTask", json!({"id": setup.task_id}))
                .await?;
        assert_eq!(
            response["result"]["status"]["state"], expected_rpc,
            "settled work state mismatch"
        );
    } else {
        supervise(&t, &t.b);
        let record = terminal(&t, &setup.task_id).await?;
        assert_eq!(record.task.status.state, expected_state);
    }
    teardown(&t, setup);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_terminal_foreground() -> Result<()> {
    run_settled_control_test(ControlKind::Terminal, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_terminal_background_sweep() -> Result<()> {
    run_settled_control_test(ControlKind::Terminal, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_cancelled_foreground() -> Result<()> {
    run_settled_control_test(ControlKind::Cancelled, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_hitl_interaction_owned_control_cancelled_background_sweep() -> Result<()> {
    run_settled_control_test(ControlKind::Cancelled, false).await
}

mod assertions;
mod fixtures;
mod mixed_version;
