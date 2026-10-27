use super::fixtures::*;
use crate::support::{alice, Harness, Script, DEADLINE};
use a2a_lf::TaskState;
use anyhow::{Context, Result};
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    input_map::InputLimits,
};
use harnx_runtime::NatsSession;
use harnx_runtime::SessionActivationRoute;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

async fn wait_for_blocking_send_task(
    http: &Http,
    send: &mut (impl std::future::Future<Output = Result<Value>> + Unpin),
    session: &NatsSession,
) -> Result<String> {
    tokio::select! {
        result = send => anyhow::bail!("blocking send returned early: {result:?}"),
        result = tokio::time::timeout(DEADLINE, http.h.llm.requested.notified()) => { result?; },
    }
    let authority = http
        .h
        .store
        .read_context(session.storage_key())
        .await?
        .context("authority")?;
    let id = authority
        .document
        .state
        .active
        .context("admitted task")?
        .snapshot
        .task
        .id;
    Ok(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_blocking_send_returns_input_required_without_terminal_result() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;
    let msg = message("input-required", Some(&json!(session.session_id())));
    let send = http.rpc("SendMessage", json!({"message": msg}));
    tokio::pin!(send);
    let id = wait_for_blocking_send_task(&http, &mut send, &session).await?;
    let requests = http.h.llm.requests.lock().len();
    let interrupted = inject_response_state(&http.h, &id, TaskState::InputRequired).await?;
    let response = tokio::time::timeout(DEADLINE, &mut send).await??;
    assert_eq!(response["result"]["task"]["id"], id);
    assert_eq!(
        response["result"]["task"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
    assert!(!interrupted.task.status.state.is_terminal());
    assert_eq!(http.h.llm.requests.lock().len(), requests);
    Ok(())
}

fn boundary_test_handler(h: &Harness) -> HarnxHandler {
    let backend = Arc::new(Backend::new(
        h.runner.clone(),
        h.store.clone(),
        BackendConfig {
            config: h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: harnx_core::abort::create_abort_signal(),
        },
    ));
    HarnxHandler::new(
        h.export.clone(),
        Default::default(),
        backend,
        InputLimits::default(),
    )
}

async fn append_user_reply_history(
    h: &Harness,
    storage_key: &str,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    let context = h
        .store
        .read_context(storage_key)
        .await?
        .context("authority")?;
    let write = h
        .store
        .prepare_context_update(
            storage_key,
            &context.version()?,
            &uuid::Uuid::new_v4().to_string(),
            |state| {
                state
                    .active
                    .as_mut()
                    .unwrap()
                    .snapshot
                    .task
                    .history
                    .get_or_insert_default()
                    .push(a2a_lf::Message::new(
                        a2a_lf::Role::User,
                        vec![a2a_lf::Part::text("y")],
                    ));
            },
        )
        .await?;
    Ok(h.store
        .commit_context(&write)
        .await?
        .document
        .state
        .active
        .unwrap()
        .snapshot)
}

struct InitialBoundarySetup {
    h: Harness,
    id: String,
    q1: harnx_a2a_server::store::TaskRecord,
}

async fn setup_initial_boundary() -> Result<InitialBoundarySetup> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let admitted = h
        .send(
            &session,
            a2a_lf::Message::new(
                a2a_lf::Role::User,
                vec![a2a_lf::Part::text("boundary cursor")],
            ),
        )
        .await?;
    let id = admitted.snapshot.task.id;
    let q1 = inject_response_state(&h, &id, TaskState::InputRequired).await?;
    // Reply history/intent writes aren't a new response boundary.
    let updated = append_user_reply_history(&h, session.storage_key()).await?;
    assert!(updated.revision > q1.revision);
    assert_eq!(updated.stream_seq, q1.stream_seq);
    Ok(InitialBoundarySetup { h, id, q1 })
}

struct BoundaryWaiter<'a> {
    handler: &'a HarnxHandler,
    owner: &'a harnx_a2a_server::identity::RequestIdentity,
    id: &'a str,
}

async fn observe_next_boundary(
    h: &Harness,
    waiter: BoundaryWaiter<'_>,
    q1_seq: u64,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    let waiting = waiter
        .handler
        .wait_response(waiter.owner, waiter.id, Some(q1_seq));
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => anyhow::bail!("returned stale challenge: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {},
    }
    inject_response_state(h, waiter.id, TaskState::Working).await?;
    let q2 = inject_response_state(h, waiter.id, TaskState::AuthRequired).await?;
    let observed = tokio::time::timeout(DEADLINE, &mut waiting).await??;
    assert_eq!(observed.task.status.state, TaskState::AuthRequired);
    assert_eq!(observed.stream_seq, q2.stream_seq);
    Ok(q2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_boundary_after_skips_stale_input_required_and_observes_next() -> Result<()> {
    let setup = setup_initial_boundary().await?;
    let handler = boundary_test_handler(&setup.h);
    let owner = alice().into();
    assert_eq!(
        handler
            .wait_response(&owner, &setup.id, None)
            .await?
            .stream_seq,
        setup.q1.stream_seq
    );
    let waiter = BoundaryWaiter {
        handler: &handler,
        owner: &owner,
        id: &setup.id,
    };
    let q2 = observe_next_boundary(&setup.h, waiter, setup.q1.stream_seq).await?;
    // Next boundary can already be durable before waiter subscribes.
    assert_eq!(
        handler
            .wait_response(&owner, &setup.id, Some(setup.q1.stream_seq))
            .await?
            .stream_seq,
        q2.stream_seq
    );
    inject_response_state(&setup.h, &setup.id, TaskState::Completed).await?;
    assert_eq!(
        handler
            .wait_response(&owner, &setup.id, Some(u64::MAX))
            .await?
            .task
            .status
            .state,
        TaskState::Completed
    );
    assert_eq!(setup.h.llm.requests.lock().len(), 1);
    Ok(())
}
