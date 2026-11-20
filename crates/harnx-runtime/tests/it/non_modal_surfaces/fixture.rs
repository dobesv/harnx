//! The non-modal surface test fixture: the surface-publishing toolset, its
//! parked slow target, and the worker-daemon session the tests drive.

use crate::common;
use crate::worker::{
    await_worker_ready, local_nats_runtime_config, start_tool_server, EnvGuard, CI_SAFE_TIMEOUT,
};
use anyhow::Result;
use async_trait::async_trait;
use harnx_core::instance::ServerScope;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session::{NatsSession, NatsSessionConfig};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer as MetadataInitializer, SessionOverrides,
};
use harnx_runtime::nats_worker::worker_ready_subject;
use harnx_runtime::SessionActivationRoute;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

use super::envelope::publisher_output;
use super::helpers::first_published_surface;
use super::stub::surface_call_fn;

use harnx_core::abort::create_abort_signal;
use harnx_core::require_nextest;

#[derive(Clone, Default)]
pub(super) struct SurfaceTools {
    pub(super) counter: Arc<AtomicUsize>,
    /// Executions of the `slow` tool: the at-most-once tests count these.
    pub(super) slow_counter: Arc<AtomicUsize>,
    /// Releases the in-flight `slow` tool. The tool parks after recording its
    /// execution until the test flips this, so a test controls exactly when
    /// the direct tool call completes.
    pub(super) slow_release: Arc<AtomicBool>,
}

#[async_trait]
impl Toolset for SurfaceTools {
    fn name(&self) -> &str {
        "surfaces"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        ["publish", "counter", "slow", "useronly"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: "Non-modal surface test tool".into(),
                input_schema: json!({"type": "object"}),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: None,
                meta: None,
                cancellation_guarantee: Default::default(),
            })
            .collect()
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        match tool {
            "publish" => Ok(publisher_output()),
            "counter" => {
                // A realistic model cadence: a busy turn appends every few
                // hundred milliseconds, not every loop iteration. The
                // at-most-once tests deliberately race surface action
                // appends against this.
                tokio::time::sleep(Duration::from_millis(250)).await;
                let count = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(json!({ "count": count }))
            }
            "slow" => {
                let count = self.slow_counter.fetch_add(1, Ordering::SeqCst) + 1;
                // Park inside the call: the test appends to the log and
                // flips `slow_release` while the direct tool call is running.
                while !self.slow_release.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(json!({ "slow": count }))
            }
            "useronly" => Ok(json!({
                "status": "useronly-ok",
                "content": [
                    { "type": "text", "text": "useronly-ok" },
                    { "type": "resource",
                      "resource": {
                          "uri": "a2ui://useronly-secret-card",
                          "mimeType": "application/a2ui+json",
                          "text": serde_json::to_string(&json!({
                              "version": "1.0",
                              "createSurface": {
                                  "surfaceId": "useronly-secret-card",
                                  "components": []
                              }
                          })).unwrap()
                      },
                      "annotations": { "audience": ["user"] } }
                ]
            })),
            other => Err(ToolInvokeError::Fatal(format!("unknown tool {other}"))),
        }
    }
}

/// Model-visible request per round, captured by the stub model: each model
/// message serialized to JSON, so non-text parts (tool results) are checked
/// for leaks too — `MessageContent::to_text` drops tool-round content.
pub(super) struct SurfaceFixture {
    pub(super) session: NatsSession,
    pub(super) log: NatsSessionLog,
    pub(super) jetstream: async_nats::jetstream::Context,
    /// Raw client for control-subject traffic tests (abandoned first
    /// attempts, duplicate commands from old-style resending clients).
    pub(super) client: async_nats::Client,
    _server: common::NatsServerHandle,
    _env: [EnvGuard; 4],
    _tool_server: AbortOnDropHandle<Result<()>>,
    pub(super) daemon: tokio::task::JoinHandle<Result<()>>,
    pub(super) counter: Arc<AtomicUsize>,
    pub(super) slow_counter: Arc<AtomicUsize>,
    pub(super) slow_release: Arc<AtomicBool>,
}

impl SurfaceFixture {
    pub(super) async fn start(mode: &'static str) -> Result<Self> {
        require_nextest();
        let Some(server) = common::spawn_nats_server().await? else {
            eprintln!("skipping: nats-server not available");
            return Err(anyhow::anyhow!("nats-server not available"));
        };
        let scope = ServerScope::new();
        let mut env = Vec::new();
        env.extend(EnvGuard::tool_server_environment(&scope, server.url()));
        // Keep the control-plane recovery budget short so idle-path tests
        // exercise the fallback quickly; worker-path tests are unaffected
        // because the worker acknowledges before the budget can elapse.
        env.push(EnvGuard::set(
            "HARNX_SURFACE_ACTION_RECOVERY_TIMEOUT_MS",
            "800",
        ));
        let env: [EnvGuard; 4] = [
            env.pop().unwrap(),
            env.pop().unwrap(),
            env.pop().unwrap(),
            env.pop().unwrap(),
        ];
        let client = async_nats::connect(server.url()).await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let tools = SurfaceTools::default();
        let counter = tools.counter.clone();
        let slow_counter = tools.slow_counter.clone();
        let slow_release = tools.slow_release.clone();
        // Serve the toolset and wait for its registration so the first model
        // round can never race the discovery.
        let _tool_server =
            start_tool_server(&jetstream, client.clone(), scope.clone(), Arc::new(tools)).await?;
        let config = local_nats_runtime_config(server.url());
        // A managing daemon mints its own server scope and would never see the
        // fixture's tool server. `WorkerDaemonConfig::new` is non-managing, so
        // the daemon resolves the scope EnvGuard just propagated and finds
        // the registered toolset like a deployed worker would.
        let ready_client = async_nats::connect(server.url()).await?;
        let mut ready = ready_client
            .subscribe(worker_ready_subject("local"))
            .await?;
        ready_client.flush().await?;
        let worker_config =
            harnx_runtime::nats_worker::WorkerDaemonConfig::new("local", "worker-non-modal");
        let call_fn = surface_call_fn(mode);
        let mut daemon = tokio::spawn(async move {
            harnx_runtime::nats_worker::run_worker_daemon(
                config,
                worker_config,
                Some(call_fn),
                None,
            )
            .await
        });
        await_worker_ready(&mut daemon, &mut ready).await?;

        let session_id = format!("non-modal-{}", uuid::Uuid::new_v4());
        let session = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".to_string(),
                initializer: MetadataInitializer::inline(
                    "",
                    Default::default(),
                    SessionOverrides {
                        use_tools: Some(vec!["surfaces_*".into()]),
                        ..Default::default()
                    },
                ),
                session_id: Some(session_id.clone()),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            client,
            jetstream.clone(),
            create_abort_signal(),
        )
        .await?;
        let key = session.storage_key().to_string();
        let log = NatsSessionLog::new_with_replicas(jetstream.clone(), key, 1);
        let fixture_client = async_nats::connect(server.url()).await?;
        Ok(Self {
            session,
            log,
            jetstream,
            client: fixture_client,
            _server: server,
            _env: env,
            _tool_server,
            daemon,
            counter,
            slow_counter,
            slow_release,
        })
    }

    pub(super) async fn entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        self.log.load_events_async().await
    }

    /// The published surface's identity from the durable log.
    pub(super) async fn wait_surface_published(&self) -> Result<(String, u64)> {
        let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
        loop {
            let entries = self.entries().await?;
            if let Some((seq, surface_id)) = first_published_surface(&entries) {
                return Ok((surface_id, seq));
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "surface never published durably; entries: {entries:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
