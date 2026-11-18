use anyhow::Result;
use harnx_core::{session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_worker::NatsSessionLogBackend,
};
use std::sync::Arc;

pub(super) struct AcceptanceFixture {
    pub _server: crate::common::NatsServerHandle,
    pub js: async_nats::jetstream::Context,
    pub backend: NatsSessionLogBackend,
    pub lease: Arc<NatsSessionLease>,
    pub log: NatsSessionLog,
}

impl AcceptanceFixture {
    pub async fn start() -> Result<Self> {
        let server = crate::common::spawn_nats_server()
            .await?
            .expect("NATS required");
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client);
        let session_id = "test-interaction-nats-accept";
        let lease = Arc::new(
            NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id,
                worker_id: "test-worker".into(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            })
            .await?
            .expect("lease acquired"),
        );
        let log = NatsSessionLog::new_with_replicas(js.clone(), session_id, 1);
        let backend = NatsSessionLogBackend::new(js.clone(), session_id, 1);
        append_owning_round(&log, lease.fence_token()).await?;
        Ok(Self {
            _server: server,
            js,
            backend,
            lease,
            log,
        })
    }
}

async fn append_owning_round(log: &NatsSessionLog, fence: u64) -> Result<()> {
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "deploy_prod".into(),
            serde_json::json!({}),
            Some("call-1".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(fence),
    })
    .await?;
    log.append_event_async(&super::sample_interaction_request(2, 1, "int-deploy").1)
        .await?;
    Ok(())
}
