//! Current ACL memberships and immutable owners across independent replicas.
use super::*;
use harnx_a2a_server::identity::{Principal, RequestIdentity};
mod anonymous;
mod recovery;
mod support;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_membership_change_during_first_retry_preserves_principal_dedup() -> Result<()> {
    let t = start().await?;
    let mut reserved =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { call(app, &member("alice"), "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, reserved.reached()).await?;
    let saved = super::admission::reservation(&t).await?;
    let mut waiting =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::InitializationWait, 1);
    let retry = tokio::spawn({
        let app = t.b.app.clone();
        async move { call(app, &reviewer("alice"), "SendMessage", first_message()).await }
    });
    tokio::time::timeout(DEADLINE, waiting.reached()).await?;
    drop(reserved);
    let first = send.await??;
    drop(waiting);
    let second = retry.await??;
    assert_eq!(result_task(&first)?["id"], result_task(&second)?["id"]);
    let id = &saved.allocation.task_id;
    let mut changed = first_message();
    changed["message"]["parts"][0]["text"] = json!("changed payload");
    hidden_code(
        &call(t.b.app.clone(), &reviewer("alice"), "SendMessage", changed).await?,
        -32602,
    );
    finish(&t, id, &reviewer("alice")).await?;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(
        prompts(&NatsSessionLog::new(
            t.h.jetstream.clone(),
            &saved.allocation.storage_key
        ))
        .await?
        .len(),
        1
    );
    assert_stored_owner(&t, &saved.allocation.local_id, "alice").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_shared_memberships_and_admin_do_not_merge_first_principal_identities() -> Result<()> {
    let t = start().await?;
    let first = call(
        t.a.app.clone(),
        &member("alice"),
        "SendMessage",
        first_message(),
    )
    .await?;
    let alice_task = result_task(&first)?.clone();
    finish(&t, alice_task["id"].as_str().unwrap(), &member("alice")).await?;
    let second = call(
        t.b.app.clone(),
        &admin("bob"),
        "SendMessage",
        first_message(),
    )
    .await?;
    let bob_task = result_task(&second)?.clone();
    assert_ne!(alice_task["contextId"], bob_task["contextId"]);
    finish(&t, bob_task["id"].as_str().unwrap(), &member("bob")).await?;
    let retry = call(
        t.a.app.clone(),
        &reviewer("bob"),
        "SendMessage",
        first_message(),
    )
    .await?;
    assert_eq!(result_task(&retry)?["id"], bob_task["id"]);
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    assert_stored_owner(&t, alice_task["contextId"].as_str().unwrap(), "alice").await?;
    assert_stored_owner(&t, bob_task["contextId"].as_str().unwrap(), "bob").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_revocation_and_namespace_collision_hide_all_remote_routes_before_dedup() -> Result<()>
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
    let id = task["id"].as_str().unwrap();
    let context = task["contextId"].as_str().unwrap();
    for caller in [
        revoked("alice"),
        member("bob"),
        identity("bob", &["alice"], &["alice"]),
    ] {
        for (method, params) in [
            ("GetTask", json!({"id":id})),
            ("CancelTask", json!({"id":id})),
            ("SubscribeToTask", json!({"id":id})),
            ("ListTasks", json!({"contextId":context})),
            (
                "SendMessage",
                followup(context, "same-first-message", "changed payload"),
            ),
        ] {
            hidden_code(
                &call(t.b.app.clone(), &caller, method, params).await?,
                -32001,
            );
        }
        assert!(t
            .b
            .backend
            .store
            .get_task_for_export(&t.h.export, &caller, id)
            .await?
            .is_none());
        assert!(t
            .b
            .backend
            .runner
            .subscribe(&t.h.export, &caller, id)
            .await
            .is_err());
        assert!(handler(&t, &t.b).wait_terminal(&caller, id).await.is_err());
    }
    let mut mismatched = first_message();
    mismatched["message"]["parts"][0]["text"] = json!("changed payload");
    hidden_code(
        &call(
            t.b.app.clone(),
            &revoked("alice"),
            "SendMessage",
            mismatched,
        )
        .await?,
        -32001,
    );
    assert!(t
        .a
        .backend
        .store
        .read_context(&harnx_core::session_identity::session_key(
            Some("runner"),
            context
        ))
        .await?
        .unwrap()
        .document
        .state
        .active
        .unwrap()
        .cancel
        .is_none());
    finish(&t, id, &member("alice")).await?;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_stored_owner(&t, context, "alice").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_admin_reads_streams_and_cancels_without_transferring_owner() -> Result<()> {
    let t = start().await?;
    let first = call(
        t.a.app.clone(),
        &member("alice"),
        "SendMessage",
        first_message(),
    )
    .await?;
    let task = result_task(&first)?.clone();
    let id = task["id"].as_str().unwrap();
    let caller = admin("bob");
    let read = call(t.b.app.clone(), &caller, "GetTask", json!({"id":id})).await?;
    assert_eq!(read["result"]["id"], id, "{read}");
    let list = call(
        t.b.app.clone(),
        &caller,
        "ListTasks",
        json!({"contextId":task["contextId"]}),
    )
    .await?;
    assert_eq!(list["result"]["tasks"][0]["id"], id);
    let mut sub =
        t.b.backend
            .runner
            .subscribe(&t.h.export, &caller, id)
            .await?;
    let canceled = call(t.b.app.clone(), &caller, "CancelTask", json!({"id":id})).await?;
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    tokio::time::timeout(DEADLINE, async {
        loop {
            if sub.events.recv().await?.is_terminal() {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    hidden_code(
        &call(t.b.app.clone(), &member("bob"), "GetTask", json!({"id":id})).await?,
        -32001,
    );
    assert_stored_owner(&t, task["contextId"].as_str().unwrap(), "alice").await?;
    Ok(())
}
