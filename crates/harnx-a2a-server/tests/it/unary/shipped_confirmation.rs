//! Real Daedalus configuration through HTTP, durable worker suspension and A2A replies.
use super::{fixtures::*, hitl::assert::*};
use crate::support::{Harness, Script};
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::derive_pending_interactions;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_shipped_daedalus_approve_dispatches_handoff() -> Result<()> {
    decision(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_shipped_daedalus_deny_returns_script_error() -> Result<()> {
    decision(false).await
}

async fn decision(approved: bool) -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::ShippedDaedalus).await?).await?;
    let task = challenge(&http).await?;
    let before = entries(&http, &task).await?;
    let pending = derive_pending_interactions(&before)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request.kind, "confirm");
    assert_eq!(
        pending[0].summary(),
        "Hand off this plan to Atlas for execution?"
    );
    assert!(!before
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::HandoffCommitted { .. })));
    let response = http.rpc("SendMessage", json!({"message": reply(
        "shipped-decision", &task, if approved { "yes audit note" } else { "no deny note" }, true
    )})).await?;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["task"]["id"], task["id"]);
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    let log = entries(&http, &task).await?;
    assert_eq!(external_prompts(&log), 1);
    let answers = log
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse {
                interaction_id,
                request_seq,
                response,
                ..
            } => Some((interaction_id, request_seq, response)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].0, &pending[0].interaction_id);
    assert_eq!(*answers[0].1, pending[0].request_seq);
    assert_eq!(
        answers[0].2.action,
        if approved { "approve" } else { "deny" }
    );
    assert_eq!(
        answers[0].2.note.as_deref(),
        Some(if approved { "audit note" } else { "deny note" })
    );
    let results = log
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .expect("worker commits final result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "atlas_session_handoff");
    let handoffs = log.iter().filter(|(_, e)| matches!(e, SessionLogEntry::HandoffCommitted { target_agent, .. } if target_agent == "atlas@runner")).count();
    if approved {
        assert_eq!(
            handoffs,
            1,
            "results: {results:#?}; worker: {}",
            http.h.logs.text()
        );
    } else {
        assert_eq!(handoffs, 0);
        assert_eq!(results[0].output["is_error"], true);
        assert_eq!(
            results[0].output["error"],
            "Atlas handoff denied by the user."
        );
    }
    Ok(())
}
