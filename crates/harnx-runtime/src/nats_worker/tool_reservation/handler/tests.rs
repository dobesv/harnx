use super::*;
use crate::config::{GlobalConfig, ToolServerConfig};
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
use crate::nats_worker::server_reconciler::ServerLauncher;
use async_trait::async_trait;
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(3);

struct Gate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct GatedLauncher {
    gate_start: bool,
    gate: StdMutex<Option<Gate>>,
    started: StdMutex<Vec<String>>,
    stopped: StdMutex<Vec<String>>,
}

impl GatedLauncher {
    async fn hold(&self) {
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            let _ = gate.release.await;
        }
    }
}

#[async_trait]
impl ServerLauncher for GatedLauncher {
    async fn start(&self, server: &ToolServerConfig) -> Result<()> {
        self.started.lock().unwrap().push(server.name.clone());
        if self.gate_start && server.name == "slow" {
            self.hold().await;
        }
        Ok(())
    }

    async fn stop(&self, name: &str) {
        self.stopped.lock().unwrap().push(name.to_owned());
        if !self.gate_start && name == "slow" {
            self.hold().await;
        }
    }
}

struct Fixture {
    handler: Arc<Handler>,
    launcher: Arc<GatedLauncher>,
    worker: AbortOnDropHandle<()>,
    shutdown: CancellationToken,
    subject: String,
    session_key: String,
    entered: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
    _server: crate::nats_test_common::NatsServerHandle,
}

async fn seed_test_session_metadata(
    js: &async_nats::jetstream::Context,
) -> Result<(SessionMetadataStore, String)> {
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let session = SessionMetadata::new(
        "responsive-reservation",
        SessionInitializer::named("worker", Default::default()),
    );
    let session_key = session.storage_key();
    metadata.create(&session).await?;
    Ok((metadata, session_key))
}

fn build_test_tool_config() -> GlobalConfig {
    let config = GlobalConfig::default();
    config.write().tool_servers = ["slow", "healthy"]
        .into_iter()
        .map(|name| ToolServerConfig {
            name: name.to_owned(),
            command: "unused-test-launcher".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            description: None,
            package: None,
            hooks: None,
        })
        .collect();
    config
}

fn build_test_launcher(
    gate_start: bool,
) -> (
    Arc<GatedLauncher>,
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
) {
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let launcher = Arc::new(GatedLauncher {
        gate_start,
        gate: StdMutex::new(Some(Gate {
            entered: entered_tx,
            release: release_rx,
        })),
        started: StdMutex::new(Vec::new()),
        stopped: StdMutex::new(Vec::new()),
    });
    (launcher, entered, release)
}

impl Fixture {
    async fn start(gate_start: bool) -> Result<Option<Self>> {
        let Some(server) = crate::nats_test_common::spawn_nats_server().await? else {
            return Ok(None);
        };
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let (metadata, session_key) = seed_test_session_metadata(&js).await?;
        let config = build_test_tool_config();
        let (launcher, entered, release) = build_test_launcher(gate_start);

        let subject = client.new_inbox();
        let reserves = client.subscribe(subject.clone()).await?;
        let control_prefix = client.new_inbox();
        let controls = client.subscribe(format!("{control_prefix}.*")).await?;
        client.flush().await?;
        let handler = Arc::new(Handler {
            config,
            metadata,
            reconciler: Some(Arc::new(ServerReconciler::new(
                launcher.clone(),
                Duration::ZERO,
            ))),
            client,
            worker_id: "test-worker".to_owned(),
            server_scope: "test-scope".to_owned(),
            control_prefix,
            ttl: Duration::from_secs(60),
            reservations: Mutex::new(HashMap::new()),
        });
        let shutdown = CancellationToken::new();
        let worker = AbortOnDropHandle::new(tokio::spawn(handler.clone().run(
            reserves,
            controls,
            shutdown.clone(),
        )));
        Ok(Some(Self {
            handler,
            launcher,
            worker,
            shutdown,
            subject,
            session_key,
            entered,
            release: Some(release),
            _server: server,
        }))
    }

    fn request(&self, selector: &str) -> Reserve {
        Reserve::new(
            &self.session_key,
            ToolReservationView {
                package: None,
                use_tools: vec![selector.to_owned()],
            },
        )
    }

    async fn reserve(&self, selector: &str) -> Reserved {
        match request_reserve(&self.handler.client, &self.subject, self.request(selector)).await {
            ReserveReply::Reserved(reserved) => reserved,
            ReserveReply::Error(error) => panic!("reserve failed: {error:?}"),
        }
    }

    async fn control(&self, reserved: &Reserved, renew: bool) -> ToolReservationControlReply {
        let control = if renew {
            ToolReservationControl::Renew(Renew {
                reservation_id: reserved.reservation_id.clone(),
            })
        } else {
            ToolReservationControl::Release(Release {
                reservation_id: reserved.reservation_id.clone(),
            })
        };
        let reply = tokio::time::timeout(
            PROMPT_TIMEOUT,
            self.handler.client.request(
                reserved.control_subject.clone(),
                serde_json::to_vec(&control).unwrap().into(),
            ),
        )
        .await
        .expect("control must not wait for teardown")
        .unwrap();
        serde_json::from_slice(&reply.payload).unwrap()
    }

    async fn expire(&self, reserved: &Reserved) {
        *self
            .handler
            .reservations
            .lock()
            .await
            .get_mut(&reserved.reservation_id)
            .unwrap() = Instant::now() - Duration::from_secs(1);
    }
}

async fn request_reserve(
    client: &async_nats::Client,
    subject: &str,
    request: Reserve,
) -> ReserveReply {
    let reply = tokio::time::timeout(
        PROMPT_TIMEOUT,
        client.request(
            subject.to_owned(),
            serde_json::to_vec(&request).unwrap().into(),
        ),
    )
    .await
    .expect("unrelated reserve must not wait for teardown")
    .unwrap();
    serde_json::from_slice(&reply.payload).unwrap()
}

fn assert_ok(reply: ToolReservationControlReply) {
    assert_eq!(
        reply,
        ToolReservationControlReply::Ok(ToolReservationOk::Ok)
    );
}

fn assert_expired(reply: ToolReservationControlReply) {
    assert_eq!(reply, unknown_or_expired());
}

async fn wait_for_count(handler: &Handler, count: usize) {
    tokio::time::timeout(PROMPT_TIMEOUT, async {
        while handler.reservations.lock().await.len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reservation state must reach expected count");
}

#[derive(Clone, Copy)]
enum EndReservation {
    Release,
    Sweep,
    ExpiredRenew,
}

async fn blocked_stop_remains_responsive(end: EndReservation) -> Result<()> {
    let Some(mut f) = Fixture::start(false).await? else {
        return Ok(());
    };
    let slow = f.reserve("slow_*").await;
    let healthy = f.reserve("healthy_*").await;
    match end {
        EndReservation::Release => assert_ok(f.control(&slow, false).await),
        EndReservation::Sweep => f.expire(&slow).await,
        EndReservation::ExpiredRenew => {
            f.expire(&slow).await;
            assert_expired(f.control(&slow, true).await);
        }
    }
    // Gate stays closed throughout the requests below. No sleep estimates the
    // stop duration: it cannot complete until this test explicitly releases it.
    tokio::time::timeout(PROMPT_TIMEOUT, &mut f.entered).await??;
    assert_ok(f.control(&healthy, true).await);
    let another = f.reserve("healthy_*").await;
    assert_expired(f.control(&slow, true).await);
    assert_ok(f.control(&slow, false).await);
    let mut wrong_subject = healthy.clone();
    wrong_subject.control_subject = slow.control_subject.clone();
    assert_ok(f.control(&wrong_subject, false).await);
    assert_expired(f.control(&wrong_subject, true).await);
    assert_ok(f.control(&healthy, true).await);

    // Same-name reserve really reaches setup while the old slot is Stopping.
    // It must wait, not join a process that cleanup is about to remove.
    let racing = AbortOnDropHandle::new(tokio::spawn({
        let client = f.handler.client.clone();
        let subject = f.subject.clone();
        let request = f.request("slow_*");
        async move { request_reserve(&client, &subject, request).await }
    }));
    wait_for_count(&f.handler, 3).await;
    assert!(!racing.is_finished());
    f.release.take().unwrap().send(()).unwrap();
    let ReserveReply::Reserved(fresh) = racing.await? else {
        panic!("same-name reserve failed");
    };
    assert_ok(f.control(&slow, false).await);
    assert_expired(f.control(&slow, true).await);
    assert_ok(f.control(&fresh, true).await);
    assert_ok(f.control(&healthy, false).await);
    assert_ok(f.control(&another, true).await);
    assert_eq!(
        f.handler.reconciler.as_ref().unwrap().running().await,
        ["healthy", "slow"]
    );
    assert_eq!(
        f.launcher.started.lock().unwrap().as_slice(),
        ["slow", "healthy", "slow"]
    );
    assert_eq!(f.launcher.stopped.lock().unwrap().as_slice(), ["slow"]);
    f.shutdown.cancel();
    tokio::time::timeout(PROMPT_TIMEOUT, f.worker).await??;
    assert!(f.handler.reservations.lock().await.is_empty());
    assert!(f
        .handler
        .reconciler
        .as_ref()
        .unwrap()
        .running()
        .await
        .is_empty());
    let mut stopped = f.launcher.stopped.lock().unwrap().clone();
    stopped.sort();
    assert_eq!(stopped, ["healthy", "slow", "slow"]);
    Ok(())
}

#[tokio::test]
async fn release_blocked_stop_keeps_control_and_reserve_responsive() -> Result<()> {
    blocked_stop_remains_responsive(EndReservation::Release).await
}

#[tokio::test]
async fn expiry_blocked_stop_keeps_control_and_reserve_responsive() -> Result<()> {
    blocked_stop_remains_responsive(EndReservation::Sweep).await
}

#[tokio::test]
async fn expired_renew_blocked_stop_keeps_control_and_reserve_responsive() -> Result<()> {
    blocked_stop_remains_responsive(EndReservation::ExpiredRenew).await
}

#[tokio::test]
async fn blocked_cleanup_does_not_consume_reserve_capacity_or_resurrect_canceled_claims(
) -> Result<()> {
    let Some(mut f) = Fixture::start(false).await? else {
        return Ok(());
    };
    let slow = f.reserve("slow_*").await;
    assert_ok(f.control(&slow, false).await);
    tokio::time::timeout(PROMPT_TIMEOUT, &mut f.entered).await??;
    // Every request below waits for the same stopping slot. Publish directly
    // so no client timeout cancels the test's pending requests.
    for _ in 0..MAX_PENDING_RESERVES - 1 {
        f.handler
            .client
            .publish_with_reply(
                f.subject.clone(),
                f.handler.client.new_inbox(),
                serde_json::to_vec(&f.request("slow_*")).unwrap().into(),
            )
            .await?;
    }
    wait_for_count(&f.handler, MAX_PENDING_RESERVES - 1).await;
    let healthy = f.reserve("healthy_*").await;
    assert_ok(f.control(&healthy, true).await);
    f.handler
        .client
        .publish_with_reply(
            f.subject.clone(),
            f.handler.client.new_inbox(),
            serde_json::to_vec(&f.request("slow_*")).unwrap().into(),
        )
        .await?;
    wait_for_count(&f.handler, MAX_PENDING_RESERVES + 1).await;
    let ReserveReply::Error(error) =
        request_reserve(&f.handler.client, &f.subject, f.request("healthy_*")).await
    else {
        panic!("pending limit must still apply");
    };
    assert_eq!(
        error.code,
        ToolReservationErrorCode::Other("Busy".to_owned())
    );

    f.shutdown.cancel();
    // Shutdown cancels/drains all claim futures before removing their IDs.
    // Teardown is still blocked and must be drained, not canceled.
    wait_for_count(&f.handler, 0).await;
    assert!(!f.worker.is_finished());
    f.release.take().unwrap().send(()).unwrap();
    tokio::time::timeout(PROMPT_TIMEOUT, f.worker).await??;
    assert!(f
        .handler
        .reconciler
        .as_ref()
        .unwrap()
        .running()
        .await
        .is_empty());
    assert_eq!(
        f.launcher.started.lock().unwrap().as_slice(),
        ["slow", "healthy"]
    );
    Ok(())
}

#[tokio::test]
async fn released_during_startup_is_not_reinserted_when_start_finishes() -> Result<()> {
    let Some(mut f) = Fixture::start(true).await? else {
        return Ok(());
    };
    let pending = AbortOnDropHandle::new(tokio::spawn({
        let client = f.handler.client.clone();
        let subject = f.subject.clone();
        let request = f.request("slow_*");
        async move { request_reserve(&client, &subject, request).await }
    }));
    tokio::time::timeout(PROMPT_TIMEOUT, &mut f.entered).await??;
    let id = f
        .handler
        .reservations
        .lock()
        .await
        .keys()
        .next()
        .unwrap()
        .clone();
    let reserved = Reserved {
        protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
        attempt_id: String::new(),
        reservation_id: id.clone(),
        worker_id: f.handler.worker_id.clone(),
        server_scope: f.handler.server_scope.clone(),
        control_subject: format!("{}.{id}", f.handler.control_prefix),
        ttl_ms: 0,
        renew_after_ms: 0,
    };
    assert_ok(f.control(&reserved, false).await);
    f.release.take().unwrap().send(()).unwrap();
    let ReserveReply::Error(error) = pending.await? else {
        panic!("released setup must not succeed");
    };
    assert_eq!(error.code, ToolReservationErrorCode::UnknownOrExpired);
    assert_expired(f.control(&reserved, true).await);
    f.shutdown.cancel();
    tokio::time::timeout(PROMPT_TIMEOUT, f.worker).await??;
    assert!(f.handler.reservations.lock().await.is_empty());
    assert!(f
        .handler
        .reconciler
        .as_ref()
        .unwrap()
        .running()
        .await
        .is_empty());
    assert_eq!(f.launcher.stopped.lock().unwrap().as_slice(), ["slow"]);
    Ok(())
}
