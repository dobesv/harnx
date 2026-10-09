//! Multi-replica admission and isolation regressions against real worker/NATS.
//! Enable with --features fault-injection. Both replicas retain separate local
//! gates, dedupe LRUs and runner slots, sharing only broker/config/worker.
use crate::support::{alice, Harness, Script, DEADLINE};
use anyhow::{Context, Result};
use axum::{body::Body, http::Request, Router};
use harnx_a2a_server::{
    fault_injection::Boundary,
    handler::{Backend, BackendConfig, HarnxHandler},
    input_map::InputLimits,
    routes,
    runner::{ContextKey, Runner, SessionRequest},
    store::{A2aStore, TaskAccess},
};
use harnx_core::{message::MessageRole, session::SessionLogEntry};
use harnx_runtime::{nats_session_log::NatsSessionLog, NatsSession, SessionActivationRoute};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
mod admission;
mod auth;
mod operations;
mod streaming;
mod supervision;

struct Replica {
    backend: Arc<Backend>,
    app: Router,
}
impl Replica {
    fn new(h: &Harness, store: Arc<A2aStore>, runner: Arc<Runner>) -> Result<Self> {
        let backend = Arc::new(Backend::new(
            runner,
            store,
            BackendConfig {
                config: h.config.clone(),
                route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            },
        ));
        let mut export = h.export.clone();
        export.lookup_keys = vec![export.public_name.clone()];
        let app = routes::router(
            &[export],
            None,
            &["X-User-ID".into()],
            |export, identity| {
                Arc::new(HarnxHandler::new(
                    export.clone(),
                    identity,
                    backend.clone(),
                    InputLimits::default(),
                ))
            },
        )?;
        Ok(Self { backend, app })
    }
    async fn rpc(&self, user: &str, method: &str, params: Value) -> Result<Value> {
        rpc(self.app.clone(), user, method, params).await
    }
}

async fn rpc(app: Router, user: &str, method: &str, params: Value) -> Result<Value> {
    let request = Request::post("/agents/runner")
        .header("Content-Type", "application/json")
        .header("X-User-ID", user)
        .body(Body::from(serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )?))?;
    let response = app.oneshot(request).await?;
    assert!(response.status().is_success());
    Ok(serde_json::from_slice(
        &response.into_body().collect().await?.to_bytes(),
    )?)
}
fn first_message() -> Value {
    json!({"message":{"messageId":"same-first-message","role":"ROLE_USER","parts":[{"text":"Hello, agent"}]},
           "configuration":{"returnImmediately":true}})
}
fn result_task(response: &Value) -> Result<&Value> {
    anyhow::ensure!(response.get("error").is_none(), "RPC failed: {response}");
    response
        .get("result")
        .and_then(|result| result.get("task"))
        .context("missing task")
}

struct TwoBackends {
    a: Replica,
    b: Replica,
    h: Harness,
}
impl TwoBackends {
    async fn start() -> Result<Self> {
        let h = Harness::start(Script::Text).await?;
        let a = Replica::new(&h, h.store.clone(), h.runner.clone())?;
        let store_b = Arc::new(A2aStore::new(h.metadata.clone()));
        let b = Replica::new(&h, store_b.clone(), Runner::new(store_b))?;
        assert!(!Arc::ptr_eq(&a.backend.runner, &b.backend.runner));
        assert!(!Arc::ptr_eq(&a.backend.store, &b.backend.store));
        Ok(Self { a, b, h })
    }
    async fn resume(&self, replica: &Replica, context: &str) -> Result<NatsSession> {
        replica
            .backend
            .runner
            .session(SessionRequest {
                export: &self.h.export,
                owner: &alice().into(),
                local_id: Some(context),
                global_config: &self.h.config,
                activation_route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            })
            .await
    }
}
async fn prompts(log: &NatsSessionLog) -> Result<Vec<(u64, String)>> {
    Ok(log
        .load_events_async()
        .await?
        .into_iter()
        .filter_map(|(seq, entry)| match entry {
            SessionLogEntry::Message {
                role: MessageRole::User,
                id: Some(id),
                ..
            } => Some((seq, id)),
            _ => None,
        })
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_first_messages_share_context_task_prompt_and_execution() -> Result<()> {
    tokio::time::timeout(DEADLINE, duplicate_first_messages())
        .await
        .context("duplicate admission deadline")?
}
async fn duplicate_first_messages() -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks_a = t.a.backend.runner.fault_hooks();
    let hooks_b = t.b.backend.runner.fault_hooks();
    let mut miss_a = hooks_a.pause(Boundary::DedupeMiss, 1);
    let mut miss_b = hooks_b.pause(Boundary::DedupeMiss, 1);
    let mut output_a = hooks_a.pause(Boundary::Publication, 2);
    let mut output_b = hooks_b.pause(Boundary::Publication, 2);
    let app_a = t.a.app.clone();
    let app_b = t.b.app.clone();
    let send_a =
        tokio::spawn(async move { rpc(app_a, "alice", "SendMessage", first_message()).await });
    let send_b =
        tokio::spawn(async move { rpc(app_b, "alice", "SendMessage", first_message()).await });
    tokio::join!(miss_a.reached(), miss_b.reached());
    assert_eq!(hooks_a.count(Boundary::Claim), 0);
    assert_eq!(hooks_b.count(Boundary::Claim), 0);
    // Both real handlers passed their own dedupe gate before either allocated a session.
    drop((miss_a, miss_b));
    let response_a = send_a.await??;
    let response_b = send_b.await??;
    let task_a = result_task(&response_a)?;
    let task_b = result_task(&response_b)?;
    let contexts = (&task_a["contextId"], &task_b["contextId"]);
    assert_eq!(contexts.0, contexts.1, "shared context identity");
    let ids = (&task_a["id"], &task_b["id"]);
    assert_eq!(ids.0, ids.1, "shared task identity");
    for (replica, task) in [(&t.a, task_a), (&t.b, task_b)] {
        assert_durable_prompt(&t, replica, task).await?;
    }
    // Both independent replicas follow the same retained winner.
    let retry_a = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let retry_b = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry_a)?["id"], task_a["id"]);
    assert_eq!(result_task(&retry_b)?["id"], task_b["id"]);
    assert_eq!(hooks_a.count(Boundary::DedupeMiss), 1);
    assert_eq!(hooks_b.count(Boundary::DedupeMiss), 1);
    if hooks_a.count(Boundary::BeforeAdmission) == 1 {
        output_a.reached().await;
    } else {
        output_b.reached().await;
    }
    assert_eq!(
        t.h.llm.requests.lock().len(),
        1,
        "one retained message identity executes once"
    );
    drop((output_a, output_b));
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_reconciliation_does_not_cancel_an_active_owner_turn() -> Result<()> {
    tokio::time::timeout(DEADLINE, sibling_orphan())
        .await
        .context("sibling orphan deadline")?
}
async fn sibling_orphan() -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks_a = t.a.backend.runner.fault_hooks();
    // First publication is Working, second is the first artifact. Hold A after
    // actual worker output, so B's cancellation/status CAS cannot race A's writer.
    let mut output = hooks_a.pause(Boundary::Publication, 2);
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&response)?;
    let context = task["contextId"].as_str().context("context")?;
    let id = task["id"].as_str().context("task")?;
    output.reached().await;
    assert_eq!(
        t.h.llm.requests.lock().len(),
        1,
        "real worker reached scripted LLM"
    );
    let key = ContextKey::new(&t.h.export, context);
    assert!(t.a.backend.runner.is_busy(&key).await);
    assert!(!t.b.backend.runner.is_busy(&key).await);
    let session_b = t.resume(&t.b, context).await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session_b.storage_key());
    assert!(!log
        .load_events_async()
        .await?
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    reconcile_live_sibling(&t, session_b, id, &log).await?;
    drop(output);
    t.a.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_hooks_bound_real_claim_prompt_append_and_publication() -> Result<()> {
    tokio::time::timeout(DEADLINE, boundary_sequence())
        .await
        .context("boundary sequence deadline")?
}
async fn boundary_sequence() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let session = h.session(None, &alice()).await?;
    let log = NatsSessionLog::new(h.jetstream.clone(), session.storage_key());
    let hooks = h.runner.fault_hooks();
    let mut claim = hooks.pause(Boundary::Claim, 1);
    let mut publication = hooks.pause(Boundary::Publication, 1);
    let mut before = hooks.pause(Boundary::BeforeAdmission, 1);
    let mut after = hooks.pause(Boundary::AfterAdmission, 1);
    let runner = h.runner.clone();
    let export = h.export.clone();
    let turn_session = session.clone();
    let send = tokio::spawn(async move {
        runner
            .start_turn(
                harnx_a2a_server::runner::TurnRequest {
                    export: &export,
                    owner: &alice().into(),
                    session: turn_session,
                    message: serde_json::from_value(first_message()["message"].clone())?,
                },
                InputLimits::default(),
            )
            .await
    });
    claim.reached().await;
    assert_eq!(hooks.count(Boundary::Publication), 0);
    assert!(prompts(&log).await?.is_empty());
    drop(claim);
    publication.reached().await;
    let task_id = assert_before_publication(&h, &session).await?;
    drop(publication);
    before.reached().await;
    assert!(prompts(&log).await?.is_empty());
    drop(before);
    after.reached().await;
    assert_after_append(&h, &session, &log, &task_id).await?;
    drop(after);
    let admitted = send.await??;
    assert!(admitted.snapshot.user_msg_seq > 0);
    for boundary in [
        Boundary::Claim,
        Boundary::BeforeAdmission,
        Boundary::AfterAdmission,
    ] {
        assert_eq!(hooks.count(boundary), 1, "{boundary:?}");
    }
    h.runner.shutdown().await;
    Ok(())
}

async fn assert_durable_prompt(t: &TwoBackends, replica: &Replica, task: &Value) -> Result<()> {
    let session = t
        .resume(replica, task["contextId"].as_str().context("context")?)
        .await?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), session.storage_key());
    let prompt = prompts(&log).await?;
    assert_eq!(
        prompt.len(),
        1,
        "shared context has one real durable prompt"
    );
    let record = replica
        .backend
        .store
        .get_task(
            session.storage_key(),
            task["id"].as_str().context("task id")?,
        )
        .await?
        .context("persisted task")?;
    assert_eq!((record.user_msg_seq, record.user_msg_id), prompt[0].clone());
    assert_eq!(
        record.task.history.as_ref().context("history")?[0].message_id,
        "same-first-message"
    );
    Ok(())
}

async fn reconcile_live_sibling(
    t: &TwoBackends,
    session_b: NatsSession,
    id: &str,
    log: &NatsSessionLog,
) -> Result<()> {
    let key = ContextKey::new(&t.h.export, session_b.session_id());
    let record =
        t.b.backend
            .runner
            .reconcile_orphan(
                TaskAccess {
                    export: &t.h.export,
                    owner: &alice().into(),
                    task_id: id,
                },
                &session_b,
            )
            .await?;
    assert!(t.a.backend.runner.is_busy(&key).await);
    assert_eq!(record.task.status.state, a2a_lf::TaskState::Working);
    let entries = log.load_events_async().await?;
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    assert_eq!(prompts(log).await?.len(), 1);
    assert_eq!(
        t.b.backend
            .runner
            .fault_hooks()
            .count(Boundary::OrphanCancel),
        0
    );
    Ok(())
}

async fn assert_before_publication(h: &Harness, session: &NatsSession) -> Result<String> {
    let tasks = crate::support::list_all_tasks_for_test(
        &h.store,
        &h.export,
        &alice(),
        session.session_id(),
    )
    .await?
    .context("tasks")?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].task.status.state, a2a_lf::TaskState::Submitted);
    let admissions = h.runner.fault_hooks().count(Boundary::BeforeAdmission);
    assert_eq!(admissions, 0);
    let requests = h.llm.requests.lock();
    assert!(requests.is_empty());
    Ok(tasks[0].task.id.clone())
}

async fn assert_after_append(
    h: &Harness,
    session: &NatsSession,
    log: &NatsSessionLog,
    task_id: &str,
) -> Result<()> {
    assert_eq!(
        prompts(log).await?.len(),
        1,
        "append completed before admission projection"
    );
    let record = h
        .store
        .get_task(session.storage_key(), task_id)
        .await?
        .context("task")?;
    assert_eq!(
        record.user_msg_seq, 0,
        "projection hasn't run at after-append boundary"
    );
    assert!(
        h.llm.requests.lock().is_empty(),
        "follow/worker activation hasn't run"
    );
    Ok(())
}
