use super::*;
use crate::unary::{hitl::reply, Http};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_hitl_saved_reply_decision_mismatch_fails_closed() -> Result<()> {
    let (http, session, context, answer) = mismatch_fixture().await?;
    assert_reply_request_failed(&http, &answer).await?;
    assert_get_task_input_required(&http, &session, &context).await?;
    assert_retry_outcomes(&http, &session, &context, &answer).await?;
    assert_cancel_recovers(&http, &session, &context).await?;
    http.h.runner.shutdown().await;
    Ok(())
}

async fn mismatch_fixture() -> Result<(Http, NatsSession, ContextSnapshot, Value)> {
    let h = Harness::start(Script::Tool).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let task = serde_json::json!(started.snapshot.task);
    let answer = reply("saved-no", &task, "y saved full reason", false);
    // Fixture records a rejection receipt; history has matching fingerprint but approves.
    seed_saved_intent_message(&h, &session, &context, &answer).await?;
    let http = Http::from_harness(h).await?;
    Ok((http, session, context, answer))
}

async fn assert_reply_request_failed(http: &Http, answer: &Value) -> Result<()> {
    let response = http
        .rpc("SendMessage", serde_json::json!({"message": answer}))
        .await?;
    assert_eq!(response["error"]["code"], -32603, "{response}");
    assert_eq!(response["error"]["message"], "request failed", "{response}");
    Ok(())
}

async fn assert_get_task_input_required(
    http: &Http,
    session: &NatsSession,
    original: &ContextSnapshot,
) -> Result<()> {
    let response = http
        .rpc("GetTask", serde_json::json!({"id": task_id(original)}))
        .await?;
    assert_eq!(response["result"]["id"], task_id(original), "{response}");
    assert_eq!(
        response["result"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    assert_saved_intent_undelivered(http, session, original).await?;
    assert_decision_log(http, session, false).await
}

async fn assert_retry_outcomes(
    http: &Http,
    session: &NatsSession,
    original: &ContextSnapshot,
    answer: &Value,
) -> Result<()> {
    assert_reply_request_failed(http, answer).await?;
    assert_get_task_input_required(http, session, original).await?;
    let mut different = answer.clone();
    different["messageId"] = "different-reply".into();
    let response = http
        .rpc("SendMessage", serde_json::json!({"message": different}))
        .await?;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_get_task_input_required(http, session, original).await
}

async fn assert_cancel_recovers(
    http: &Http,
    session: &NatsSession,
    original: &ContextSnapshot,
) -> Result<()> {
    let params = serde_json::json!({"id": task_id(original)});
    let cancel = http.rpc("CancelTask", params.clone()).await?;
    assert_eq!(cancel["result"]["id"], task_id(original), "{cancel}");
    assert_eq!(
        cancel["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancel}"
    );
    let get = http.rpc("GetTask", params).await?;
    assert_eq!(
        get["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{get}"
    );
    let original_ticket = ticket(original, session.storage_key())?;
    assert!(matches!(
        session.fixed_prompt_progress(&original_ticket).await?,
        PromptProgress::Terminal(result) if result.was_cancelled
    ));
    assert_decision_log(http, session, true).await?;
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

fn task_id(context: &ContextSnapshot) -> &str {
    &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id
}

async fn assert_decision_log(http: &Http, session: &NatsSession, canceled: bool) -> Result<()> {
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
            .load_events_latest_required_async()
            .await?;
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. })));
    assert_eq!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        canceled
    );
    Ok(())
}

async fn assert_saved_intent_undelivered(
    http: &Http,
    session: &NatsSession,
    original: &ContextSnapshot,
) -> Result<()> {
    let original_ticket = ticket(original, session.storage_key())?;
    assert!(matches!(
        session.fixed_prompt_progress(&original_ticket).await?,
        PromptProgress::InputRequired(_)
    ));
    let current = http
        .h
        .store
        .read_context(session.storage_key())
        .await?
        .unwrap();
    assert!(current.document.owner.is_none());
    let active = current.document.state.active.as_ref().unwrap();
    let checkpoint = active.hitl_checkpoint.as_ref().unwrap();
    let original_checkpoint = original
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap();
    assert_eq!(active.snapshot.task.status.state, TaskState::InputRequired);
    assert_eq!(checkpoint.phase, HitlPhase::DecisionIntent);
    assert_eq!(checkpoint.request, original_checkpoint.request);
    assert!(!checkpoint.reply.as_ref().unwrap().approved);
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}
