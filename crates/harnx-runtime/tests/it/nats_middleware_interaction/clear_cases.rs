use super::harness::*;
use anyhow::{Context, Result};
use harnx_core::{message::MessageContent, session::SessionLogEntry};
use harnx_runtime::{
    interaction_checkpoint::round_started,
    interactions::{derive_pending_interactions, InteractionDecisionOutcome},
    nats_lease::session_has_active_lease,
    nats_session::{fixed_admission::FixedAdmissionOutcome, PromptProgress},
    nats_session_log::NatsSessionLog,
    nats_worker::{publish_session_activate, SessionActivate},
};
use serde_json::json;
use std::time::Duration;

// No save/restore: resuming this script would repeat the physical dispatch.
const CONFIRM_AFTER_NEXT: &str = r#"
middleware:
- name: gate
  match: '^probe_run$'
  bash: |
    next
    confirm --id keep 'Keep this result?'
"#;

async fn start() -> Result<Harness> {
    let model = single_call_model("probe_run", json!({"value": "before-clear"}), "reused-call");
    Harness::required_start(CONFIRM_AFTER_NEXT, model).await
}

struct ClearedRound {
    request_seq: u64,
    interaction_id: String,
    clear_seq: u64,
    round: u64,
}

async fn clear_suspension(harness: &Harness) -> Result<ClearedRound> {
    let session = &harness.session;
    let (request_seq, interaction_id, _) = harness.wait_for_suspension().await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while session_has_active_lease(&harness.js, session.storage_key()).await? {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::Ok(())
    })
    .await
    .context("suspended worker did not release its lease")??;
    assert_eq!(harness.probe.call_count("probe_run"), 1);
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), session.storage_key(), 1);
    let entries = log.load_events_async().await?;
    let round = derive_pending_interactions(&entries)?[0].tool_round_seq;
    assert!(round_started(&harness.js, session.storage_key(), round).await?);
    let clear_seq = log.append_event_async(&SessionLogEntry::Clear).await?;
    assert!(derive_pending_interactions(&log.load_events_async().await?)?.is_empty());
    assert_eq!(
        harness.approve(&interaction_id, request_seq).await?,
        InteractionDecisionOutcome::AfterComplete
    );
    Ok(ClearedRound {
        request_seq,
        interaction_id,
        clear_seq,
        round,
    })
}

async fn clear_suspended_round(remove_middleware: bool) -> Result<()> {
    let harness = start().await?;
    let session = harness.session.clone().with_external_admission();
    session
        .enqueue_text("start suspended round")
        .await?
        .into_activation_result()?;
    let cleared = clear_suspension(&harness).await?;
    if remove_middleware {
        harness.update_agent_middleware("")?;
    }
    session
        .enqueue_text("fresh prompt after Clear")
        .await?
        .into_activation_result()?;
    let entries = harness.wait_for_turn_end().await?;
    for (_, entry) in entries.iter().filter(|(seq, _)| *seq > cleared.clear_seq) {
        assert!(
            !matches!(
                entry,
                SessionLogEntry::ToolCalls { .. }
                    | SessionLogEntry::ToolResults { .. }
                    | SessionLogEntry::InteractionRequested { .. }
                    | SessionLogEntry::SuspensionReady { .. }
            ),
            "cleared round must not be resumed or repaired: {entry:?}"
        );
    }
    assert!(entries.iter().any(|(seq, entry)| *seq > cleared.clear_seq
        && matches!(entry, SessionLogEntry::Message { role, content, .. }
            if role.is_assistant() && content.to_text() == "activation completed")));
    assert_eq!(
        harness.probe.call_count("probe_run"),
        1,
        "no side-effect replay"
    );
    assert!(derive_pending_interactions(&entries)?.is_empty());
    assert!(
        round_started(&harness.js, session.storage_key(), cleared.round).await?,
        "Clear retains the old checkpoint as inert history"
    );
    assert_eq!(
        harness
            .approve(&cleared.interaction_id, cleared.request_seq)
            .await?,
        InteractionDecisionOutcome::AfterComplete
    );
    harness.daemon.abort();
    harness.tool_server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_suspended_middleware_round_allows_fresh_prompt_without_replay() -> Result<()> {
    clear_suspended_round(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_suspended_round_then_remove_middleware_does_not_recover_old_effects() -> Result<()> {
    clear_suspended_round(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_suspended_fixed_prompt_progress_is_not_parked() -> Result<()> {
    let harness = start().await?;
    let session = harness.session.clone().with_external_admission();
    let ticket = session
        .prepare_fixed_admission("before-clear".into(), "before-clear".into(), "close".into())
        .await?;
    assert!(matches!(
        session
            .append_fixed_prompt(
                &ticket,
                MessageContent::Text("start suspended round".into())
            )
            .await?,
        FixedAdmissionOutcome::Admitted { .. }
    ));
    publish_session_activate(
        &harness.js,
        "local",
        &SessionActivate::new(session.storage_key()),
        1,
    )
    .await?;
    clear_suspension(&harness).await?;
    assert!(
        matches!(
            session.fixed_prompt_progress(&ticket).await?,
            PromptProgress::Running {
                last_decision: None
            }
        ),
        "Clear must abandon the parked round, not wait for an unanswerable consent"
    );
    harness.daemon.abort();
    harness.tool_server.abort();
    Ok(())
}
