use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::{
    event::{AgentEvent, SubAgentProgress, SubAgentProgressStatus, TurnEvent},
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    nats_event_sink::{events_subject, AdvisoryEnvelope},
    nats_session_log::NatsSessionLog,
    nats_tool_provider::NatsToolProvider,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

use super::fixture::*;

// Cross one 2s poll, with timer-wheel rounding margin, never the 10s heartbeat.
const POLL_STEP: Duration = Duration::from_millis(2100);

struct TimeResumeGuard(tokio::task::JoinHandle<()>);
impl TimeResumeGuard {
    fn pause() -> Self {
        tokio::time::pause();
        // Keep Tokio from auto-advancing while NATS I/O is pending. Only the
        // explicit 2s steps below may reach watcher polls or the 10s heartbeat.
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}
impl Drop for TimeResumeGuard {
    fn drop(&mut self) {
        self.0.abort();
        tokio::time::resume();
    }
}

fn extract_progress_from_event(event: AgentEvent) -> Option<SubAgentProgress> {
    match event {
        AgentEvent::Turn(TurnEvent::SubAgentProgress(p)) => Some(p),
        AgentEvent::SubAgent { event, .. } => extract_progress_from_event(*event),
        _ => None,
    }
}

fn extract_progress(msg: &async_nats::Message) -> Option<SubAgentProgress> {
    let Ok(env) = AdvisoryEnvelope::from_bytes(&msg.payload) else {
        return None;
    };
    extract_progress_from_event(env.event)
}

async fn wait_for_progress<F>(
    events: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
    max_duration: Duration,
    mut predicate: F,
) -> Result<SubAgentProgress>
where
    F: FnMut(&SubAgentProgress) -> bool,
{
    // Real-time safety bound only; paused Tokio time controls runtime behavior.
    let deadline = std::time::Instant::now() + max_duration;
    while std::time::Instant::now() < deadline {
        let msg = tokio::select! {
            biased;
            msg = events.next() => msg.context("progress subscription closed")?,
            _ = tokio::task::yield_now() => continue,
        };
        if let Some(p) = extract_progress(&msg) {
            if predicate(&p) {
                return Ok(p);
            }
        }
    }
    anyhow::bail!("timed out waiting for matching SubAgentProgress")
}

async fn append_decision(log: &NatsSessionLog, tool_call_id: &str) -> Result<u64> {
    log.append_event_async(&SessionLogEntry::HitlApprovalDecision {
        tool_call_id: tool_call_id.to_string(),
        approved: true,
        note: None,
        fence_token: 0,
    })
    .await
}

async fn append_approval_request(
    log: &NatsSessionLog,
    tool_call_id: &str,
    summary: &str,
) -> Result<u64> {
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: tool_call_id.to_string(),
        summary: summary.to_string(),
        fence_token: 0,
    })
    .await
}

async fn seed_initial_child(log: &NatsSessionLog, prompt_id: &str) -> Result<()> {
    crate::worker::append_admitted_fixture_user(log, prompt_id, "start task").await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "calling tools".to_string(),
        thought: None,
        calls: vec![
            ToolCall::new(
                "probe_run".into(),
                json!({"value": "call_1"}),
                Some("call-1".into()),
                None,
            ),
            ToolCall::new(
                "probe_aux".into(),
                json!({"value": "call_2"}),
                Some("call-2".into()),
                None,
            ),
        ],
        timestamp: None,
        fence_token: None,
    })
    .await?;
    Ok(())
}

async fn start_watcher(provider: &NatsToolProvider, prompt_id: &str) -> Result<()> {
    let result = call_subagent_tool(
        provider,
        "session_wait",
        json!({
            "session_id": SESSION_ID, "prompt_id": prompt_id, "seconds": 1,
        }),
    )
    .await?;
    assert_eq!(result["status"], "pending");
    Ok(())
}

async fn verify_appear(
    events: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
) -> Result<()> {
    tokio::time::advance(POLL_STEP).await;
    let p = wait_for_progress(events, Duration::from_secs(5), |p| {
        p.pending_interaction.is_some()
    })
    .await?;
    assert_eq!(p.status, SubAgentProgressStatus::Running);
    let s = p.pending_interaction.unwrap();
    assert!(
        p.elapsed_ms < 4_000,
        "first poll, not heartbeat: {}ms",
        p.elapsed_ms
    );
    assert_eq!(s.message, "Approve first call?");
    assert_eq!(s.pending_count, 1);
    Ok(())
}

async fn verify_change(
    events: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
) -> Result<()> {
    tokio::time::advance(POLL_STEP).await;
    let p = wait_for_progress(events, Duration::from_secs(5), |p| {
        p.pending_interaction
            .as_ref()
            .is_some_and(|s| s.message == "Approve second call?")
    })
    .await?;
    assert_eq!(p.status, SubAgentProgressStatus::Running);
    let s = p.pending_interaction.unwrap();
    assert_eq!(s.pending_count, 0);
    assert!(p.elapsed_ms < 6_000, "change must publish on next poll");
    Ok(())
}

async fn verify_clear_nonterminal(
    events: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
) -> Result<()> {
    tokio::time::advance(POLL_STEP).await;
    let p = wait_for_progress(events, Duration::from_secs(5), |p| {
        p.status == SubAgentProgressStatus::Running && p.pending_interaction.is_none()
    })
    .await?;
    assert_eq!(p.status, SubAgentProgressStatus::Running);
    assert!(p.pending_interaction.is_none());
    assert!(p.elapsed_ms < 8_000, "clear must precede 10s heartbeat");
    Ok(())
}

async fn verify_heartbeat_clear(
    events: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
) -> Result<()> {
    tokio::time::advance(Duration::from_secs(10)).await;
    let p = wait_for_progress(events, Duration::from_secs(15), |p| {
        p.status == SubAgentProgressStatus::Running && p.pending_interaction.is_none()
    })
    .await?;
    assert_eq!(p.status, SubAgentProgressStatus::Running);
    assert!(p.pending_interaction.is_none());
    Ok(())
}

#[tokio::test]
async fn subagent_blocked_live_progress_controlled_clock() -> Result<()> {
    require_nextest();

    let dummy_call: AgentCallFn =
        Arc::new(|_, _, _| Box::pin(async { unreachable!("no model calls") }));
    let harness = Harness::required_start("", dummy_call).await?;
    let child_log =
        NatsSessionLog::new_with_replicas(harness.js.clone(), harness.session.storage_key(), 1);

    let prompt_id = "live-prompt-clock".to_string();
    seed_initial_child(&child_log, &prompt_id).await?;
    let _lease = hold_child_lease(&harness).await?;

    let provider = Arc::new(discover_parent_tools(&harness).await?);
    let parent_key =
        harnx_core::session_identity::session_key(Some("parent-agent"), "parent-waiter");
    let mut parent_events = harness
        .client
        .subscribe(events_subject(&parent_key))
        .await?;
    harness.client.flush().await?;

    // A soft wait starts the detached watcher without leaving an observer timer
    // outstanding when the test takes control of Tokio time.
    start_watcher(&provider, &prompt_id).await?;
    harness.client.flush().await?;

    let _p_initial = wait_for_progress(&mut parent_events, Duration::from_secs(5), |p| {
        p.status == SubAgentProgressStatus::Running && p.pending_interaction.is_none()
    })
    .await
    .context("receive initial announce progress")?;

    let _resume_guard = TimeResumeGuard::pause();

    append_approval_request(&child_log, "call-1", "Approve first call?").await?;
    append_approval_request(&child_log, "call-2", "Approve second call?").await?;
    harness.client.flush().await?;

    // Step 1: appear on first 2s poll
    verify_appear(&mut parent_events)
        .await
        .context("verify_appear")?;

    // Step 2: change to call-2 on decision (detached watcher keeps polling)
    append_decision(&child_log, "call-1").await?;
    harness.client.flush().await?;
    verify_change(&mut parent_events)
        .await
        .context("verify_change")?;

    // Step 3: clear while running on second decision
    append_decision(&child_log, "call-2").await?;
    harness.client.flush().await?;
    verify_clear_nonterminal(&mut parent_events)
        .await
        .context("verify_clear_nonterminal")?;

    // Step 4: heartbeat does not reinstate
    verify_heartbeat_clear(&mut parent_events)
        .await
        .context("verify_heartbeat_clear")?;

    Ok(())
}

async fn hold_child_lease(
    harness: &Harness,
) -> Result<harnx_runtime::nats_lease::NatsSessionLease> {
    use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: harness.js.clone(),
        session_id: harness.session.storage_key(),
        worker_id: "controlled-fixture-owner".into(),
        generation: 1,
        config: NatsLeaseConfig {
            replicas: 1,
            ..Default::default()
        },
        session_metadata: None,
    })
    .await?
    .context("fixture lease excludes observer repair")
}
