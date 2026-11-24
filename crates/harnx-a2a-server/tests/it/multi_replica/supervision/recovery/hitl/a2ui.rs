//! Recovery reparses the DataPart saved with the exact receipt, not a text summary.
use super::*;
use crate::support::a2ui::{action_reply, cases, data_part};

fn task_value(context: &ContextSnapshot) -> Value {
    serde_json::to_value(
        &context
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .snapshot
            .task,
    )
    .unwrap()
}

async fn assert_response(t: &TwoBackends, session: &NatsSession, note: Option<&str>) -> Result<()> {
    let entries = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::InteractionResponse { response, .. }
        if response.action == "deploy" && &response.data == cases()[0]["expected"].as_object().unwrap()
            && response.note.as_deref() == note)));
    Ok(())
}

async fn recover_reply(boundary: Boundary) -> Result<()> {
    let t = TwoBackends::start_script(Script::A2uiTool).await?;
    let (session, before) = start_parked(&t).await?;
    let task = task_value(&before);
    let mut params = action_reply(
        "recovery-data",
        &task,
        "deploy",
        cases()[0]["values"].clone(),
    );
    // Receipt notes are bounded; recovery must read the original full DataPart.
    let note = "Full recovery note ".repeat(100);
    params["message"]["parts"][0]["data"]["action"]["metadata"]["extensions"]["harnx_response"]
        ["note"] = json!(note);
    let mut pause = t.a.backend.runner.fault_hooks().pause(boundary, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", params).await }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
    interaction::assert_completed(&t, &session, &before).await?;
    assert_response(&t, &session, Some(&note)).await?;
    drop(pause);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_a2ui_saved_datapart_intent_delivered_by_new_replica() -> Result<()> {
    recover_reply(Boundary::HitlReplyIntentCommitted).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_a2ui_custom_action_committed_before_following() -> Result<()> {
    recover_reply(Boundary::HitlDecisionCommitted).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_a2ui_parked_challenge_reply_on_new_replica() -> Result<()> {
    let t = TwoBackends::start_script(Script::A2uiTool).await?;
    let (session, before) = start_parked(&t).await?;
    t.a.backend.runner.shutdown().await;
    assert_readonly_inspection(&t, &session, before.revision).await?;
    let task = task_value(&before);
    let response =
        t.b.rpc(
            "alice",
            "SendMessage",
            action_reply("replacement", &task, "deploy", cases()[0]["values"].clone()),
        )
        .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{response}"
    );
    interaction::assert_completed(&t, &session, &before).await?;
    assert_response(&t, &session, None).await?;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_a2ui_uncheckpointed_surface_rebuilt_on_new_replica() -> Result<()> {
    let t = TwoBackends::start_script(Script::A2uiTool).await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::HitlRequestObserved, 1);
    let (session, _) = interaction::begin(&t).await?;
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    remove_owner_lease(&t, session.storage_key()).await?;
    supervise(&t, &t.b);
    let before = parked(&t, session.storage_key(), 0).await?;
    let restored = task_value(&before);
    assert_eq!(data_part(&restored)["data"]["version"], "1.0");
    let response =
        t.b.rpc(
            "alice",
            "SendMessage",
            reply(&before, "deploy choice=prod name=rel-1"),
        )
        .await?;
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_COMPLETED",
        "{response}"
    );
    interaction::assert_completed(&t, &session, &before).await?;
    assert_response(&t, &session, None).await?;
    drop(pause);
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
