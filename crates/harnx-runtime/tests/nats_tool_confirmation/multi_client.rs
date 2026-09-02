use super::*;
use harnx_runtime::nats_event_sink::SessionEventStream;

#[derive(Default)]
struct ApprovalGate {
    requests: Mutex<Vec<ToolConfirmationRequest>>,
    requested: tokio::sync::Notify,
    approval: tokio::sync::Notify,
}

impl ApprovalGate {
    fn handler(self: &Arc<Self>) -> Arc<ToolConfirmationHandler> {
        let gate = Arc::clone(self);
        Arc::new(move |request| {
            gate.requests.lock().push(request);
            gate.requested.notify_one();
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                gate.approval.notified().await;
                true
            })
        })
    }
}

fn counting_confirmation_handler(request_count: &Arc<AtomicUsize>) -> Arc<ToolConfirmationHandler> {
    let request_count = Arc::clone(request_count);
    Arc::new(move |_| {
        request_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { true })
    })
}

async fn wait_for_observer_convergence(observer: &mut SessionEventStream) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        observer.refresh_history().await?;
        let has_tool_result = observer.history().iter().any(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::ToolResults { results, .. }
                    if results.iter().any(|result| result.name == "target_session_handoff")
            )
        });
        let has_turn_end = observer
            .history()
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }));
        if has_tool_result && has_turn_end {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "observer did not converge after approval: {:?}",
            observer.history()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn assert_observer_matches_source(
    harness: &ConfirmationHarness,
    observer: &SessionEventStream,
    attached_seq: u64,
) -> Result<()> {
    let source_entries = NatsSessionLog::new(harness.jetstream.clone(), SOURCE_SESSION_ID)
        .load_events_async()
        .await?;
    assert_eq!(observer.history().len(), source_entries.len());
    assert_eq!(
        observer.last_applied_seq(),
        source_entries
            .last()
            .map(|(seq, _)| *seq)
            .unwrap_or_default()
    );
    assert!(observer.last_applied_seq() > attached_seq);
    Ok(())
}

fn assert_confirmation_routing(gate: &ApprovalGate, observer_request_count: &AtomicUsize) {
    assert_eq!(
        gate.requests.lock().as_slice(),
        &[ToolConfirmationRequest {
            session_id: SOURCE_SESSION_ID.to_string(),
            tool_call_id: Some("hook-approval-handoff".to_string()),
            tool_name: "target_session_handoff".to_string(),
            arguments: json!({
                "prompt": "finish after approval",
                "session_id": TARGET_SESSION_ID,
            }),
            reason: Some("Approve the handoff?".to_string()),
        }]
    );
    assert_eq!(
        observer_request_count.load(Ordering::SeqCst),
        0,
        "confirmation must only use the route carried by the activation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_by_owning_frontend_propagates_to_second_observer() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    let gate = Arc::new(ApprovalGate::default());
    let owner_route = harness
        .source
        .tool_confirmation_route(gate.handler())
        .await?;
    let observer_request_count = Arc::new(AtomicUsize::new(0));
    let observer_route = harness
        .source
        .tool_confirmation_route(counting_confirmation_handler(&observer_request_count))
        .await?;

    let turn = harness.source.run_turn_with_tool_confirmation_route(
        "start handoff",
        Arc::new(NullSink),
        None,
        &owner_route,
    );
    tokio::pin!(turn);
    tokio::select! {
        _ = gate.requested.notified() => {}
        result = &mut turn => anyhow::bail!("turn ended before requesting confirmation: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            anyhow::bail!("worker did not request tool confirmation")
        }
    }

    let mut observer = SessionEventStream::attach(
        harness.jetstream.clone(),
        harness.client.clone(),
        SOURCE_SESSION_ID,
    )
    .await?;
    let observer_attached_seq = observer.last_applied_seq();
    gate.approval.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(30), &mut turn)
        .await
        .context("source turn did not finish after approval")??;

    wait_for_observer_convergence(&mut observer).await?;
    assert_observer_matches_source(&harness, &observer, observer_attached_seq).await?;
    assert_confirmation_routing(&gate, &observer_request_count);
    drop(observer_route);
    wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        TARGET_SESSION_ID,
    ))
    .await?;
    Ok(())
}
