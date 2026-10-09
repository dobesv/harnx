//! Rules-off anonymous callers intentionally share one immutable principal.
use super::super::admission::remove_owner_lease;
use super::*;
use harnx_a2a_server::store::{message_fingerprint, DedupeKey};

fn anonymous_routes(h: &Harness, replica: &Replica) -> Result<Router> {
    let mut export = h.export.clone();
    export.lookup_keys = vec![export.public_name.clone()];
    routes::router(&[export], None, &[], |export, identity| {
        Arc::new(HarnxHandler::new(
            export.clone(),
            identity,
            replica.backend.clone(),
            InputLimits::default(),
        ))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_anonymous_recovery_keeps_shared_principal_without_prompt_replay() -> Result<()> {
    let mut t = TwoBackends::start().await?;
    t.a.app = anonymous_routes(&t.h, &t.a)?;
    t.b.app = anonymous_routes(&t.h, &t.b)?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "ignored-alice", "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    let identity = DedupeKey {
        cluster: "runner".into(),
        export: "runner".into(),
        owner: None,
        message_id: "same-first-message".into(),
    };
    let message: a2a_lf::Message = serde_json::from_value(first_message()["message"].clone())?;
    let saved =
        t.b.backend
            .store
            .first_message_reservation(&identity, &message_fingerprint(&message.parts))
            .await?
            .unwrap();
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    assert_eq!(
        settled(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        a2a_lf::TaskState::Failed
    );
    drop(pause);
    let first = send.await??;
    let retry =
        t.b.rpc("ignored-bob", "SendMessage", first_message())
            .await?;
    assert_eq!(result_task(&first)?["id"], result_task(&retry)?["id"]);
    assert!(t
        .b
        .backend
        .store
        .get_binding(&saved.allocation.storage_key)
        .await?
        .unwrap()
        .owner
        .is_none());
    let metadata =
        t.h.metadata
            .get(&saved.allocation.storage_key)
            .await?
            .unwrap()
            .metadata;
    assert!(
        harnx_runtime::nats_session_metadata::session_properties(&metadata)?
            .text("user_id")
            .is_none()
    );
    let mut cursor = 0;
    assert!(t
        .b
        .backend
        .store
        .next_recovery_registration(&mut cursor)
        .await?
        .unwrap()
        .owner
        .is_none());
    assert!(t.h.llm.requests.lock().is_empty());
    assert!(prompts(&NatsSessionLog::new(
        t.h.jetstream.clone(),
        &saved.allocation.storage_key
    ))
    .await?
    .is_empty());
    t.b.backend.abort.set_ctrlc();
    t.a.backend.runner.shutdown().await;
    Ok(())
}
