use crate::support::{Broker, DEADLINE};
use anyhow::{Context, Result};
use futures::StreamExt;
use harnx_a2a_server::store::{context::*, A2aStore, TaskRecord};
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_metadata::SessionMetadataStore,
};
use std::{sync::Arc, time::Duration};

pub(crate) const STORAGE: &str = "authority-storage";
pub(crate) const LOCAL: &str = "authority-local";
pub(crate) const TASK: &str = "authority-local.01234567-89ab-cdef-0123-456789abcdef";

pub(crate) struct Fixture {
    pub a: Arc<A2aStore>,
    pub b: Arc<A2aStore>,
    pub metadata: SessionMetadataStore,
    pub js: async_nats::jetstream::Context,
    _broker: Broker,
}
impl Fixture {
    pub async fn start() -> Result<Self> {
        harnx_core::require_nextest();
        let (broker, _, client) = Broker::start().await?;
        let js = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        Ok(Self {
            a: Arc::new(A2aStore::new(metadata.clone())),
            b: Arc::new(A2aStore::new(metadata.clone())),
            metadata,
            js,
            _broker: broker,
        })
    }
    pub fn params(&self, boot: &str) -> NatsLeaseAcquireParams<'static> {
        NatsLeaseAcquireParams {
            jetstream: self.js.clone(),
            session_id: STORAGE,
            worker_id: boot.into(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(2),
                renew_interval: Duration::from_millis(250),
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        }
    }
    pub async fn lease(&self, boot: &str) -> Result<NatsSessionLease> {
        NatsSessionLease::acquire_scoped(self.params(boot), "a2a")
            .await?
            .context("lease unavailable")
    }
    pub async fn claim(
        &self,
        store: &A2aStore,
        lease: &NatsSessionLease,
        op: &str,
    ) -> Result<ContextSnapshot> {
        let write = store
            .prepare_context_claim(
                ContextIdentity {
                    storage_key: STORAGE,
                    local_id: LOCAL,
                },
                lease,
                op,
            )
            .await?;
        store.commit_context(&write).await
    }
    pub async fn active(
        &self,
        store: &A2aStore,
        context: &ContextSnapshot,
    ) -> Result<ContextSnapshot> {
        let active = active_task();
        let write = store
            .prepare_context_update(STORAGE, &context.version()?, "allocate-task", |state| {
                state.active = Some(active)
            })
            .await?;
        store.commit_context(&write).await
    }
}

pub(crate) fn active_task() -> ActiveTask {
    let now = chrono::Utc::now();
    ActiveTask {
        snapshot: TaskRecord {
            version: 1,
            task: a2a_lf::Task {
                id: TASK.into(),
                context_id: LOCAL.into(),
                status: a2a_lf::TaskStatus {
                    state: a2a_lf::TaskState::Working,
                    message: None,
                    timestamp: Some(now),
                },
                history: None,
                artifacts: None,
                metadata: None,
            },
            user_msg_id: String::new(),
            user_msg_seq: 0,
            execution_id: String::new(),
            revision: 1,
            stream_seq: 0,
            created_at: now,
            updated_at: now,
        },
        message: RetainedMessage {
            message_id: "first-message".into(),
            fingerprint: "fingerprint".into(),
        },
        admission: AdmissionState {
            invocation_id: "stable-invocation".into(),
            prompt_id: "stable-prompt".into(),
            fixed_predecessor: 10,
            phase: AdmissionPhase::Reserved,
            prompt_sequence: None,
            closure_id: None,
        },
        cancel: None,
        publication: Default::default(),
        projections: Default::default(),
        stop_confirmed: false,
    }
}

/// Await the actual broker TTL marker; elapsed wall time is not evidence of expiry.
pub(crate) async fn expiry(f: &Fixture, lease: &NatsSessionLease) -> Result<()> {
    let kv = f.js.get_key_value("harnx_leases").await?;
    let mut watch = kv.watch(lease.key()).await?;
    lease.stop_renewal_for_test().await;
    tokio::time::timeout(DEADLINE, async {
        while let Some(entry) = watch.next().await {
            if entry?.operation != async_nats::jetstream::kv::Operation::Put {
                return Ok(());
            }
        }
        anyhow::bail!("lease watch stopped before expiry")
    })
    .await
    .context("lease expiry deadline")?
}

pub(crate) fn assert_authority_error(error: anyhow::Error, expected: AuthorityError) {
    assert_eq!(
        error.downcast_ref::<AuthorityError>(),
        Some(&expected),
        "{error:#}"
    );
}
