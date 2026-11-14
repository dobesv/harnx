//! Watcher behavior for dispatched prompts: the background task a
//! `session_send`/`session_wait` starts publishes live `Running` progress on
//! the parent's advisory stream and a terminal row only from durable
//! settlement. These tests drive the fixture's durable child log directly, so
//! they stay independent of a real worker.
use super::fixture::*;
use super::*;
use crate::nats_event_sink::{events_subject, AdvisoryEnvelope, SessionEventStream};
use futures_util::StreamExt;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{AgentEvent, SubAgentProgress, SubAgentProgressStatus, TurnEvent};
use harnx_core::message::MessageRole;
use harnx_core::session::SessionLogEntry;
use std::time::Duration;

/// Generous for a degraded CI runner; the fixture's cadences are 20/50 ms.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);

async fn subscribe_parent_progress(f: &Fixture) -> Result<async_nats::Subscriber> {
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    let client = async_nats::connect(f.server.url()).await?;
    let events = client.subscribe(events_subject(&parent)).await?;
    client.flush().await?;
    Ok(events)
}

fn progress_of(envelope: &AdvisoryEnvelope) -> Option<SubAgentProgress> {
    let AgentEvent::SubAgent { event, .. } = &envelope.event else {
        return None;
    };
    let AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) = event.as_ref() else {
        return None;
    };
    Some(progress.clone())
}

async fn next_progress(events: &mut async_nats::Subscriber) -> SubAgentProgress {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        loop {
            let message = events.next().await.expect("parent events remain open");
            let envelope =
                AdvisoryEnvelope::from_bytes(&message.payload).expect("decode progress advisory");
            if let Some(progress) = progress_of(&envelope) {
                return progress;
            }
        }
    })
    .await
    .expect("receive sub-agent progress")
}

/// Heartbeats arrive continuously while the child runs; read until the
/// watcher's terminal snapshot. Bounded, so a watcher that never settles
/// fails the test instead of hanging it.
async fn next_terminal(events: &mut async_nats::Subscriber) -> SubAgentProgress {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        loop {
            let progress = next_progress(events).await;
            if progress.status != SubAgentProgressStatus::Running {
                return progress;
            }
        }
    })
    .await
    .expect("receive sub-agent terminal progress")
}

/// Non-blocking drain of already-buffered progress events. One deadline
/// bounds the whole drain: the fixture's 20 ms heartbeat is faster than the
/// old per-message timeout, so draining while a watcher still heartbeats
/// would loop forever.
async fn drain_progress(events: &mut async_nats::Subscriber) -> Vec<SubAgentProgress> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(message) = tokio::time::timeout_at(deadline, events.next()).await {
        let message = message.expect("parent events remain open");
        if let Ok(envelope) = AdvisoryEnvelope::from_bytes(&message.payload) {
            if let Some(progress) = progress_of(&envelope) {
                seen.push(progress);
            }
        }
    }
    seen
}

#[tokio::test]
async fn wait_started_watcher_announces_and_finishes_from_durable_terminal() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let mut events = subscribe_parent_progress(&f).await?;
    let writer = async {
        // The dispatch itself starts no watcher in the fixture (it calls
        // dispatch directly), so this announcement can only come from the
        // watcher session_wait started.
        let running = next_progress(&mut events).await;
        assert_eq!(running.status, SubAgentProgressStatus::Running);
        assert_eq!(running.invocation_id, "original-prompt");
        assert_eq!(running.agent, "helper");
        assert_eq!(running.session_id, "child");
        f.append(end(original.prompt.user_msg_seq())).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 30})), writer);
    written?;
    let result = result?;
    assert_eq!(result["status"], "completed");
    // The settled result carries the pair the TUI history rebuild needs: the
    // `sub_agent` marker and the terminal progress summary.
    assert_eq!(result["sub_agent"]["agent"], "helper");
    assert_eq!(result["sub_agent"]["session_id"], "child");
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())?;
    assert_eq!(summary.status, SubAgentProgressStatus::Done);
    assert_eq!(summary.invocation_id, "original-prompt");
    // The watcher's own terminal event also reached the parent stream.
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    Ok(())
}

#[tokio::test]
async fn wait_on_already_settled_prompt_publishes_terminal_without_running() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    f.append(end(original.prompt.user_msg_seq())).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    let result = f.poll(json!({"seconds": 30})).await?;
    assert_eq!(result["status"], "completed");
    // The watcher bound a prompt that was already settled: one terminal
    // snapshot, no Running announcement that would immediately flip.
    let seen = tokio::time::timeout(OBSERVE_TIMEOUT, async {
        let mut seen = Vec::new();
        loop {
            let progress = next_progress(&mut events).await;
            seen.push(progress.clone());
            if progress.status != SubAgentProgressStatus::Running {
                return seen;
            }
        }
    })
    .await
    .expect("one-shot terminal progress published");
    assert!(
        seen.iter()
            .all(|progress| progress.status == SubAgentProgressStatus::Done),
        "a settled prompt must not announce Running: {seen:?}"
    );
    Ok(())
}

/// Shared body for a non-Done durable terminal (`Error`/`Cancel`): the
/// watcher announces Running, the child settles through the appended entry,
/// and the settled summary and the advisory terminal report the same status.
async fn non_done_terminal_reports_the_same_status(
    terminal_entry: SessionLogEntry,
    status: &str,
    progress_status: SubAgentProgressStatus,
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let mut events = subscribe_parent_progress(&f).await?;
    let writer = async {
        let running = next_progress(&mut events).await;
        assert_eq!(running.status, SubAgentProgressStatus::Running);
        f.append(terminal_entry).await?;
        Ok::<_, anyhow::Error>(())
    };
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 30})), writer);
    written?;
    let result = result?;
    assert_eq!(result["status"], status);
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())?;
    assert_eq!(summary.status, progress_status);
    assert_eq!(summary.invocation_id, "original-prompt");
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, progress_status);
    Ok(())
}

#[tokio::test]
async fn failed_child_watcher_reports_failed_terminal() -> Result<()> {
    non_done_terminal_reports_the_same_status(
        SessionLogEntry::Error {
            message: "child broke".into(),
            fence_token: 0,
            timestamp: None,
        },
        "failed",
        SubAgentProgressStatus::Failed,
    )
    .await
}

#[tokio::test]
async fn cancelled_child_watcher_reports_cancelled_terminal() -> Result<()> {
    non_done_terminal_reports_the_same_status(
        SessionLogEntry::Cancel {
            fence_token: 0,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
        "cancelled",
        SubAgentProgressStatus::Cancelled,
    )
    .await
}

#[tokio::test]
async fn terminal_usage_matches_the_durable_turn_end_total() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let mut events = subscribe_parent_progress(&f).await?;
    let expected = CompletionTokenUsage::new(Some(210), Some(64), Some(21));
    let writer = async {
        let _ = next_progress(&mut events).await;
        f.append(SessionLogEntry::TurnEnd {
            through_seq: original.prompt.user_msg_seq(),
            fence_token: 0,
            timestamp: None,
            usage: Some(expected.clone()),
        })
        .await?;
        Ok::<_, anyhow::Error>(())
    };
    let (result, written) = tokio::join!(f.poll(json!({"seconds": 30})), writer);
    written?;
    let result = result?;
    assert_eq!(result["status"], "completed");
    // The watcher's terminal replaced the lossy live estimate with the
    // durable turn total. One bounded wait: the outer timeout covers the
    // whole run-to-terminal sequence, so a watcher that never settles fails
    // the test instead of looping forever on heartbeats.
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(
        terminal.usage, expected,
        "the terminal snapshot carries the durable TurnEnd total"
    );
    // The replay summary carries the same total instead of zeros.
    let summary: SubAgentProgress = serde_json::from_value(result["sub_agent_progress"].clone())?;
    assert_eq!(summary.usage, expected);
    Ok(())
}

#[tokio::test]
async fn unknown_prompt_watcher_announces_nothing() -> Result<()> {
    let f = Fixture::new().await?;
    let mut events = subscribe_parent_progress(&f).await?;
    progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: f.request.parent_session_id.clone().expect("fixture parent"),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "no-such-prompt".into(),
            tool_call_id: None,
        },
    );
    // The watcher must bind before announcing, so a prompt with no binding
    // produces no events at all. The window covers many announcement and
    // heartbeat ticks of the fixture's 20 ms cadence.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let drained = drain_progress(&mut events).await;
    assert!(
        drained.is_empty(),
        "a watcher for an unbound prompt must not announce: {drained:?}"
    );
    Ok(())
}

/// Next progress advisory that clears the client's sequence fence and matches
/// the wanted status. A heartbeat stamped before the test's appends can be
/// delivered after the client attaches; `should_render` correctly drops it,
/// so skip fenced advisories and keep waiting — `OBSERVE_TIMEOUT` still
/// fails the test if no renderable advisory of the wanted status arrives.
async fn next_rendered_progress(
    stream: &mut SessionEventStream,
    status: SubAgentProgressStatus,
) -> SubAgentProgress {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        loop {
            let envelope = stream
                .next()
                .await
                .expect("parent advisory stream stays open");
            let matches = match progress_of(&envelope) {
                Some(progress) => progress.status == status,
                None => false,
            };
            if matches && stream.should_render(&envelope) {
                return progress_of(&envelope).expect("checked above");
            }
        }
    })
    .await
    .expect("renderable sub-agent progress of the wanted status")
}

/// J-A2 regression: the watcher publishes while the parent's own writer keeps
/// appending durable entries. A client that attaches after those appends has
/// its fence at the new tail, so every progress advisory must still render
/// through `SessionEventStream::should_render` — only correct per-publish
/// `after_seq` re-stamping passes.
#[tokio::test]
async fn watcher_advisories_render_after_the_parent_log_advances() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    // Create the parent's stream before the watcher starts: the watcher's
    // live-seq sink resolves the stream at construction, and the attaching
    // client needs history to replay.
    let parent_log =
        crate::nats_session_log::NatsSessionLog::new_with_replicas(f.js.clone(), &parent, 1);
    parent_log.load_events_async().await?;
    progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: parent.clone(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "original-prompt".into(),
            tool_call_id: Some("parent-transcript-call".into()),
        },
    );
    // Let the watcher announce and heartbeat with its construction-time seq,
    // then have the parent's own writer append durable entries past it.
    tokio::time::sleep(Duration::from_millis(60)).await;
    parent_log
        .append_event_async(&message("parent-note-1", MessageRole::User, "later"))
        .await?;
    parent_log
        .append_event_async(&message("parent-note-2", MessageRole::User, "later still"))
        .await?;
    // A caught-up client attaches after the appends: last-applied durable seq
    // is the new tail, so stale-stamped advisories are fenced out.
    let client = async_nats::connect(f.server.url()).await?;
    let mut stream = SessionEventStream::attach(f.js.clone(), client, &parent).await?;
    let running = next_rendered_progress(&mut stream, SubAgentProgressStatus::Running).await;
    assert_eq!(
        running.tool_call_id.as_deref(),
        Some("parent-transcript-call"),
        "the watcher labels its row with the parent transcript's call id"
    );
    assert_eq!(running.invocation_id, "original-prompt");
    // The child settles; the terminal advisory must clear the same fence.
    f.append(end(original.prompt.user_msg_seq())).await?;
    let terminal = next_rendered_progress(&mut stream, SubAgentProgressStatus::Done).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    Ok(())
}

/// PR-2700 regression: the watcher must follow the child's live advisories on
/// the STORAGE-key subject (`session_key(agent, local_id)`), because that is
/// the identity the child worker's own sink publishes under; the local
/// session id is a presentation name that never reaches a subject. A tool
/// start published on the storage-key subject must reach the watcher's
/// running row, and the same advisory on the local-id subject must not.
#[tokio::test]
async fn watcher_counts_child_advisories_published_on_the_storage_key_subject() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let parent = ensure_test_watcher(&f, Some("parent-transcript-call")).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    assert_eq!(
        next_progress(&mut events).await.status,
        SubAgentProgressStatus::Running
    );

    let client = async_nats::connect(f.server.url()).await?;
    // First publish on the LOCAL-id subject: with the correct subscription
    // this must not be counted (what a watcher subscribed to the wrong
    // subject would miss in production, inverted).
    publish_child_tool_start(&client, "child").await?;
    client.flush().await?;
    let storage = harnx_core::session_identity::session_key(Some("helper"), "child");
    assert_ne!(
        storage, "child",
        "the fixture's storage key must differ from the local id for this test to bite"
    );
    // Give a wrongly-subscribed watcher several heartbeat ticks to (wrongly)
    // count the local-id advisory: within this window every advisory it
    // publishes must still report zero tool starts.
    tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            let progress = next_progress(&mut events).await;
            assert_eq!(
                progress.tool_call_count, 0,
                "an advisory on the local-id subject must not be counted"
            );
        }
    })
    .await
    .expect_err("the heartbeat window expires while the child keeps running");
    publish_child_tool_start(&client, &storage).await?;
    client.flush().await?;

    // The storage-key tool start reaches the watcher: the next Running
    // advisory counts it.
    let counted = tokio::time::timeout(OBSERVE_TIMEOUT, async {
        loop {
            let progress = next_progress(&mut events).await;
            if progress.tool_call_count >= 1 {
                return progress;
            }
        }
    })
    .await
    .expect("child tool start on the storage-key subject counted");
    assert_eq!(counted.status, SubAgentProgressStatus::Running);
    assert_eq!(counted.invocation_id, "original-prompt");
    assert_eq!(
        counted.tool_call_count, 1,
        "only the storage-key advisory is counted"
    );

    // The watcher still settles from durable evidence and unregisters.
    f.append(end(original.prompt.user_msg_seq())).await?;
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    await_watcher_removal(&parent).await?;
    Ok(())
}

/// The finished-snapshot cache keeps a settled prompt's measured view for a
/// later same-process wait: once the watcher has unregistered, a fresh
/// `ensure_watcher` cell opens prefilled with the finished terminal
/// snapshot instead of empty, so a later wait's summary cannot zero out
/// the measured title, count and elapsed time.
#[tokio::test]
async fn a_later_watcher_is_prefilled_with_the_finished_terminal_snapshot() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let parent = ensure_test_watcher(&f, Some("parent-transcript-call")).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    assert_eq!(
        next_progress(&mut events).await.status,
        SubAgentProgressStatus::Running
    );
    f.append(end(original.prompt.user_msg_seq())).await?;
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    await_watcher_removal(&parent).await?;

    // The registry entry is gone; a later wait starts a fresh watcher whose
    // shared cell must open with the finished snapshot, not empty.
    let shared = progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: parent.clone(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "original-prompt".into(),
            tool_call_id: Some("parent-transcript-call".into()),
        },
    );
    let prefilled = shared
        .lock()
        .expect("watcher share")
        .clone()
        .expect("the finished terminal snapshot prefills the fresh cell");
    assert_eq!(prefilled.status, SubAgentProgressStatus::Done);
    assert_eq!(prefilled.invocation_id, "original-prompt");

    // The fresh watcher re-settles the already-terminal prompt with one
    // terminal republish (no Running announcement), then unregisters.
    let republished = next_terminal(&mut events).await;
    assert_eq!(republished.status, SubAgentProgressStatus::Done);
    assert_eq!(republished.invocation_id, "original-prompt");
    await_watcher_removal(&parent).await?;
    Ok(())
}

/// R6-B1: a re-wait on a prompt this process already settled must not
/// announce `Running` even when the fresh watcher's first refreshes fail
/// transiently — the retained terminal snapshot is the live view until the
/// durable log reconfirms it. The failures are forced by a test seam, not a
/// tiny pass limit that could succeed or starve nondeterministically.
#[tokio::test]
async fn reconfirmation_after_transient_refresh_failures_never_announces_running() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let parent = ensure_test_watcher(&f, Some("parent-transcript-call")).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    assert_eq!(
        next_progress(&mut events).await.status,
        SubAgentProgressStatus::Running
    );
    f.append(end(original.prompt.user_msg_seq())).await?;
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    // The finished-snapshot cache hit is deterministic once the first
    // watcher has unregistered.
    await_watcher_removal(&parent).await?;

    // A re-wait opens a prefilled cell, and the fresh watcher's scan fails
    // its first TWO refreshes — the initial one inside the bind, then the
    // reconfirmation loop's first pass — before a real pass reads the
    // durable log and confirms the settlement.
    set_forced_refresh_failures(2);
    let shared = progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: parent.clone(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "original-prompt".into(),
            tool_call_id: Some("parent-transcript-call".into()),
        },
    );
    let prefilled = shared
        .lock()
        .expect("watcher share")
        .clone()
        .expect("the cache hit prefills the terminal view");
    assert_eq!(prefilled.status, SubAgentProgressStatus::Done);

    // Bounded wait for the watcher to settle and unregister, then prove the
    // retained-terminal branch ran the reconfirmation loop: the initial
    // forced failure means the one-shot settled path could not have been
    // taken, so zero passes means the branch was never entered.
    await_watcher_removal(&parent).await?;
    let passes = progress::reconfirmation_passes();
    assert!(
        passes >= 2,
        "the retained-terminal branch entered the reconfirmation loop ({passes} passes)"
    );
    // Every progress event the re-wait produced: one terminal republish and
    // no Running advisory anywhere in the sequence.
    let seen = drain_progress(&mut events).await;
    assert_eq!(
        seen.len(),
        1,
        "exactly one terminal republish, no Running advisory: {seen:?}"
    );
    assert_eq!(seen[0].status, SubAgentProgressStatus::Done);
    assert_eq!(seen[0].invocation_id, "original-prompt");
    assert_eq!(
        seen[0].tool_call_id.as_deref(),
        Some("parent-transcript-call")
    );
    Ok(())
}

/// A prompt that joined a busy turn (#2694) stays watchable: the watcher
/// binds through the joined-dispatch record — the authoritative content is
/// the joined message, not the invocation starter's prompt — announces
/// `Running`, and settles through the shared turn's terminal instead of
/// exiting with a content mismatch.
#[tokio::test]
async fn joined_prompt_watcher_announces_and_settles_through_the_shared_turn() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    assert_eq!(
        joined.prompt.execution_id(),
        Some(original.handle.prompt_id.as_str()),
        "the fixture joined the busy turn, not a new invocation"
    );

    let mut events = subscribe_parent_b_progress(&f).await?;
    progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: "parent-b".into(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "join-call".into(),
            tool_call_id: Some("parent-b-call".into()),
        },
    );
    // Binding through the joined record finds the joined message; without
    // it the scan would flag the starter's prompt content and the watcher
    // would exit before announcing anything.
    let running = next_progress(&mut events).await;
    assert_eq!(running.status, SubAgentProgressStatus::Running);
    assert_eq!(running.invocation_id, "join-call");
    assert_eq!(running.tool_call_id.as_deref(), Some("parent-b-call"));

    // The shared turn ends covering the joined message; the joined prompt's
    // watcher settles through that terminal.
    f.append(message("reply", MessageRole::Assistant, "joined answer"))
        .await?;
    f.append(end(joined.prompt.user_msg_seq())).await?;
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(terminal.invocation_id, "join-call");
    await_prompt_watcher_removal("parent-b", "join-call").await?;
    Ok(())
}

/// A journaled second-parent send against the fixture's busy child,
/// mirroring `joined_tests::joining_send` (#2672): the joining prompt gets
/// its own durable message and a joined-dispatch record.
async fn joining_send(
    f: &Fixture,
    call: &str,
    message: &str,
) -> Result<dispatch::DispatchedPrompt> {
    let mut request = f.request.clone();
    request.call_id = call.into();
    request.operation_id = format!("{call}-operation");
    request.parent_session_id = Some("parent-b".into());
    request.run_context = Some(crate::nats_session::test_support::inherited_tool_context());
    request.args = json!({"message": message, "session_id": "child"});
    f.journal
        .record(&request, ("session_send", "helper", "server"))
        .await?;
    let context = harnx_toolset::ToolInvocationContext {
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        run_context: request.run_context.clone(),
        checkpoint_store: Some(std::sync::Arc::new(f.journal.checkpoint_store(&request))),
        partial_result_store: Some(std::sync::Arc::new(
            f.journal.partial_result_store(&request),
        )),
        ..Default::default()
    };
    Ok(
        dispatch::dispatch_send(&f.toolset, request.args, CancellationToken::new(), context)
            .await?
            .0,
    )
}

/// The joining parent's advisory stream. The parent's durable stream is
/// created first so the watcher's live-seq sink can bind it.
async fn subscribe_parent_b_progress(f: &Fixture) -> Result<async_nats::Subscriber> {
    let client = async_nats::connect(f.server.url()).await?;
    crate::nats_session_log::NatsSessionLog::new_with_replicas(f.js.clone(), "parent-b", 1)
        .load_events_async()
        .await?;
    let events = client.subscribe(events_subject("parent-b")).await?;
    client.flush().await?;
    Ok(events)
}

/// `WatchScan::bind` rejects a joined-dispatch record doctored to a different
/// invocation than the prompt's admission — the same binding check
/// `Scope::resolve` makes (#2672/#2694). The mismatched prompt binds as
/// unknown: the watcher announces nothing and unregisters.
#[tokio::test]
async fn doctored_joined_record_is_rejected_at_bind() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    let storage = f.log.storage_key();
    let metadata = &f.toolset.session_metadata;
    let admission = metadata
        .prompt_admission(storage, "original-prompt")
        .await?
        .expect("the fixture's prompt has an admission");
    // A joined record for this prompt that names another invocation: the
    // prompt's admission still binds the original one.
    let doctored = crate::nats_session_metadata::JoinedDispatch {
        parent_storage_key: Some("parent-b".into()),
        parent_invocation_id: None,
        prompt_content: harnx_core::message::MessageContent::Text("joined steering content".into()),
        invocation_id: format!("doctored-{}", admission.invocation_id.as_str()),
    };
    metadata
        .create_joined_dispatch(storage, "original-prompt", &doctored)
        .await?;
    assert_ne!(doctored.invocation_id, admission.invocation_id.as_str());

    // The binding check itself: the mismatched record is as good as no
    // binding at all.
    let Err(WatchBindError::UnknownPrompt) = WatchScan::bind(WatchTarget {
        metadata,
        jetstream: &f.js,
        storage,
        prompt_id: "original-prompt",
        pass_limit: Duration::from_secs(1),
    })
    .await
    else {
        panic!("a joined record for another invocation must not bind");
    };

    // The watcher the prompt would have started announces nothing: the
    // registry entry goes away without a single advisory.
    let mut events = subscribe_parent_progress(&f).await?;
    progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: parent.clone(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "original-prompt".into(),
            tool_call_id: None,
        },
    );
    await_watcher_removal(&parent).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let drained = drain_progress(&mut events).await;
    assert!(
        drained.is_empty(),
        "a doctored joined record must not announce Running: {drained:?}"
    );
    Ok(())
}

/// Deletes one message from the child's durable stream. The hole is what a
/// bounded scan can never bridge: every later window that would have covered
/// the deleted sequence reports a gap instead.
async fn delete_child_message(f: &Fixture, seq: u64) -> Result<()> {
    let stream =
        f.js.get_stream(crate::nats_session_log::stream_name_for_session(
            f.log.storage_key(),
        ))
        .await?;
    stream.delete_message(seq).await?;
    Ok(())
}

/// Publish a child tool-start advisory on the given events subject, the way
/// the child worker's own sink would.
async fn publish_child_tool_start(client: &async_nats::Client, subject: &str) -> Result<()> {
    let envelope = AdvisoryEnvelope::new(
        0,
        AgentEvent::Tool(harnx_core::event::ToolEvent::Started {
            id: "child-tool-1".into(),
            name: "ping".into(),
            kind: harnx_core::event::ToolKind::default(),
            markdown: None,
            input: serde_json::json!({}),
            locations: vec![],
        }),
    );
    let bytes = envelope.to_bytes().expect("serialize child advisory");
    client
        .publish(events_subject(subject), bytes.into())
        .await?;
    Ok(())
}

async fn ensure_test_watcher(f: &Fixture, tool_call_id: Option<&str>) -> Result<String> {
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    progress::ensure_watcher(
        &f.toolset,
        progress::WatchedPrompt {
            parent: parent.clone(),
            agent: f.toolset.agent.clone(),
            session_id: "child".into(),
            prompt_id: "original-prompt".into(),
            tool_call_id: tool_call_id.map(str::to_owned),
        },
    );
    Ok(parent)
}

/// Waits until no watcher is registered for the fixture's prompt.
async fn await_watcher_removal(parent: &str) -> Result<()> {
    await_prompt_watcher_removal(parent, "original-prompt").await
}

/// Waits until no watcher is registered for one prompt of the fixture child.
async fn await_prompt_watcher_removal(parent: &str, prompt_id: &str) -> Result<()> {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        while progress::watcher_registered(parent, "helper", "child", prompt_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the watcher removes its registry entry when it stops");
    Ok(())
}

/// R2-B1 regression: `WatchScan` must bound each refresh pass with its own
/// `Budget`, not one created at bind. A snapshot budget's deadline is
/// absolute from its construction, so a bind-time budget starves every
/// refresh after it — with the default 10 s bound, a child running longer
/// would never get its terminal advisory and the watcher would leak. The
/// fixture pins the per-pass bound at 30 ms, far below its 50 ms poll
/// cadence, so the same starvation reproduces in milliseconds: the child
/// settles several budget lifetimes after bind and must still get its
/// terminal, then unregister.
#[tokio::test]
async fn watcher_publishes_terminal_for_a_child_settling_after_many_pass_limits() -> Result<()> {
    let f = Fixture::configured_cadence(
        "helper",
        "observe",
        "child",
        Cadence {
            heartbeat: Some(Duration::from_millis(20)),
            poll: Some(Duration::from_millis(50)),
            watcher_pass_limit: Some(Duration::from_millis(30)),
        },
    )
    .await?;
    let original = f.dispatch().await?;
    let parent = ensure_test_watcher(&f, Some("parent-transcript-call")).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    assert_eq!(
        next_progress(&mut events).await.status,
        SubAgentProgressStatus::Running
    );
    // Several per-pass budgets elapse while the child runs: a bind-time
    // budget would already be dead here, and its refresh errors would be
    // swallowed as Pending forever.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        progress::watcher_registered(&parent, "helper", "child", "original-prompt"),
        "the watcher is still polling, well past one pass budget"
    );
    // The child settles long after bind: the terminal advisory must still
    // arrive, and the watcher must unregister itself instead of leaking.
    f.append(end(original.prompt.user_msg_seq())).await?;
    let terminal = next_terminal(&mut events).await;
    assert_eq!(terminal.status, SubAgentProgressStatus::Done);
    assert_eq!(terminal.invocation_id, "original-prompt");
    await_watcher_removal(&parent).await?;
    let leftovers = drain_progress(&mut events).await;
    assert!(
        leftovers.is_empty(),
        "no heartbeat follows the terminal: {leftovers:?}"
    );
    Ok(())
}

/// A durable gap that appears after the watcher has already bound and
/// announced the prompt is a permanent evidence error: the watcher stops
/// without inventing a terminal, and its registry entry goes away.
#[tokio::test]
async fn mid_run_evidence_gap_stops_the_watcher_without_a_terminal() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    assert_eq!(original.prompt.user_msg_seq(), 1);
    // The hole exists before the watcher starts, but beyond the log's last
    // surviving entry: the watcher's first refresh reads only up to the
    // prompt and announces `Running` normally.
    f.append(message("hole", MessageRole::Assistant, "vanishes"))
        .await?;
    delete_child_message(&f, 2).await?;
    let parent = ensure_test_watcher(&f, None).await?;
    let mut events = subscribe_parent_progress(&f).await?;
    assert_eq!(
        next_progress(&mut events).await.status,
        SubAgentProgressStatus::Running
    );
    // The first surviving entry past the hole: the watcher's next refresh
    // sees the successor without its predecessor and reports the gap
    // instead of trusting a partial history.
    f.append(message("after", MessageRole::Assistant, "later"))
        .await?;
    await_watcher_removal(&parent).await?;
    let seen = drain_progress(&mut events).await;
    assert!(
        seen.iter()
            .all(|progress| progress.status == SubAgentProgressStatus::Running),
        "an evidence gap must not fabricate a terminal: {seen:?}"
    );
    Ok(())
}

/// A prompt whose history is already unprovable at bind time announces
/// nothing: a `Running` row that can never finish would stay on screen
/// forever (R2-N5).
#[tokio::test]
async fn bind_time_evidence_gap_announces_nothing() -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    assert_eq!(original.prompt.user_msg_seq(), 1);
    f.append(message("hole", MessageRole::Assistant, "vanishes"))
        .await?;
    delete_child_message(&f, 2).await?;
    f.append(message("after", MessageRole::Assistant, "later"))
        .await?;
    let mut events = subscribe_parent_progress(&f).await?;
    // The wait sees the unprovable history; the watcher it starts must bind,
    // hit the evidence gap, and announce nothing.
    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["terminal"], false);
    // The observer's own result does not establish the background watcher
    // finished, so wait for its removal instead of a fixed sleep; the
    // drain's own deadline then bounds any wrongly-announced advisory into
    // the assertion.
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    await_watcher_removal(&parent).await?;
    let drained = drain_progress(&mut events).await;
    assert!(
        drained.is_empty(),
        "a bind-time evidence gap must not announce Running: {drained:?}"
    );
    Ok(())
}

/// The reporter can die before flushing its terminal publish; the watcher's
/// one retry re-emits the terminal snapshot itself. A `Running` share must
/// not be republished — it would resurrect a dead row — and an empty share
/// publishes nothing.
#[tokio::test]
async fn terminal_retry_republishes_only_a_terminal_snapshot() -> Result<()> {
    let f = Fixture::new().await?;
    let parent = f.request.parent_session_id.clone().expect("fixture parent");
    let mut events = subscribe_parent_progress(&f).await?;
    let sink = crate::nats_event_sink::NatsEventSink::new(
        f.js.client().clone(),
        f.js.clone(),
        parent.clone(),
    )
    .await;
    let shared: crate::nats_worker::subagent_progress::SharedSnapshots =
        std::sync::Arc::new(std::sync::Mutex::new(Some(SubAgentProgress {
            invocation_id: "prompt-1".into(),
            agent: "helper".into(),
            session_id: "child".into(),
            status: SubAgentProgressStatus::Done,
            elapsed_ms: 4_200,
            usage: CompletionTokenUsage::default(),
            tool_call_count: 2,
            title: Some("child work".into()),
            tool_call_id: Some("parent-transcript-call".into()),
        })));
    progress::retry_terminal_publish(&sink, &shared).await;
    let republished = next_progress(&mut events).await;
    assert_eq!(republished.status, SubAgentProgressStatus::Done);
    assert_eq!(republished.invocation_id, "prompt-1");
    // A Running share stays unpublished.
    *shared.lock().expect("watcher share") = Some(SubAgentProgress {
        invocation_id: "prompt-1".into(),
        agent: "helper".into(),
        session_id: "child".into(),
        status: SubAgentProgressStatus::Running,
        elapsed_ms: 0,
        usage: CompletionTokenUsage::default(),
        tool_call_count: 0,
        title: None,
        tool_call_id: None,
    });
    progress::retry_terminal_publish(&sink, &shared).await;
    // An empty share publishes nothing.
    *shared.lock().expect("watcher share") = None;
    progress::retry_terminal_publish(&sink, &shared).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let leftovers = drain_progress(&mut events).await;
    assert!(
        leftovers.is_empty(),
        "a retry must not resurrect a Running row: {leftovers:?}"
    );
    Ok(())
}
