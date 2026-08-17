//! Registry bookkeeping: handing out live handles, and reaping only when it is safe.

use super::*;

fn noop_call_fn() -> AgentCallFn {
    Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Registry with a 50ms reap TTL and an agent to resolve, ready to spawn actors.
async fn registry_sandbox() -> (
    harnx_runtime::client::TestStateGuard<'static>,
    TestConfigSandbox,
    SessionRegistry,
) {
    let guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let registry = registry_with_call_fn(noop_call_fn());
    (guard, sandbox, registry)
}

/// Arm the reap deadline the way a departing subscriber does, and wait until the actor has
/// processed it: `Unsubscribe` carries no reply, so a following `Get` round-trip is the
/// confirmation that the deadline is set.
async fn arm_reap_deadline(handle: &SessionHandle) {
    handle
        .tx
        .send(SessionCommand::Unsubscribe)
        .await
        .expect("send unsubscribe");
    get_info(handle).await;
}

async fn wait_until_unregistered(registry: &SessionRegistry, key: &SessionKey) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.has_session(key) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle session actor should be reaped once no caller holds its handle");
}

fn registered_actor_id(registry: &SessionRegistry, key: &SessionKey) -> Option<u64> {
    registry.map.get(key).map(|entry| entry.actor_id)
}

#[tokio::test]
async fn reap_spares_an_actor_whose_handle_a_caller_still_holds() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "reap-with-held-handle");
    let handle = registry.get_or_spawn(key.clone());
    let actor_id = handle.actor_id;

    arm_reap_deadline(&handle).await;
    // Well past the 50ms TTL: the reap must keep deferring while this handle is alive.
    sleep(Duration::from_millis(300)).await;

    let info = get_info(&handle).await;
    assert_eq!(info.state, SessionState::Idle);
    assert_eq!(
        registered_actor_id(&registry, &key),
        Some(actor_id),
        "the same actor should still be registered"
    );
}

#[tokio::test]
async fn reap_removes_an_idle_actor_once_its_handle_is_dropped() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "reap-after-handle-dropped");
    let handle = registry.get_or_spawn(key.clone());

    arm_reap_deadline(&handle).await;
    drop(handle);

    wait_until_unregistered(&registry, &key).await;
}

#[tokio::test]
async fn get_or_spawn_replaces_an_entry_whose_actor_has_stopped() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "stopped-actor-entry");

    // An actor that died without deregistering, e.g. a panicking task.
    let (dead_tx, dead_rx) = mpsc::channel(1);
    drop(dead_rx);
    registry.map.insert(
        key.clone(),
        SessionHandle {
            tx: dead_tx,
            actor_id: u64::MAX,
        },
    );

    let handle = registry.get_or_spawn(key.clone());
    assert_ne!(handle.actor_id, u64::MAX, "expected a fresh actor");
    assert_eq!(get_info(&handle).await.state, SessionState::Idle);
    assert_eq!(registered_actor_id(&registry, &key), Some(handle.actor_id));
}

#[tokio::test]
async fn a_stopping_actor_leaves_its_replacement_registered() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "replacement-survives");

    let outgoing = registry.get_or_spawn(key.clone());
    // Unregister the first actor so the key is free, then register a second one for it. The
    // first actor now only stops once `outgoing` goes away.
    registry.map.remove(&key);
    let replacement = registry.get_or_spawn(key.clone());
    assert_ne!(outgoing.actor_id, replacement.actor_id);

    drop(outgoing);
    sleep(Duration::from_millis(200)).await;

    assert_eq!(
        registered_actor_id(&registry, &key),
        Some(replacement.actor_id),
        "the outgoing actor must not remove its replacement's entry"
    );
    assert_eq!(get_info(&replacement).await.state, SessionState::Idle);
}
