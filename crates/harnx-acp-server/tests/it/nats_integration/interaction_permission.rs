//! Middleware confirms use durable ready manifests and exact response acceptance over ACP.
#[path = "interaction_permission/a2ui.rs"]
mod a2ui;
#[path = "interaction_permission/harness.rs"]
mod harness;
#[path = "interaction_permission/live.rs"]
mod live;
#[path = "interaction_permission/retry.rs"]
mod retry;

use agent_client_protocol::v1::schema::{
    CancelNotification, CloseSessionRequest, LoadSessionRequest, PermissionOptionKind,
    RequestPermissionOutcome, ResumeSessionRequest,
};
use anyhow::Result;
use harness::Fixture;
use harnx_acp_server::permission::{ALLOW_OPTION_ID, REJECT_OPTION_ID};
use harnx_core::{interaction::InteractionResponse, session::SessionLogEntry};
use harnx_runtime::interactions::{accept_interaction_response, InteractionDecisionOutcome};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirm_waits_for_suspension_ready_and_exposes_only_public_message() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    let seq = f.request("confirm-1", "confirm").await?;
    f.quiet().await;
    f.ready(&["confirm-1"]).await?; // No advisory; observer must still refresh durable history.
    let request = f.permission().await?;
    assert_eq!(request.session_id, f.session_id);
    assert_eq!(
        request.tool_call.fields.title.as_deref(),
        Some("Public message for confirm-1?")
    );
    assert_eq!(
        request.options.iter().map(|o| o.kind).collect::<Vec<_>>(),
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::RejectOnce
        ]
    );
    let value = serde_json::to_value(&request)?;
    assert_eq!(value["_meta"]["harnx:interaction_id"], "confirm-1");
    assert_eq!(value["_meta"]["harnx:request_seq"], seq);
    assert!(!value.to_string().contains("private"));
    let response = f.answer("confirm-1", ALLOW_OPTION_ID).await?;
    assert_eq!(response.action, "approve");
    assert!(response.note.is_none());
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_pending_permissions_are_serial_in_manifest_not_physical_order() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    let first_seq = f.request("physical-first", "confirm").await?;
    let second_seq = f.request("physical-second", "confirm").await?;
    f.ready(&["physical-second", "physical-first"]).await?;
    let second = serde_json::to_value(f.permission().await?)?;
    assert_eq!(second["_meta"]["harnx:request_seq"], second_seq);
    f.quiet().await;
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("physical-second").await?.action, "deny");
    let first = serde_json::to_value(f.permission().await?)?;
    assert_eq!(first["_meta"]["harnx:request_seq"], first_seq);
    f.reply(ALLOW_OPTION_ID)?;
    assert_eq!(f.decision("physical-first").await?.action, "approve");
    f.quiet().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_interaction_is_reject_only_and_forged_allow_cannot_skip_head() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("future", "a2ui-form").await?;
    f.request("queued", "confirm").await?;
    f.ready(&["future", "queued"]).await?;
    let request = f.permission().await?;
    assert!(request
        .tool_call
        .fields
        .title
        .unwrap()
        .contains("not supported by this client"));
    assert_eq!(request.options.len(), 1);
    assert_eq!(request.options[0].kind, PermissionOptionKind::RejectOnce);
    f.reply(ALLOW_OPTION_ID)?;
    f.reported("option that was not offered").await?;
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. })));
    let retry = serde_json::to_value(f.permission().await?)?;
    assert_eq!(retry["_meta"]["harnx:interaction_id"], "future");
    assert_eq!(retry["options"].as_array().unwrap().len(), 1);
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("future").await?.action, "deny");
    let queued = serde_json::to_value(f.permission().await?)?;
    assert_eq!(queued["_meta"]["harnx:interaction_id"], "queued");
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("queued").await?.action, "deny");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_interaction_reject_commits_durable_denial() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("future", "a2ui-form").await?;
    f.ready(&["future"]).await?;
    f.permission().await?;
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("future").await?.action, "deny");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_permission_response_is_durable_denial_not_approval() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("cancelled-dialog", "confirm").await?;
    f.ready(&["cancelled-dialog"]).await?;
    f.permission().await?;
    f.client
        .permission_responses
        .send(RequestPermissionOutcome::Cancelled)?;
    assert_eq!(f.decision("cancelled-dialog").await?.action, "deny");
    Ok(())
}

async fn rejected_response(outcome: &str) -> Result<()> {
    let Some((mut f, seq)) = Fixture::spawn_pending("changing").await? else {
        return Ok(());
    };
    f.permission().await?;
    match outcome {
        "conflicting" => {
            let response = InteractionResponse {
                action: "deny".into(),
                data: Default::default(),
                note: None,
            };
            assert_eq!(
                accept_interaction_response(&f.backend, &f.lease, "changing", seq, &response)
                    .await?,
                InteractionDecisionOutcome::Applied
            );
        }
        "stale" => f.ready(&[]).await?,
        "wrong_sequence" => {
            let entry = f
                .log
                .load_events_latest_async()
                .await?
                .into_iter()
                .find(|(s, _)| *s == seq)
                .unwrap()
                .1;
            f.log
                .append_event_async(&SessionLogEntry::EditEntries {
                    from: seq as usize,
                    to: seq as usize,
                    replacements: vec![harnx_runtime::nats_session_log::serialize_entry(&entry)?],
                })
                .await?;
        }
        "after_complete" => {
            f.log
                .append_event_async(&SessionLogEntry::ToolResults {
                    results: vec![],
                    handoff_selection: None,
                    timestamp: None,
                })
                .await?;
        }
        "lease_lost" => f.lease.release().await?,
        _ => unreachable!(),
    }
    f.reply(ALLOW_OPTION_ID)?;
    f.reported(outcome).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_response_is_reported_to_client() -> Result<()> {
    rejected_response("conflicting").await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_response_is_reported_to_client() -> Result<()> {
    rejected_response("stale").await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_sequence_is_reported_to_client() -> Result<()> {
    rejected_response("wrong_sequence").await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_round_response_is_reported_to_client() -> Result<()> {
    rejected_response("after_complete").await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_lease_response_is_reported_to_client() -> Result<()> {
    rejected_response("lease_lost").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_cancel_abandons_permission_without_approval_or_reopening() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("cancel-turn", "confirm").await?;
    f.request("queued", "confirm").await?;
    f.ready(&["cancel-turn", "queued"]).await?;
    f.permission().await?;
    f.agent
        .cancel(CancelNotification::new(f.session_id.clone()))
        .await?;
    f.reply(ALLOW_OPTION_ID)?;
    f.quiet().await;
    f.ready(&["queued"]).await?;
    f.quiet().await;
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. })));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_stops_observer_and_resume_or_load_hydrates_pending_permission() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.agent
        .close_session(CloseSessionRequest::new(f.session_id.clone()))
        .await?;
    f.restart_round().await?;
    f.request("reconnect", "confirm").await?;
    f.ready(&["reconnect"]).await?;
    f.quiet().await;
    f.agent
        .resume_session(ResumeSessionRequest::new(
            f.session_id.clone(),
            std::env::current_dir()?,
        ))
        .await?;
    f.permission().await?;
    f.reply(REJECT_OPTION_ID)?;
    assert_eq!(f.decision("reconnect").await?.action, "deny");
    f.agent
        .close_session(CloseSessionRequest::new(f.session_id.clone()))
        .await?;
    f.restart_round().await?;
    f.request("loaded", "confirm").await?;
    f.ready(&["loaded"]).await?;
    f.agent
        .load_session(LoadSessionRequest::new(
            f.session_id.clone(),
            std::env::current_dir()?,
        ))
        .await?;
    f.permission().await?;
    f.reply(ALLOW_OPTION_ID)?;
    assert_eq!(f.decision("loaded").await?.action, "approve");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_concurrent_attach_keeps_one_permission_queue() -> Result<()> {
    let Some(mut f) = Fixture::spawn().await? else {
        return Ok(());
    };
    f.request("attached", "confirm").await?;
    f.ready(&["attached"]).await?;
    f.permission().await?;
    let cwd = std::env::current_dir()?;
    let (loaded, resumed) = tokio::join!(
        f.agent
            .load_session(LoadSessionRequest::new(f.session_id.clone(), cwd.clone())),
        f.agent
            .resume_session(ResumeSessionRequest::new(f.session_id.clone(), cwd)),
    );
    loaded?;
    resumed?;
    f.quiet().await;
    assert_eq!(
        f.answer("attached", ALLOW_OPTION_ID).await?.action,
        "approve"
    );
    f.quiet().await;
    Ok(())
}
