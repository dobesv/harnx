//! Rejected decisions and disconnected clients must not wedge the manifest head.

use std::sync::Arc;

use agent_client_protocol::schema::v1::ResumeSessionRequest;
use anyhow::Result;
use harnx_acp_server::permission::{ALLOW_OPTION_ID, REJECT_OPTION_ID};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::InteractionDecisionOutcome;

use super::super::support::{attach_test_client, PermissionReply};
use super::harness::{selected, Fixture};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_lost_can_retry_exact_head_before_queued_permission() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    let seq = f.request("recover", "confirm").await?;
    f.request("queued", "confirm").await?;
    f.ready(&["recover", "queued"]).await?;
    f.permission().await?;
    f.force_outcomes
        .send(InteractionDecisionOutcome::LeaseLost)?;
    f.reply(ALLOW_OPTION_ID)?;
    f.reported("lease_lost").await?;
    let retry = serde_json::to_value(f.permission().await?)?;
    assert_eq!(retry["_meta"]["harnx:interaction_id"], "recover");
    assert_eq!(retry["_meta"]["harnx:request_seq"], seq);
    f.reply(ALLOW_OPTION_ID)?;
    assert_eq!(f.decision("recover").await?.action, "approve");
    let queued = serde_json::to_value(f.permission().await?)?;
    assert_eq!(queued["_meta"]["harnx:interaction_id"], "queued");
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("queued").await?.action, "deny");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_preserves_pending_permission_for_replacement_client() -> Result<()> {
    let Some((mut f, seq)) = Fixture::spawn_pending("disconnected").await? else {
        return Ok(());
    };
    f.permission().await?;
    let replacement = attach_test_client(Arc::clone(&f.agent), PermissionReply::Manual).await?;
    let old = std::mem::replace(&mut f.client, replacement);
    old.disconnect();
    f.reported("permission request failed").await?;
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. })));
    f.agent
        .resume_session(ResumeSessionRequest::new(
            f.session_id.clone(),
            std::env::current_dir()?,
        ))
        .await?;
    let retry = serde_json::to_value(f.permission().await?)?;
    assert_eq!(retry["_meta"]["harnx:interaction_id"], "disconnected");
    assert_eq!(retry["_meta"]["harnx:request_seq"], seq);
    f.reply(ALLOW_OPTION_ID)?;
    assert_eq!(f.decision("disconnected").await?.action, "approve");
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_reply_reports_to_latest_connection_not_old_permission_transport() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("stale-connection", "confirm").await?;
    f.ready(&["stale-connection"]).await?;
    f.permission().await?;
    let replacement = attach_test_client(Arc::clone(&f.agent), PermissionReply::Manual).await?;
    let old = std::mem::replace(&mut f.client, replacement);
    f.ready(&[]).await?;
    old.permission_responses.send(selected(ALLOW_OPTION_ID))?;
    f.reported("stale").await?;
    f.quiet().await;
    Ok(())
}
