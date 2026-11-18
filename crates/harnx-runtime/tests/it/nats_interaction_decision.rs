//! Integration tests for interaction response acceptance and pending derivation.

use crate::common::spawn_nats_server;
use anyhow::Result;
use harnx_core::{
    interaction::{InteractionResponse, InteractionStep},
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    interactions::{
        accept_interaction_response, derive_pending_interactions, InteractionDecisionOutcome,
    },
    nats_lease::{NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::{publish_control_command, ControlCommand, NatsSessionLogBackend},
};
use serde_json::json;
use std::sync::Arc;

#[path = "nats_interaction_decision/fixture.rs"]
mod fixture;

fn sample_interaction_request(seq: u64, round_seq: u64, id: &str) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::InteractionRequested {
            tool_round_seq: round_seq,
            tool_call_id: "call-1".into(),
            interaction_id: id.into(),
            producer: "middleware:integration".into(),
            step_path: vec![InteractionStep {
                middleware: "integration".into(),
                key: "gate".into(),
            }],
            key: "gate".into(),
            kind: "confirm".into(),
            payload: json!({"action": "deploy", "target": "stage"}),
            summary: "Confirm deployment action".into(),
            chain_version: "v1".into(),
            checkpoint: "cp-int".into(),
            fence_token: 100,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_acceptance_nats_lifecycle() -> Result<()> {
    require_nextest();
    let fixture::AcceptanceFixture {
        _server,
        js,
        backend,
        lease,
        log,
    } = fixture::AcceptanceFixture::start().await?;

    // Manifest: int-deploy is pending
    let ready_entry = SessionLogEntry::SuspensionReady {
        tool_round_seq: 1,
        pending: vec!["int-deploy".into()],
        fence_token: lease.fence_token(),
    };
    let resp = InteractionResponse {
        action: "approve".into(),
        data: serde_json::Map::new(),
        note: Some("proceed".into()),
    };

    assert!(derive_pending_interactions(&log.load_events_async().await?)?.is_empty());
    assert_eq!(
        accept_interaction_response(&backend, &lease, "int-deploy", 2, &resp).await?,
        InteractionDecisionOutcome::Stale
    );
    log.append_event_async(&ready_entry).await?;

    // 1. Wrong sequence
    let outcome = accept_interaction_response(&backend, &lease, "int-deploy", 999, &resp).await?;
    assert_eq!(outcome, InteractionDecisionOutcome::WrongSequence);

    // 2. Unknown / stale ID
    let outcome = accept_interaction_response(&backend, &lease, "int-other", 2, &resp).await?;
    assert_eq!(outcome, InteractionDecisionOutcome::Stale);

    // Concurrent identical submissions produce one append, including CAS loss.
    let (first, second) = tokio::join!(
        accept_interaction_response(&backend, &lease, "int-deploy", 2, &resp),
        accept_interaction_response(&backend, &lease, "int-deploy", 2, &resp),
    );
    let outcomes = [first?, second?];
    assert!(outcomes.contains(&InteractionDecisionOutcome::Applied));
    assert!(outcomes.contains(&InteractionDecisionOutcome::AlreadyApplied));

    assert_retries_until_complete(&backend, &lease, &log, &resp, ready_entry).await?;
    let entries = log.load_events_async().await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::InteractionResponse { .. }))
            .count(),
        1
    );
    assert_broker_fence_rejects(&js, &backend, &lease, &resp).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_control_command_wire() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let session_id = "test-interaction-control-wire";
    let mut control_sub = client
        .subscribe(harnx_runtime::nats_worker::control_subject(session_id))
        .await?;

    let cmd = ControlCommand::InteractionResponse {
        interaction_id: "int-cmd".into(),
        request_seq: 15,
        response: InteractionResponse {
            action: "confirm".into(),
            data: serde_json::Map::new(),
            note: None,
        },
    };

    publish_control_command(&client, session_id, &cmd).await?;

    use futures_util::StreamExt;
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), control_sub.next())
        .await?
        .expect("control command received");

    let received: ControlCommand = serde_json::from_slice(&msg.payload)?;
    assert_eq!(cmd, received);
    Ok(())
}

#[test]
fn pending_interactions_lifecycle_derivation() -> Result<()> {
    let call_1 = ToolCall::new("deploy".into(), json!({}), Some("call-1".into()), None);
    let entries = vec![
        (
            1,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![call_1],
                timestamp: None,
                fence_token: Some(10),
            },
        ),
        sample_interaction_request(2, 1, "int-deploy"),
        (
            3,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 1,
                pending: vec!["int-deploy".into()],
                fence_token: 10,
            },
        ),
    ];

    let pending = derive_pending_interactions(&entries)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].interaction_id, "int-deploy");
    assert_eq!(pending[0].request_seq, 2);
    Ok(())
}

async fn assert_broker_fence_rejects(
    js: &async_nats::jetstream::Context,
    backend: &NatsSessionLogBackend,
    lease: &Arc<NatsSessionLease>,
    response: &InteractionResponse,
) -> Result<()> {
    let config = NatsLeaseConfig::default();
    let bucket = js.get_key_value(&config.bucket).await?;
    let key = config.key_for_session(backend.session_id());
    let entry = bucket.entry(&key).await?.expect("held lease record");
    let mut owner: harnx_runtime::nats_lease::LeaseRecord = serde_json::from_slice(&entry.value)?;
    owner.worker_id = "replacement-worker".into();
    owner.generation += 1;
    // Replace broker ownership while the old handle still believes it's held.
    bucket
        .update(&key, serde_json::to_vec(&owner)?.into(), entry.revision)
        .await?;
    assert!(lease.is_held());
    assert_eq!(
        accept_interaction_response(backend, lease, "int-deploy", 2, response).await?,
        InteractionDecisionOutcome::LeaseLost
    );
    Ok(())
}

async fn assert_retries_until_complete(
    backend: &NatsSessionLogBackend,
    lease: &Arc<NatsSessionLease>,
    log: &NatsSessionLog,
    response: &InteractionResponse,
    mut ready: SessionLogEntry,
) -> Result<()> {
    // An identical retry remains valid after the pending manifest advances.
    if let SessionLogEntry::SuspensionReady {
        ref mut pending, ..
    } = ready
    {
        *pending = vec!["int-reprompt".into()];
    }
    log.append_event_async(&ready).await?;
    assert_eq!(
        accept_interaction_response(backend, lease, "int-deploy", 2, response).await?,
        InteractionDecisionOutcome::AlreadyApplied
    );
    let conflict = InteractionResponse {
        action: "deny".into(),
        note: None,
        ..response.clone()
    };
    assert_eq!(
        accept_interaction_response(backend, lease, "int-deploy", 2, &conflict).await?,
        InteractionDecisionOutcome::Conflicting
    );
    log.append_event_async(&SessionLogEntry::ToolResults {
        results: vec![],
        handoff_selection: None,
        timestamp: None,
    })
    .await?;
    assert_eq!(
        accept_interaction_response(backend, lease, "int-deploy", 2, response).await?,
        InteractionDecisionOutcome::AfterComplete
    );
    Ok(())
}
