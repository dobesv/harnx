use super::*;

static RETRACTED_ORPHAN_ACTIVATION_CALLS: AtomicUsize = AtomicUsize::new(0);

fn assert_no_resume_or_interrupt_metric_delta(
    metrics_before: harnx_runtime::nats_metrics::NatsMetricsSnapshot,
    metrics_after: harnx_runtime::nats_metrics::NatsMetricsSnapshot,
) {
    assert_eq!(
        metrics_after.resumes, metrics_before.resumes,
        "retracted tool round must not trigger a resume/orphan repair"
    );
    assert_eq!(
        metrics_after.interrupt_errors_synthesized, metrics_before.interrupt_errors_synthesized,
        "retracted tool round must not synthesize an interrupt-error result"
    );
}

fn assert_retracted_orphan_absent(entries: &[(u64, SessionLogEntry)], call_id: &str) -> Result<()> {
    assert_eq!(
        count_tool_results_with_id(entries, call_id),
        0,
        "worker must not synthesize or replay ToolResults for retracted orphan tool call"
    );
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    assert!(
        !effective.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolCalls { calls, .. }
                if calls.iter().any(|call| call.id.as_deref() == Some(call_id))
        )),
        "effective log must not contain retracted tool call"
    );
    assert!(
        !effective.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|result| result.id.as_deref() == Some(call_id))
        )),
        "effective log must not contain resurrected ToolResults for retracted tool call"
    );
    Ok(())
}

/// Append a `ToolCalls` round for `call-retracted-orphan` and immediately
/// retract it with an `EditEntries` tombstone, leaving the effective log idle.
///
/// `fence_token` must stay `None` (or <= the worker's lease revision) so the
/// resume is not aborted by `abort_resume_if_fenced` before it reaches
/// `load_or_repair_session` — a stale fence here would make the test pass
/// vacuously, with the orphan scan never running.
async fn seed_retracted_orphan_tool_call(log: &NatsSessionLog) -> Result<()> {
    let tool_calls_seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: Some("thinking".to_string()),
            calls: vec![ToolCall::new(
                "echo".to_string(),
                json!({"message": "ghost"}),
                Some("call-retracted-orphan".to_string()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let tool_calls_seq = usize::try_from(tool_calls_seq).expect("JetStream seq fits usize");
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: tool_calls_seq,
        to: tool_calls_seq,
        replacements: vec![],
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_orphan_tool_call_is_not_repaired_by_worker() -> Result<()> {
    RETRACTED_ORPHAN_ACTIVATION_CALLS.store(0, Ordering::SeqCst);
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let tool_calls_seen = Arc::new(AtomicUsize::new(0));
    let seen_for_call_fn = Arc::clone(&tool_calls_seen);
    let call_fn: harnx_runtime::agent_loop::AgentCallFn =
        Arc::new(move |_input, _config, _abort| {
            let seen = seen_for_call_fn.clone();
            Box::pin(async move {
                RETRACTED_ORPHAN_ACTIVATION_CALLS.fetch_add(1, Ordering::SeqCst);
                seen.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "should not run".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            })
        });
    let daemon =
        spawn_worker_daemon_with_call_fn(config, "worker-retracted-orphan", call_fn).await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "retracted-orphan-test";
    let log = NatsSessionLog::new(js.clone(), storage_key(session_id));

    seed_session_metadata(&js, session_id).await?;
    // Deliberately NO unanswered user message in the seed: the log must contain
    // ONLY a retracted tool round, so that after mutations are applied the
    // effective log is idle and the worker has nothing to do. A pending user
    // message would (correctly) make the worker run a normal turn and invoke
    // call_fn, confounding the "orphan repair must not run" assertions below.
    seed_retracted_orphan_tool_call(&log).await?;

    // Snapshot HA metrics before activation. With the bug (raw orphan scan) the
    // worker would detect the retracted ToolCalls as an orphan (resumes += 1) and
    // synthesize an interrupt-error ToolResults (interrupt_errors_synthesized += 1).
    // With the fix (effective scan) the retract is honored: neither increments.
    let metrics_before = harnx_runtime::nats_metrics::snapshot();

    activate_session(&js, session_id).await?;

    // Wait for acquisition followed by lease release and task completion, so
    // zero-valued assertions cannot pass before the orphan scan has run.
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;

    let entries = log.load_events_async().await?;
    assert_eq!(
        tool_calls_seen.load(Ordering::SeqCst),
        0,
        "orphan repair must not invoke call_fn for retracted tool call"
    );
    assert_no_resume_or_interrupt_metric_delta(
        metrics_before,
        harnx_runtime::nats_metrics::snapshot(),
    );
    assert_retracted_orphan_absent(&entries, "call-retracted-orphan")?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
