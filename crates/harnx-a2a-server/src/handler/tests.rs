use super::*;
use crate::runner_test_support::{alice, Harness, Script, DEADLINE};
use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};
use tokio_util::task::AbortOnDropHandle;

pub(super) struct CancelSnapshotGate {
    captured: parking_lot::Mutex<Option<oneshot::Sender<TaskState>>>,
    resume: Notify,
}

impl CancelSnapshotGate {
    pub(super) async fn after_snapshot(&self, state: TaskState) {
        let captured = self.captured.lock().take();
        if let Some(captured) = captured {
            captured.send(state).expect("snapshot observer");
            self.resume.notified().await;
        }
    }
}

async fn start_cancel_http(
    h: &Harness,
    gate: Arc<CancelSnapshotGate>,
) -> Result<(String, AbortOnDropHandle<()>)> {
    let backend = Arc::new(Backend::new(
        h.runner.clone(),
        h.store.clone(),
        BackendConfig {
            config: h.config.clone(),
            route: harnx_runtime::SessionActivationRoute::ClusterShared,
            abort: harnx_core::abort::create_abort_signal(),
        },
    ));
    let mut export = h.export.clone();
    export.lookup_keys = vec![export.public_name.clone()];
    let app = crate::routes::router(
        std::slice::from_ref(&export),
        None,
        &["X-User-ID".into()],
        |export, identity| {
            let mut handler = HarnxHandler::new(
                export.clone(),
                identity,
                backend.clone(),
                InputLimits::default(),
            );
            handler.cancel_snapshot_gate = Some(gate.clone());
            Arc::new(handler)
        },
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/agents/runner", listener.local_addr()?);
    let server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));

    Ok((url, server))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_http_cancel_returns_completed_when_completion_wins_after_snapshot() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let (captured_tx, captured_rx) = oneshot::channel();
    let gate = Arc::new(CancelSnapshotGate {
        captured: parking_lot::Mutex::new(Some(captured_tx)),
        resume: Notify::new(),
    });
    let (url, _server) = start_cancel_http(&h, gate.clone()).await?;

    let session = h.session(None, &alice()).await?;
    let started = h
        .send(
            &session,
            Message::new(Role::User, vec![Part::text("Hello, agent")]),
        )
        .await?;
    let id = started.snapshot.task.id.clone();
    tokio::time::timeout(DEADLINE, h.llm.requested.notified()).await?;
    let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
    let mut cancel = AbortOnDropHandle::new(tokio::spawn(async move {
        client.post(url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", "alice")
            .json(&json!({"jsonrpc": "2.0", "id": "cancel-race", "method": "CancelTask", "params": {"id": id}}))
            .send().await
    }));

    // The HTTP handler has read a nonterminal snapshot but hasn't reconciled it.
    let initial = tokio::time::timeout(DEADLINE, captured_rx).await??;
    assert_eq!(initial, TaskState::Working);
    assert!(
        !cancel.is_finished(),
        "CancelTask must still be held at the snapshot gate"
    );
    h.llm.release.notify_one();
    let completed = tokio::time::timeout(DEADLINE, async {
        loop {
            let record = h.task(&started.snapshot.task.id).await?;
            if record.task.status.state == TaskState::Completed {
                return Ok::<_, anyhow::Error>(record.task);
            }
            assert_eq!(record.task.status.state, TaskState::Working);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert!(
        !cancel.is_finished(),
        "completion must land before reconciliation resumes"
    );
    gate.resume.notify_one();

    let response = tokio::time::timeout(DEADLINE, &mut cancel).await???;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let wire: Value = response.json().await?;
    assert!(wire.get("error").is_none(), "{wire}");
    assert_eq!(wire["id"], "cancel-race");
    assert_eq!(wire["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(wire["result"], serde_json::to_value(completed)?);
    h.runner.shutdown().await;
    Ok(())
}
