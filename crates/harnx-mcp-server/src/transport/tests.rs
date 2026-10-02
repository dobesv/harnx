use super::*;
use futures_util::FutureExt;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(30);
const REPEATS: usize = 12;

struct TestServer {
    reject: bool,
}

impl ServerHandler for TestServer {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, ErrorData> {
        if self.reject {
            Err(ErrorData::invalid_request("test rejects initialize", None))
        } else {
            self.negotiate_initialize(&request)
        }
    }
}

fn initialize_message() -> ClientJsonRpcMessage {
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "registry-test", "version": "1" }
        }
    }))
    .unwrap()
}

fn initialized_message() -> ClientJsonRpcMessage {
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    }))
    .unwrap()
}

async fn assert_baseline(manager: &OwnedSessionManager, baseline: usize, retired: &SessionId) {
    // Lifecycle pruning is synchronous; rmcp handle removal is tracked async work.
    assert_eq!(manager.lifecycles.lock().len(), baseline);
    assert!(!manager.lifecycles.lock().contains_key(retired));
    tokio::time::timeout(DEADLINE, async {
        while manager.inner.sessions.read().await.len() != baseline {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminated rmcp sessions must be removed");
    assert!(!manager.has_session(retired).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_idle_expiry_prunes_both_registries_without_delete() {
    let mut config = SessionConfig::default();
    config.init_timeout = None;
    // Leave handshake scheduling margin under concurrent crate/stress runs.
    config.keep_alive = Some(Duration::from_millis(250));
    let manager = OwnedSessionManager::new(config);
    // Keep one unrelated, uninitialized session alive as the registry baseline.
    let (sentinel_id, sentinel) = manager.create_session().await.unwrap();
    for _ in 0..REPEATS {
        let (id, transport) = manager.create_session().await.unwrap();
        let server = tokio::spawn(TestServer { reject: false }.serve(transport));
        let response = tokio::time::timeout(
            DEADLINE,
            manager.initialize_session(&id, initialize_message()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(response, ServerJsonRpcMessage::Response(_)));
        manager
            .accept_message(&id, initialized_message())
            .await
            .unwrap();
        let service = server.await.unwrap().unwrap();
        tokio::time::timeout(DEADLINE, service.waiting())
            .await
            .unwrap()
            .unwrap();
        assert_baseline(&manager, 1, &id).await;
        assert!(manager.has_session(&sentinel_id).await.unwrap());
    }
    drop(sentinel);
    manager.close_all().await;
    assert_baseline(&manager, 0, &sentinel_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_uninitialized_expiry_prunes_both_registries() {
    let mut config = SessionConfig::default();
    config.init_timeout = Some(Duration::from_millis(30));
    let manager = OwnedSessionManager::new(config);
    for _ in 0..REPEATS {
        let (id, mut transport) = manager.create_session().await.unwrap();
        assert!(tokio::time::timeout(DEADLINE, transport.receive())
            .await
            .unwrap()
            .is_none());
        // Retaining the transport must not retain either registry entry.
        assert_baseline(&manager, 0, &id).await;
        drop(transport);
    }
    manager.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_failed_initialize_prunes_both_registries() {
    let manager = OwnedSessionManager::new(SessionConfig::default());
    for _ in 0..REPEATS {
        let (id, transport) = manager.create_session().await.unwrap();
        let server = tokio::spawn(TestServer { reject: true }.serve(transport));
        let response = tokio::time::timeout(
            DEADLINE,
            manager.initialize_session(&id, initialize_message()),
        )
        .await
        .unwrap();
        // rmcp can deliver the error response or terminate before delivery.
        if let Ok(response) = response {
            assert!(matches!(response, ServerJsonRpcMessage::Error(_)));
        }
        assert!(tokio::time::timeout(DEADLINE, server)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_baseline(&manager, 0, &id).await;
    }
    manager.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_handshake_future_drop_prunes_both_registries() {
    let manager = OwnedSessionManager::new(SessionConfig::default());
    for _ in 0..REPEATS {
        let (id, transport) = manager.create_session().await.unwrap();
        // Poll through transport ownership, then cancel the handshake future.
        assert!(TestServer { reject: false }
            .serve(transport)
            .now_or_never()
            .is_none());
        assert_baseline(&manager, 0, &id).await;
        manager.close_session(&id).await.unwrap();
    }
    manager.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_shutdown_and_transport_drop_are_idempotent() {
    for _ in 0..REPEATS {
        let manager = Arc::new(OwnedSessionManager::new(SessionConfig::default()));
        let (id, transport) = manager.create_session().await.unwrap();
        let lifecycle = Arc::downgrade(&transport.lifecycle);
        let registry = Arc::downgrade(&manager.lifecycles);
        let inner = Arc::downgrade(&manager.inner);
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let dropper = tokio::spawn({
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                drop(transport);
            }
        });
        let deleter = tokio::spawn({
            let manager = manager.clone();
            let barrier = barrier.clone();
            let id = id.clone();
            async move {
                barrier.wait().await;
                manager.close_session(&id).await.unwrap();
                manager.close_session(&id).await.unwrap();
            }
        });
        let closer = tokio::spawn({
            let manager = manager.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                manager.close_all().await;
                manager.close_all().await;
            }
        });
        barrier.wait().await;
        tokio::time::timeout(DEADLINE, async {
            dropper.await.unwrap();
            deleter.await.unwrap();
            closer.await.unwrap();
        })
        .await
        .expect("concurrent teardown must not deadlock");
        assert_baseline(&manager, 0, &id).await;
        assert!(manager.cleanup.is_empty());
        assert!(manager.create_session().await.is_err());
        assert!(lifecycle.upgrade().is_none());
        drop(manager);
        assert!(registry.upgrade().is_none(), "registry ownership cycle");
        assert!(inner.upgrade().is_none(), "rmcp manager ownership cycle");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_waits_for_admitted_creation_then_rejects_new_sessions() {
    let manager = Arc::new(OwnedSessionManager::new(SessionConfig::default()));
    let sessions = manager.inner.sessions.write().await;
    let creator = tokio::spawn({
        let manager = manager.clone();
        async move { manager.create_session().await.unwrap() }
    });
    tokio::time::timeout(DEADLINE, async {
        while manager.admission.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("create_session must acquire admission before entering rmcp");
    let closer = tokio::spawn({
        let manager = manager.clone();
        async move { manager.close_all().await }
    });
    drop(sessions);
    let (id, transport) = tokio::time::timeout(DEADLINE, creator)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(DEADLINE, closer)
        .await
        .unwrap()
        .unwrap();
    assert_baseline(&manager, 0, &id).await;
    assert!(manager.cleanup.is_empty());
    assert!(manager.create_session().await.is_err());
    // Shutdown doesn't need the final transport owner to drop before pruning.
    assert!(transport.lifecycle.state.lock().stopped);
    drop(transport);
}
