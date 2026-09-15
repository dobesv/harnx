use super::*;
use crate::nats_test_common as common;
use harnx_execution_control::{InterruptScope, Owner};

struct Fixture {
    _server: common::NatsServerHandle,
    js: async_nats::jetstream::Context,
    reconciler: Reconciler,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let store = ExecutionStore::ensure(&js, 1).await?;
        let journal = InvocationJournal::ensure(&js).await?;
        Ok(Self {
            _server: server,
            js: js.clone(),
            reconciler: Reconciler {
                store,
                journal,
                client: js.client().clone(),
            },
        })
    }

    async fn root(&self, name: &str) -> Result<ExecutionContext> {
        let store = &self.reconciler.store;
        let operation = store.session(name, None, Some("g1")).await?;
        store
            .claim(&operation.reference, Owner::invocation("worker"))
            .await?;
        store.activate_gate(&operation.reference).await
    }

    async fn acquire_cleanup_lease(
        &self,
        worker: &str,
    ) -> Result<Option<crate::nats_lease::NatsSessionLease>> {
        crate::nats_lease::NatsSessionLease::acquire(crate::nats_lease::NatsLeaseAcquireParams {
            jetstream: self.js.clone(),
            session_id: "lease-cleanup",
            worker_id: worker.into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        })
        .await
    }

    async fn stop(&self, context: &ExecutionContext) -> Result<CleanupScope> {
        let stop = self
            .reconciler
            .store
            .interrupt(
                &InterruptScope {
                    gate_root: context.gate_root().clone(),
                    operation: context.operation().clone(),
                    reason: "test interrupt".into(),
                },
                "stable-stop",
            )
            .await?;
        Ok(CleanupScope {
            context: context.clone(),
            stop,
            cleanup: CleanupStatus::default(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciler_recovers_missed_wake_and_late_owner_confirmation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let root = fixture.root("recover-cleanup").await?;
    let store = &fixture.reconciler.store;
    let tool = store
        .child(
            OperationRef::new("recover-cleanup", "tool"),
            root.operation().clone(),
        )
        .await?;
    let owner = Owner::invocation("tool-server");
    store.claim(&tool.reference, owner.clone()).await?;
    let tool = store.activate_gate(&tool.reference).await?;
    let scope = fixture.stop(&root).await?;
    // Fresh service state, no wake-up or owner process metadata to infer success from.
    let restarted = fixture.reconciler.clone();
    let recovered = restarted.store.cleanup_scopes().await?;
    assert_eq!(recovered.len(), 1);
    restarted
        .reconcile(&recovered[0], scope.stop.decision.accepted_at, false)
        .await?;
    assert_eq!(
        store.gate_cleanup(&root).await?.state,
        CleanupState::Pending
    );
    restarted
        .reconcile(
            &scope,
            scope.stop.decision.accepted_at + chrono::Duration::seconds(6),
            false,
        )
        .await?;
    assert_eq!(
        store.gate_cleanup(&root).await?.state,
        CleanupState::Unconfirmed
    );
    assert!(!store.gate_cleanup(&root).await?.owner_stopped);
    store.finish_cleanup_owner(&tool).await?;
    store
        .record_coverage(root.operation(), root.owner(), 0, true)
        .await?;
    store.finish_cleanup_owner(&root).await?;
    restarted.reconcile(&scope, Utc::now(), false).await?;
    assert_eq!(
        store.gate_cleanup(&root).await?.state,
        CleanupState::Confirmed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_scan_does_not_treat_missing_resource_metadata_as_confirmation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let store = &fixture.reconciler.store;
    let root = store
        .open_gate(
            OperationRef::new("missing-cleanup", "g1"),
            Owner::invocation("lost-worker"),
        )
        .await?;
    let scope = fixture.stop(&root).await?;
    let _service = CleanupSupervisor::start(&fixture.js, 1).await?;
    let cleanup = wait_cleanup(store, &root, CleanupState::Unconfirmed).await?;
    assert!(!cleanup.owner_stopped);
    assert!(cleanup.last_error.unwrap().contains("metadata"));
    assert_eq!(
        store.gate_stop(root.gate_root(), root.operation()).await?,
        Some(scope.stop)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_cleanup_releases_neither_g2_lease_nor_g2_execution_control() -> Result<()> {
    use super::super::{
        backend::NatsSessionLogBackend, execution_control::WorkerExecution, SessionActivate,
    };
    let fixture = Fixture::new().await?;
    let store = &fixture.reconciler.store;
    store.session("lease-cleanup", None, Some("g1")).await?;
    let lease = fixture
        .acquire_cleanup_lease("g1-owner")
        .await?
        .context("g1 lease")?;
    let mut activation = SessionActivate::new("lease-cleanup");
    let execution =
        WorkerExecution::claim(store.clone(), &mut activation, &lease, &fixture.js).await?;
    let context = execution.fence.as_ref().unwrap().context.clone();
    let backend = NatsSessionLogBackend::new(fixture.js.clone(), "lease-cleanup")
        .with_execution(execution.fence.clone());
    let config = crate::config::GlobalConfig::default();
    let abort = harnx_core::abort::create_abort_signal();
    config.write().maintenance_abort = Some(abort.clone());
    let (release, held) = tokio::sync::oneshot::channel();
    let cleanup = tokio::spawn(async {
        held.await?;
        Ok(())
    });
    store
        .cancel_operation(context.operation(), Some("lease-stop"), false)
        .await?;
    abort.set_ctrlc();
    tokio::time::timeout(
        Duration::from_secs(2),
        execution.finish(&backend, &lease, config, Some(cleanup)),
    )
    .await??;
    assert!(!lease.is_held());
    assert!(!store.gate_cleanup(&context).await?.owner_stopped);
    let g2 = store.session("lease-cleanup", None, Some("g2")).await?;
    let lease2 = fixture
        .acquire_cleanup_lease("g2-owner")
        .await?
        .context("G2 must not queue behind G1 cleanup")?;
    store
        .claim(
            &g2.reference,
            Owner {
                instance_id: lease2.worker_id().into(),
                fence: lease2.fence_token(),
            },
        )
        .await?;
    let next = store.activate_gate(&g2.reference).await?;
    release.send(()).unwrap();
    wait_cleanup(store, &context, CleanupState::Confirmed).await?;
    assert!(lease2.is_held());
    assert_eq!(
        store.current("lease-cleanup").await?.unwrap().reference,
        g2.reference
    );
    assert_eq!(
        store.gate_cleanup(&next).await?.state,
        CleanupState::Pending
    );
    lease2.release().await?;
    Ok(())
}

async fn wait_cleanup(
    store: &ExecutionStore,
    ctx: &ExecutionContext,
    state: CleanupState,
) -> Result<CleanupStatus> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut watch = store.watch().await?;
        loop {
            let cleanup = store.gate_cleanup(ctx).await?;
            if cleanup.state == state {
                return Ok(cleanup);
            }
            watch.next().await.context("cleanup watch closed")??;
        }
    })
    .await?
}
