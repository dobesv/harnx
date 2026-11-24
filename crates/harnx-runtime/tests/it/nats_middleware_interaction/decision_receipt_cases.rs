use super::harness::{single_call_model, Harness};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{
    interactions::InteractionDecisionOutcome,
    nats_session_log::NatsSessionLog,
    nats_worker::{control_subject, ControlCommand},
};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_typed_receipt_wins_over_later_round_completion() -> Result<()> {
    let harness = Harness::start("", single_call_model("probe_run", json!({}), "unused"))
        .await?
        .context("NATS required")?;
    let key = harness.session.storage_key().to_string();
    let mut commands = harness.client.subscribe(control_subject(&key)).await?;
    harness.client.flush().await?;
    let client = harness.client.clone();
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), key, 1);
    let reply = tokio::spawn(async move {
        let command = commands
            .next()
            .await
            .context("control subscription closed")?;
        assert!(matches!(
            ControlCommand::from_bytes(&command.payload)?,
            ControlCommand::InteractionResponse {
                request_seq: 42,
                ..
            }
        ));
        // Force the projection to advance before delivering the accepted receipt.
        log.append_event_async(&SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
            handoff_selection: None,
        })
        .await?;
        client
            .publish(
                command.reply.context("missing reply inbox")?,
                serde_json::to_vec(&InteractionDecisionOutcome::Applied)?.into(),
            )
            .await?;
        client.flush().await?;
        Ok::<_, anyhow::Error>(())
    });
    let outcome = harness.approve("accepted-before-completion", 42).await?;
    assert_eq!(outcome, InteractionDecisionOutcome::Applied);
    reply.await??;
    Ok(())
}
