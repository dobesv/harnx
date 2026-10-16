//! Real-broker replay: lose runtime state after append, then reload raw receipts.
use super::*;
use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use harnx_core::session::Session;
use harnx_core::{
    message::MessageContent,
    timing_identity::{MilestoneCoverage, TimingReceipt},
};
use harnx_runtime::{
    client::Client,
    config::GlobalConfig,
    nats_session_metadata::{
        AdmissionAuthority, CallTimeoutOverride, InvocationAdmission, InvocationEdgeKind,
        InvocationIdentity, RunIdentity, RunLimitsRecord,
    },
    test_utils::MockClient,
};
use std::sync::Mutex;
use tokio_util::task::AbortOnDropHandle;

#[derive(Default)]
struct Requests {
    wire: Mutex<Vec<Vec<String>>>,
    suffixes: Mutex<Vec<Option<TimingReceipt>>>,
    limits: Mutex<Vec<Option<RunLimitsRecord>>>,
    skip_modes: Mutex<Vec<bool>>,
    entered: Notify,
    release: Notify,
}

fn capture(requests: Arc<Requests>, hold: bool) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let requests = requests.clone();
        let mut input = input.clone();
        let client = MockClient::builder().build();
        let data = harnx_runtime::config::input::prepare_completion_data(
            &mut input,
            config,
            client.model(),
            false,
            &client,
        );
        let limits = config.read().run_context.clone();
        Box::pin(async move {
            requests
                .skip_modes
                .lock()
                .unwrap()
                .push(input.skip_user_log_append);
            let wire = data?
                .messages
                .into_iter()
                .filter_map(|m| {
                    TimingReceipt::from_notice_line(&m.content.to_text())
                        .map(|_| m.content.to_text())
                })
                .collect();
            requests.wire.lock().unwrap().push(wire);
            requests
                .suffixes
                .lock()
                .unwrap()
                .push(input.timing_context.map(|c| c.receipt));
            requests.limits.lock().unwrap().push(limits);
            requests.entered.notify_one();
            if hold {
                requests.release.notified().await;
            }
            Ok((
                "completed".into(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

fn notices(entries: &[(u64, SessionLogEntry)]) -> Vec<(TimingReceipt, String)> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                TimingReceipt::from_notice_line(&content.to_text()).map(|r| (r, content.to_text()))
            }
            _ => None,
        })
        .collect()
}

async fn crash_after_initial_request(
    config: GlobalConfig,
    store: SessionMetadataStore,
    requests: Arc<Requests>,
) -> Result<()> {
    let call_fn = capture(requests.clone(), true);
    let task = AbortOnDropHandle::new(tokio::spawn(direct_turn(config, store, call_fn, true)));
    tokio::time::timeout(CI_SAFE_TIMEOUT, requests.entered.notified()).await?;
    // Model is barrier-held after timing persistence. Drop the whole invocation
    // without an accepted response or terminal receipt, as with process loss.
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    Ok(())
}

async fn setup_recovery_authority_and_limits(
    store: &SessionMetadataStore,
    log: &NatsSessionLog,
    admitted_at: DateTime<Utc>,
    child: bool,
) -> Result<RunLimitsRecord> {
    let authority = if child {
        let parent = RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            admitted_at - ChronoDuration::seconds(1800),
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(5400)),
        )?;
        AdmissionAuthority::Inherited {
            parent,
            edge: InvocationEdgeKind::Delegation,
            admitted_at,
        }
    } else {
        AdmissionAuthority::External { admitted_at }
    };
    let timeout = if child { 7200 } else { 3600 };
    let mut intent =
        InvocationAdmission::new(&authority, "resume-prompt".into(), Some(timeout), None);
    intent.prompt_content = Some(MessageContent::Text("resume prompt".into()));
    let intent = store
        .reserve_admission(log.storage_key(), &intent, &[])
        .await?;
    store
        .bind_prompt_admission(
            log.storage_key(),
            "resume-prompt",
            intent.invocation_id.as_str(),
        )
        .await?;
    let limits = if let Some(parent) = &intent.parent {
        let limits = RunLimitsRecord::admit_child(
            parent,
            intent.invocation_id.clone(),
            InvocationEdgeKind::Delegation,
            admitted_at,
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(timeout)),
        )?;
        assert_eq!(limits.deadline, parent.deadline);
        assert_ne!(limits.admitted_at, parent.admitted_at);
        limits
    } else {
        let limits = RunLimitsRecord::admit_root(
            intent.run_id.clone(),
            intent.invocation_id.clone(),
            admitted_at,
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(timeout)),
        )?;
        store.put_run_limits(log.storage_key(), &limits).await?;
        limits
    };
    store
        .put_invocation_limits(log.storage_key(), &limits)
        .await?;
    Ok(limits)
}

fn assert_crash_durable_count_and_wire(durable: &[(TimingReceipt, String)], first: &Arc<Requests>) {
    assert_eq!(
        durable.len(),
        1,
        "skip mode still persists timing before model dispatch"
    );
    assert_eq!(
        *first.wire.lock().unwrap(),
        vec![vec![durable[0].1.clone()]]
    );
}

fn assert_crash_receipt_milestones(receipt: &TimingReceipt, admitted_at: DateTime<Utc>) {
    assert_eq!(receipt.origin, admitted_at);
    assert_eq!(receipt.highest_elapsed_bucket, 3);
    assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_75));
}

fn assert_crash_coverage_bounds_and_transcript(
    receipt: &TimingReceipt,
    before: &[(u64, SessionLogEntry)],
) {
    assert!(!receipt.coverage.contains(MilestoneCoverage::PERCENT_90));
    assert!(!before.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::Cancel { .. } | SessionLogEntry::TurnEnd { .. }
    )));
}

fn assert_initial_crash_notices(
    durable: &[(TimingReceipt, String)],
    first: &Arc<Requests>,
    admitted_at: DateTime<Utc>,
    before: &[(u64, SessionLogEntry)],
) {
    assert_crash_durable_count_and_wire(durable, first);
    assert_crash_receipt_milestones(&durable[0].0, admitted_at);
    assert_crash_coverage_bounds_and_transcript(&durable[0].0, before);
}

// Compaction may archive timing while re-logging executable context.
async fn compact_and_verify_reduced(
    log: &NatsSessionLog,
    prompt: &SessionLogEntry,
    base_session: &Session,
) -> Result<()> {
    log.append_event_async(&SessionLogEntry::Compress {
        prompt: "completed older work".into(),
    })
    .await?;
    log.append_event_async(prompt).await?;
    let reduced = harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
        &log.load_events_latest_async().await?,
        log.storage_key(),
        base_session.clone(),
    )?;
    assert!(!reduced
        .messages
        .iter()
        .any(|m| TimingReceipt::from_notice_line(&m.content.to_text()).is_some()));
    Ok(())
}

struct ResumedWorkerParams<'a> {
    js: &'a async_nats::jetstream::Context,
    id: &'a str,
    log: &'a NatsSessionLog,
    store: &'a SessionMetadataStore,
    limits: &'a RunLimitsRecord,
    compact: bool,
    durable: &'a [(TimingReceipt, String)],
    resumed: &'a Arc<Requests>,
}

fn assert_resumed_dispatch_context(resumed: &Arc<Requests>, expected_limits: &RunLimitsRecord) {
    assert_eq!(
        resumed.suffixes.lock().unwrap().as_slice(),
        [None],
        "covered milestones are not appended again"
    );
    assert_eq!(
        *resumed.limits.lock().unwrap(),
        vec![Some(expected_limits.clone())]
    );
    assert_eq!(*resumed.skip_modes.lock().unwrap(), vec![true]);
}

fn assert_resumed_wire_shape(resumed: &Arc<Requests>, compact: bool) {
    assert_eq!(
        resumed.wire.lock().unwrap()[0].len(),
        if compact { 0 } else { 1 }
    );
    assert_eq!(resumed.wire.lock().unwrap().len(), 1);
}

fn assert_resumed_log_status(
    reloaded: &[(u64, SessionLogEntry)],
    expected_notices: &[(TimingReceipt, String)],
) {
    assert_eq!(
        notices(reloaded),
        expected_notices,
        "raw receipts survive restart/compaction dedup"
    );
    assert_eq!(
        reconstruct_state_from_nats(reloaded).turn_status,
        TurnStatus::Idle
    );
}

async fn run_and_verify_resumed_worker(
    params: &ResumedWorkerParams<'_>,
    daemon: &tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let acquisitions = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    activate_session(params.js, params.id).await?;
    let idle = wait_for_worker_daemon_idle(params.js, params.id, acquisitions).await;
    if idle.is_err() || params.resumed.wire.lock().unwrap().is_empty() {
        anyhow::bail!(
            "reloaded worker failed: idle={idle:?}, daemon_finished={}, entries={:?}, activity={:?}",
            daemon.is_finished(),
            params.log.load_events_latest_async().await?,
            params.store.get_activity(params.log.storage_key()).await?
        );
    }
    assert_resumed_dispatch_context(params.resumed, params.limits);
    assert_resumed_wire_shape(params.resumed, params.compact);
    let reloaded = params.log.load_events_latest_async().await?;
    assert_resumed_log_status(&reloaded, params.durable);
    Ok(())
}

fn assert_fresh_final_entries(
    final_entries: &[(u64, SessionLogEntry)],
    final_notices: &[(TimingReceipt, String)],
) {
    assert_eq!(
        final_notices.len(),
        2,
        "new invocation persists a fresh initial"
    );
    assert_eq!(
        reconstruct_state_from_nats(final_entries).turn_status,
        TurnStatus::Idle
    );
    assert!(final_entries.iter().any(|(_, e)| matches!(e,
        SessionLogEntry::Message { role, content, fence_token: Some(_), .. }
            if role.is_user() && content.to_text() == final_notices[1].1)));
}

fn assert_fresh_receipt_identity(
    fresh: &TimingReceipt,
    limits: &RunLimitsRecord,
    admitted_at: DateTime<Utc>,
) {
    assert_ne!(fresh.invocation_id, limits.invocation_id.as_str());
    assert_ne!(fresh.origin, admitted_at);
}

fn assert_fresh_receipt_milestones(fresh: &TimingReceipt) {
    assert_eq!(fresh.coverage, MilestoneCoverage::INITIAL);
    assert_eq!(fresh.highest_elapsed_bucket, 0);
}

fn assert_fresh_wire_requests(resumed: &Arc<Requests>, fresh: &TimingReceipt) {
    assert_eq!(resumed.suffixes.lock().unwrap()[1].as_ref(), Some(fresh));
    assert_eq!(
        resumed.wire.lock().unwrap().len(),
        2,
        "new run produces one model call"
    );
}

async fn verify_fresh_invocation_turn(
    params: &ResumedWorkerParams<'_>,
    admitted_at: DateTime<Utc>,
) -> Result<()> {
    let ResumedWorkerParams {
        js,
        id,
        log,
        limits,
        resumed,
        ..
    } = params;
    worker::append_admitted_fixture_user(log, "fresh-prompt", "fresh prompt").await?;
    let acquisitions = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    activate_session(js, id).await?;
    wait_for_worker_daemon_idle(js, id, acquisitions).await?;
    let final_entries = log.load_events_latest_async().await?;
    let final_notices = notices(&final_entries);
    assert_fresh_final_entries(&final_entries, &final_notices);
    let fresh = &final_notices[1].0;
    assert_fresh_receipt_identity(fresh, limits, admitted_at);
    assert_fresh_receipt_milestones(fresh);
    assert_fresh_wire_requests(resumed, fresh);
    Ok(())
}

async fn run_recovery(compact: bool, child: bool) -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let id = "timing-recovery";
    let (store, metadata) = seed_session_metadata(&js, id).await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(id), 1);
    // Cross 75% and three buckets without waiting, with 800s enforcement margin.
    let admitted_at = Utc::now() - ChronoDuration::seconds(2800);
    let limits = setup_recovery_authority_and_limits(&store, &log, admitted_at, child).await?;

    let prompt = SessionLogEntry::Message {
        id: Some("resume-prompt".into()),
        role: MessageRole::User,
        content: MessageContent::Text("resume prompt".into()),
        timestamp: None,
        fence_token: None,
    };
    log.append_event_async(&prompt).await?;
    let original = local_nats_runtime_config(server.url());
    {
        let mut config = original.write();
        config.session = Some(metadata.base_session());
        config.run_context = Some(limits.clone());
    }
    let first = Arc::new(Requests::default());
    crash_after_initial_request(original, store.clone(), first.clone()).await?;
    let before = log.load_events_latest_async().await?;
    let durable = notices(&before);
    assert_initial_crash_notices(&durable, &first, admitted_at, &before);

    if compact {
        compact_and_verify_reduced(&log, &prompt, &metadata.base_session()).await?;
    }

    let resumed = Arc::new(Requests::default());
    let worker_params = ResumedWorkerParams {
        js: &js,
        id,
        log: &log,
        store: &store,
        limits: &limits,
        compact,
        durable: &durable,
        resumed: &resumed,
    };
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "timing-reloaded-worker",
        capture(resumed.clone(), false),
    )
    .await?;
    let _cleanup = scopeguard::guard(daemon.abort_handle(), |handle| handle.abort());
    run_and_verify_resumed_worker(&worker_params, &daemon).await?;
    verify_fresh_invocation_turn(&worker_params, admitted_at).await?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_recovery_skip_mode_restart_preserves_current_invocation() -> Result<()> {
    run_recovery(false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_recovery_after_compaction_uses_raw_not_active_receipts() -> Result<()> {
    run_recovery(true, false).await
}

async fn direct_turn(
    config: GlobalConfig,
    store: SessionMetadataStore,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    replay: bool,
) -> Result<()> {
    let mut input = harnx_runtime::config::input::from_str(
        &config,
        if replay {
            "resume prompt"
        } else {
            "fresh prompt"
        },
        None,
    );
    input.skip_user_log_append = replay;
    run_agent_loop_with_nats(RunAgentLoopArgs {
        cluster_key: "local",
        manage_servers: false,
        session_id: &storage_key("timing-recovery"),
        config,
        instance_id: harnx_core::instance::ServerScope::new(),
        initial_input: input,
        abort_signal: create_abort_signal(),
        token_budget: None,
        call_fn: Some(call_fn),
        lease: None,
        activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        event_sink: None,
        after_seq_observer: None,
        session_metadata: Some(&store),
        on_tool_round: None,
        working_dir: None,
    })
    .await
}

fn assert_unmanaged_notice_origin_changed(fresh: &TimingReceipt, old: &TimingReceipt) {
    assert_ne!(fresh.invocation_id, old.invocation_id);
    assert!(fresh.origin >= old.origin);
}

fn assert_unmanaged_notice_milestones_and_durable(
    fresh: &TimingReceipt,
    durable_second: &TimingReceipt,
) {
    assert_eq!(fresh.highest_elapsed_bucket, 0);
    assert_eq!(fresh.coverage, MilestoneCoverage::INITIAL);
    assert_eq!(*durable_second, *fresh);
}

fn assert_unmanaged_fresh_notice(
    fresh: &TimingReceipt,
    old: &TimingReceipt,
    durable_second: &TimingReceipt,
) {
    assert_unmanaged_notice_origin_changed(fresh, old);
    assert_unmanaged_notice_milestones_and_durable(fresh, durable_second);
}

async fn run_unmanaged_recovery(replay: bool) -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let (store, metadata) = seed_session_metadata(&js, "timing-recovery").await?;
    let log = NatsSessionLog::new_with_replicas(js, storage_key("timing-recovery"), 1);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("unmanaged-prompt".into()),
        role: MessageRole::User,
        content: MessageContent::Text("resume prompt".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    let config = local_nats_runtime_config(server.url());
    config.write().session = Some(metadata.base_session());
    let first = Arc::new(Requests::default());
    crash_after_initial_request(config, store.clone(), first).await?;
    let old = notices(&log.load_events_latest_async().await?)[0].0.clone();
    let config = local_nats_runtime_config(server.url());
    config.write().session = Some(metadata.base_session());
    let requests = Arc::new(Requests::default());
    direct_turn(config, store, capture(requests.clone(), false), replay).await?;
    let entries = log.load_events_latest_async().await?;
    let durable = notices(&entries);
    assert_eq!(durable.len(), if replay { 1 } else { 2 });
    assert_eq!(requests.limits.lock().unwrap().as_slice(), [None]);
    assert_eq!(requests.skip_modes.lock().unwrap().as_slice(), [replay]);
    if replay {
        assert_eq!(requests.suffixes.lock().unwrap().as_slice(), [None]);
        assert_eq!(
            durable[0].0, old,
            "unmanaged replay retains captured origin/identity"
        );
    } else {
        let fresh = requests.suffixes.lock().unwrap()[0].clone().unwrap();
        assert_unmanaged_fresh_notice(&fresh, &old, &durable[1].0);
    }
    assert_eq!(
        reconstruct_state_from_nats(&entries).turn_status,
        TurnStatus::Idle
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_recovery_unmanaged_existing_replay_retains_origin() -> Result<()> {
    run_unmanaged_recovery(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_recovery_unmanaged_new_input_does_not_reuse_interrupted_origin() -> Result<()> {
    run_unmanaged_recovery(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_recovery_child_reload_retains_own_origin_and_ancestor_clamp() -> Result<()> {
    run_recovery(true, true).await
}

// No old reader runs alongside the writer. Receipts outlive stopped workers.
async fn run_cutover_first_turn(
    server_url: &str,
    js: &async_nats::jetstream::Context,
    id: &str,
    log: &NatsSessionLog,
) -> Result<(
    Vec<(TimingReceipt, String)>,
    Vec<(u64, SessionLogEntry)>,
    RunLimitsRecord,
)> {
    worker::append_admitted_fixture_user(log, "cutover-prompt", "before cutover").await?;
    assert!(notices(&log.load_events_latest_async().await?).is_empty());

    let first = Arc::new(Requests::default());
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server_url),
        "cutover-compatible-worker",
        capture(first.clone(), false),
    )
    .await?;
    let cleanup = scopeguard::guard(daemon.abort_handle(), |handle| handle.abort());
    let acquisitions = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    activate_session(js, id).await?;
    wait_for_worker_daemon_idle(js, id, acquisitions).await?;
    assert_eq!(first.wire.lock().unwrap().len(), 1);
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
    drop(cleanup);
    assert!(!harnx_runtime::nats_lease::session_has_active_lease(js, log.storage_key()).await?);

    fn assert_cutover_drained_receipts(
        receipts: &[(TimingReceipt, String)],
        drained: &[(u64, SessionLogEntry)],
    ) {
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            reconstruct_state_from_nats(drained).turn_status,
            TurnStatus::Idle
        );
    }

    let drained = log.load_events_latest_async().await?;
    let receipts = notices(&drained);
    assert_cutover_drained_receipts(&receipts, &drained);
    let original_limits = first.limits.lock().unwrap()[0].clone().unwrap();
    Ok((receipts, drained, original_limits))
}

// Negative control copies latest_prompt_id's User selector from 5a6c2cba3
// (daemon_session_exec.rs:82-87). This isn't an old-binary integration test:
// even drained receipt-bearing history selects a row without prompt admission.
async fn assert_legacy_prompt_unadmitted(
    drained: &[(u64, SessionLogEntry)],
    store: &SessionMetadataStore,
    storage_key: &str,
) -> Result<String> {
    let legacy_prompt_id = drained
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id.clone()),
            _ => None,
        })
        .context("receipt-bearing history has a User row")?;
    assert!(legacy_prompt_id.starts_with("timing-v1-"));
    assert!(store
        .prompt_admission(storage_key, &legacy_prompt_id)
        .await?
        .is_none());
    assert!(store
        .prompt_admission(storage_key, "cutover-prompt")
        .await?
        .is_some());
    Ok(legacy_prompt_id)
}

struct RetainedRollbackParams<'a> {
    server_url: &'a str,
    js: &'a async_nats::jetstream::Context,
    id: &'a str,
    log: &'a NatsSessionLog,
    store: &'a SessionMetadataStore,
    drained: &'a [(u64, SessionLogEntry)],
    receipts: &'a [(TimingReceipt, String)],
    original_limits: &'a RunLimitsRecord,
    legacy_prompt_id: &'a str,
}

fn assert_rollback_history_and_wire(
    after: &[(u64, SessionLogEntry)],
    drained: &[(u64, SessionLogEntry)],
    retained: &Arc<Requests>,
    first_receipt_text: &str,
) {
    assert_eq!(
        &after[..drained.len()],
        drained,
        "rollback must not rewrite histories"
    );
    assert_eq!(
        retained.wire.lock().unwrap().len(),
        1,
        "no timing-only model calls"
    );
    assert_eq!(retained.wire.lock().unwrap()[0][0], first_receipt_text);
}

fn assert_rollback_transcript_status(after: &[(u64, SessionLogEntry)]) {
    assert_eq!(
        notices(after).len(),
        2,
        "only the new invocation gets an initial receipt"
    );
    assert_eq!(
        reconstruct_state_from_nats(after).turn_status,
        TurnStatus::Idle
    );
    assert!(!after
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })));
}

async fn assert_rollback_limits_and_admission(
    store: &SessionMetadataStore,
    storage_key: &str,
    original_limits: &RunLimitsRecord,
    legacy_prompt_id: &str,
) -> Result<()> {
    assert_eq!(
        store
            .get_invocation_limits(storage_key, original_limits.invocation_id.as_str())
            .await?,
        Some(original_limits.clone()),
        "rollback must not re-admit or extend the original run"
    );
    assert!(
        store
            .prompt_admission(storage_key, legacy_prompt_id)
            .await?
            .is_none(),
        "receipts must not be repaired by granting executable admission"
    );
    Ok(())
}

// Roll back surrounding deployment/config only, retaining a compatible reader
// over unchanged durable history.
async fn verify_retained_reader_rollback_turn(params: &RetainedRollbackParams<'_>) -> Result<()> {
    let retained = Arc::new(Requests::default());
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(params.server_url),
        "rollback-retained-compatible-worker",
        capture(retained.clone(), false),
    )
    .await?;
    let _cleanup = scopeguard::guard(daemon.abort_handle(), |handle| handle.abort());
    worker::append_admitted_fixture_user(params.log, "rollback-prompt", "after rollback").await?;
    let acquisitions = harnx_runtime::nats_metrics::snapshot().lease_acquisitions;
    activate_session(params.js, params.id).await?;
    wait_for_worker_daemon_idle(params.js, params.id, acquisitions).await?;
    let after = params.log.load_events_latest_async().await?;
    assert_rollback_history_and_wire(&after, params.drained, &retained, &params.receipts[0].1);
    assert_rollback_transcript_status(&after);
    assert_rollback_limits_and_admission(
        params.store,
        params.log.storage_key(),
        params.original_limits,
        params.legacy_prompt_id,
    )
    .await?;
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
    Ok(())
}

/// CAL-DEP01: rollback keeps receipt-compatible readers, not pre-receipt binaries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timing_receipt_hard_cutover_and_retained_reader_rollback() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("CAL-DEP01 verification requires nats-server; a skip is not evidence")?;
    let js = local_test_nats(server.url()).await?;
    let id = "timing-recovery";
    let (store, _) = seed_session_metadata(&js, id).await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(id), 1);

    let (receipts, drained, original_limits) =
        run_cutover_first_turn(server.url(), &js, id, &log).await?;

    let legacy_prompt_id =
        assert_legacy_prompt_unadmitted(&drained, &store, log.storage_key()).await?;

    let rollback_params = RetainedRollbackParams {
        server_url: server.url(),
        js: &js,
        id,
        log: &log,
        store: &store,
        drained: &drained,
        receipts: &receipts,
        original_limits: &original_limits,
        legacy_prompt_id: &legacy_prompt_id,
    };
    verify_retained_reader_rollback_turn(&rollback_params).await?;

    Ok(())
}
