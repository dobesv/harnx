use super::*;
use crate::unary::{hitl::reply, Http};
use harnx_a2a_server::fault_injection::Boundary;
use harnx_a2a_server::fault_injection::Pause;
use harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket;

use serde_json::{json, Value};

pub(super) async fn wait_progress_boundary(
    session: &NatsSession,
    fixed: &FixedAdmissionTicket,
    request_seq: u64,
    terminal: bool,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match session.fixed_prompt_progress(fixed).await? {
                PromptProgress::Terminal(_) if terminal => return Ok::<_, anyhow::Error>(()),
                PromptProgress::InputRequired(p) if !terminal && p.request_seq > request_seq => {
                    return Ok(())
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await?
}

async fn assert_external_reply_result(
    http: &Http,
    session: &NatsSession,
    response: &Value,
    terminal: bool,
) -> Result<()> {
    assert_eq!(
        response["result"]["task"]["status"]["state"],
        if terminal {
            "TASK_STATE_COMPLETED"
        } else {
            "TASK_STATE_INPUT_REQUIRED"
        },
        "{response}"
    );
    let entries =
        NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), session.storage_key(), 1)
            .load_events_latest_required_async()
            .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "external-race")
        .await?
        .is_some());
    Ok(())
}

struct ExternalReplySetup {
    http: Http,
    session: NatsSession,
    fixed: FixedAdmissionTicket,
    request: ApprovalRef,
    task: Value,
}

async fn setup_external_reply(script: Script) -> Result<ExternalReplySetup> {
    let h = Harness::start(script).await?;
    let session = h.session(None, &alice()).await?;
    let (started, context) = challenge(&h, &session).await?;
    let fixed = ticket(&context, session.storage_key())?;
    let request = context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request
        .clone();
    let http = Http::from_harness(h).await?;
    let task = serde_json::to_value(&started.snapshot.task)?;
    Ok(ExternalReplySetup {
        http,
        session,
        fixed,
        request,
        task,
    })
}

async fn wait_for_intent_committed(
    http: &Http,
    send: &mut (impl std::future::Future<Output = Result<Value>> + Unpin),
    committed: &mut Pause,
    session: &NatsSession,
) -> Result<()> {
    tokio::select! {
        response = send => anyhow::bail!("reply returned before intent: {response:?}"),
        reached = tokio::time::timeout(DEADLINE, committed.reached()) => { reached?; }
    }
    assert!(http
        .h
        .store
        .get_reply_dedupe(session.storage_key(), "external-race")
        .await?
        .is_none());
    Ok(())
}

pub(super) async fn reply_after_external_decision(script: Script, terminal: bool) -> Result<()> {
    let setup = setup_external_reply(script).await?;
    let mut committed = setup
        .http
        .h
        .runner
        .fault_hooks()
        .pause(Boundary::HitlReplyIntentCommitted, 1);
    let send = setup.http.rpc(
        "SendMessage",
        json!({"message": reply("external-race", &setup.task, "y", false)}),
    );
    tokio::pin!(send);
    wait_for_intent_committed(&setup.http, &mut send, &mut committed, &setup.session).await?;
    setup
        .session
        .decide_hitl_approval_for(&setup.request, false, Some("external decision".into()))
        .await?;
    wait_progress_boundary(
        &setup.session,
        &setup.fixed,
        setup.request.request_seq,
        terminal,
    )
    .await?;
    drop(committed);
    let response = send.await?;
    assert_eq!(
        response["result"]["task"]["id"], setup.task["id"],
        "{response}"
    );
    assert_external_reply_result(&setup.http, &setup.session, &response, terminal).await
}
