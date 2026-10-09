use super::super::admission::{remove_owner_lease, reservation};
use super::*;
use harnx_core::{access_rules::AccessRules, crypto::sha256};

fn denied_replica(t: &TwoBackends) -> Result<Replica> {
    let rules = Arc::new(AccessRules::from_yaml("rules: []")?);
    let store = Arc::new(A2aStore::new_with_access_rules(
        t.h.metadata.clone(),
        Some(rules.clone()),
    ));
    replica(&t.h, store.clone(), Runner::new(store), rules)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_revoked_first_reservation_recovers_without_memberships_or_http() -> Result<()> {
    let t = start().await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { call(app, &member("alice"), "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    let saved = reservation(&t).await?;
    assert!(t
        .h
        .metadata
        .get(&saved.allocation.storage_key)
        .await?
        .is_none());
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let denied = denied_replica(&t)?;
    supervise(&t, &denied);
    let failed = settled(&t, &saved.allocation.task_id).await?;
    assert_eq!(failed.task.status.state, a2a_lf::TaskState::Failed);
    assert!(t.h.llm.requests.lock().is_empty());
    let log = NatsSessionLog::new(t.h.jetstream.clone(), &saved.allocation.storage_key);
    assert!(prompts(&log).await?.is_empty());
    assert_stored_owner(&t, &saved.allocation.local_id, "alice").await?;
    hidden_code(
        &call(
            denied.app.clone(),
            &member("alice"),
            "SendMessage",
            first_message(),
        )
        .await?,
        -32001,
    );
    drop(pause);
    let retry = send.await??;
    assert_eq!(result_task(&retry)?["id"], saved.allocation.task_id);
    assert_eq!(result_task(&retry)?["status"]["state"], "TASK_STATE_FAILED");
    assert!(prompts(&log).await?.is_empty());
    denied.backend.abort.set_ctrlc();
    t.a.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_revoked_active_admission_is_closed_and_stale_owner_cannot_execute() -> Result<()> {
    let t = start().await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::BeforeAdmission, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { call(app, &reviewer("alice"), "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    let saved = reservation(&t).await?;
    let before =
        t.a.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .unwrap();
    hidden_code(
        &call(
            t.b.app.clone(),
            &revoked("alice"),
            "SendMessage",
            first_message(),
        )
        .await?,
        -32001,
    );
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let denied = denied_replica(&t)?;
    supervise(&t, &denied);
    let failed = settled(&t, &saved.allocation.task_id).await?;
    assert_eq!(failed.task.status.state, a2a_lf::TaskState::Failed);
    let after =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .unwrap();
    assert!(after.document.epoch > before.document.epoch);
    drop(pause);
    let _ = send.await??;
    t.a.backend.runner.shutdown().await;
    let retry = call(
        t.b.app.clone(),
        &member("alice"),
        "SendMessage",
        first_message(),
    )
    .await?;
    assert_eq!(result_task(&retry)?["id"], saved.allocation.task_id);
    assert_eq!(result_task(&retry)?["status"]["state"], "TASK_STATE_FAILED");
    assert!(t.h.llm.requests.lock().is_empty());
    assert!(prompts(&NatsSessionLog::new(
        t.h.jetstream.clone(),
        &saved.allocation.storage_key
    ))
    .await?
    .is_empty());
    assert_stored_owner(&t, &saved.allocation.local_id, "alice").await?;
    denied.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_admin_followup_recovery_keeps_original_owner_even_without_old_registry() -> Result<()>
{
    let t = start().await?;
    let first = call(
        t.a.app.clone(),
        &member("alice"),
        "SendMessage",
        first_message(),
    )
    .await?;
    let task = result_task(&first)?.clone();
    finish(&t, task["id"].as_str().unwrap(), &member("alice")).await?;
    let context = task["contextId"].as_str().unwrap();
    let storage = harnx_core::session_identity::session_key(Some("runner"), context);
    // Rebuild discovery from an admin follow-up. This used to stamp the current
    // caller, so restart would fail ownership validation or resume as the admin.
    let kv = t.h.metadata.kv_store();
    let key = format!("a2a.registry.{}", sha256(&storage));
    kv.stream
        .purge()
        .filter(format!("$KV.{}.{}", kv.name, key))
        .await?;
    assert!(harnx_nats_common::leader_reads::get(kv, &key)
        .await?
        .is_none());
    let mut pause =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::BeforeAdmission, 1);
    let request = followup(context, "admin-followup", "Follow up");
    let send = tokio::spawn({
        let app = t.b.app.clone();
        let request = request.clone();
        async move {
            call(
                app,
                &identity("bob", &[], &["acl-supervisor"]),
                "SendMessage",
                request,
            )
            .await
        }
    });
    tokio::time::timeout(DEADLINE, pause.reached()).await?;
    let authority = t.b.backend.store.read_context(&storage).await?.unwrap();
    let id = authority
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .task
        .id
        .clone();
    assert_stored_owner(&t, context, "alice").await?;
    remove_owner_lease(&t, &storage).await?;
    let denied = denied_replica(&t)?;
    supervise(&t, &denied);
    assert_eq!(
        settled(&t, &id).await?.task.status.state,
        a2a_lf::TaskState::Failed
    );
    drop(pause);
    let _ = send.await??;
    let retry = call(
        t.a.app.clone(),
        &identity("bob", &[], &["acl-supervisor"]),
        "SendMessage",
        request.clone(),
    )
    .await?;
    assert_eq!(result_task(&retry)?["id"], id);
    hidden_code(
        &call(t.a.app.clone(), &member("bob"), "SendMessage", request).await?,
        -32001,
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(
        prompts(&NatsSessionLog::new(t.h.jetstream.clone(), &storage))
            .await?
            .len(),
        1
    );
    assert_stored_owner(&t, context, "alice").await?;
    denied.backend.abort.set_ctrlc();
    t.b.backend.runner.shutdown().await;
    Ok(())
}
