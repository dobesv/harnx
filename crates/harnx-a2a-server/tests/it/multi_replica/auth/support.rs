use super::*;
use harnx_a2a_server::identity::Identity;
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session_metadata::session_properties;

pub(super) fn rules() -> Arc<AccessRules> {
    Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [runner@runner]\n    groups: [acl-team, alice]\n  - agents: [runner@runner]\n    roles: [acl-reviewer, alice]\n  - agents: [runner@runner]\n    roles: [acl-supervisor]\n    scopes: [admin]\n",
    ).unwrap())
}
pub(super) fn identity(user: &str, groups: &[&str], roles: &[&str]) -> RequestIdentity {
    RequestIdentity {
        principal: Principal::User(user.into()),
        groups: groups.iter().map(|s| (*s).into()).collect(),
        roles: roles.iter().map(|s| (*s).into()).collect(),
    }
}
pub(super) fn member(user: &str) -> RequestIdentity {
    identity(user, &["acl-team"], &[])
}
pub(super) fn reviewer(user: &str) -> RequestIdentity {
    identity(user, &[], &["acl-reviewer"])
}
pub(super) fn admin(user: &str) -> RequestIdentity {
    identity(user, &["acl-team"], &["acl-supervisor"])
}
pub(super) fn revoked(user: &str) -> RequestIdentity {
    identity(user, &[], &[])
}
fn policy() -> Result<Identity> {
    Identity::with_memberships(
        &["X-User-ID".into()],
        &["X-Groups".into()],
        &["X-Roles".into()],
    )
}
pub(super) async fn start() -> Result<TwoBackends> {
    let rules = rules();
    let h = Harness::start_with_access_rules(Script::Text, Some(rules.clone())).await?;
    let a = replica(&h, h.store.clone(), h.runner.clone(), rules.clone())?;
    let store = Arc::new(A2aStore::new_with_access_rules(
        h.metadata.clone(),
        Some(rules.clone()),
    ));
    let b = replica(&h, store.clone(), Runner::new(store), rules)?;
    Ok(TwoBackends { a, b, h })
}
pub(super) fn replica(
    h: &Harness,
    store: Arc<A2aStore>,
    runner: Arc<Runner>,
    rules: Arc<AccessRules>,
) -> Result<Replica> {
    let mut replica = Replica::new(h, store, runner)?;
    let mut export = h.export.clone();
    export.lookup_keys = vec![export.public_name.clone()];
    replica.app = routes::router_with_access_rules(
        &[export],
        None,
        policy()?,
        Some(rules),
        |export, identity| {
            Arc::new(HarnxHandler::new(
                export.clone(),
                identity,
                replica.backend.clone(),
                InputLimits::default(),
            ))
        },
    )?;
    Ok(replica)
}
pub(super) fn handler(t: &TwoBackends, replica: &Replica) -> HarnxHandler {
    HarnxHandler::new(
        t.h.export.clone(),
        policy().unwrap(),
        replica.backend.clone(),
        InputLimits::default(),
    )
}
pub(super) async fn call(
    app: Router,
    caller: &RequestIdentity,
    method: &str,
    params: Value,
) -> Result<Value> {
    let request = Request::post("/agents/runner")
        .header("Content-Type", "application/json")
        .header("X-User-ID", caller.principal.user_id().unwrap())
        .header("X-Groups", caller.groups.join(","))
        .header("X-Roles", caller.roles.join(","))
        .body(Body::from(serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )?))?;
    let response = app.oneshot(request).await?;
    assert!(response.status().is_success());
    Ok(serde_json::from_slice(
        &response.into_body().collect().await?.to_bytes(),
    )?)
}
pub(super) fn hidden_code(response: &Value, code: i32) {
    assert_eq!(response["error"]["code"], code, "{response}");
    assert!(response.get("result").is_none(), "{response}");
}
pub(super) fn followup(context: &str, message: &str, text: &str) -> Value {
    json!({"message":{"contextId":context,"messageId":message,"role":"ROLE_USER","parts":[{"text":text}]},"configuration":{"returnImmediately":true}})
}
pub(super) async fn finish(t: &TwoBackends, task: &str, caller: &RequestIdentity) -> Result<()> {
    t.h.llm.release.notify_one();
    let terminal =
        tokio::time::timeout(DEADLINE, handler(t, &t.b).wait_terminal(caller, task)).await??;
    assert_eq!(terminal.task.status.state, a2a_lf::TaskState::Completed);
    Ok(())
}
pub(super) async fn settled(
    t: &TwoBackends,
    task: &str,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    tokio::time::timeout(DEADLINE, async {
        let context = harnx_a2a_server::store::parse_task_id(task)?.0;
        let storage = harnx_core::session_identity::session_key(Some("runner"), context);
        loop {
            if let Some(record) = t.b.backend.store.get_task(&storage, task).await? {
                if record.task.status.state.is_terminal() {
                    return Ok(record);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?
}
pub(super) fn supervise(t: &TwoBackends, replica: &Replica) {
    replica
        .backend
        .runner
        .start_supervision(harnx_a2a_server::runner::SupervisionConfig {
            exports: vec![t.h.export.clone()],
            config: t.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: replica.backend.abort.clone(),
        });
}
pub(super) async fn assert_stored_owner(t: &TwoBackends, context: &str, owner: &str) -> Result<()> {
    let storage = harnx_core::session_identity::session_key(Some("runner"), context);
    assert_eq!(
        t.b.backend
            .store
            .get_binding(&storage)
            .await?
            .unwrap()
            .owner
            .as_deref(),
        Some(owner)
    );
    let metadata = t.h.metadata.get(&storage).await?.unwrap().metadata;
    assert_eq!(session_properties(&metadata)?.text("user_id"), Some(owner));
    let mut cursor = 0;
    let mut registration = None;
    while let Some(entry) =
        t.b.backend
            .store
            .next_recovery_registration(&mut cursor)
            .await?
    {
        if entry.allocation.storage_key == storage {
            registration = Some(entry);
            break;
        }
    }
    let registration = registration.context("recovery registration")?;
    assert_eq!(registration.owner.as_deref(), Some(owner));
    let bytes = serde_json::to_string(&(
        metadata,
        registration,
        t.b.backend
            .store
            .read_context(&storage)
            .await?
            .map(|c| c.document),
    ))?;
    for value in [
        "acl-team",
        "acl-reviewer",
        "acl-supervisor",
        "X-Groups",
        "X-Roles",
    ] {
        assert!(
            !bytes.contains(value),
            "request membership persisted: {value}"
        );
    }
    Ok(())
}
