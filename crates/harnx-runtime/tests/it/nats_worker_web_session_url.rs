//! Worker URLs must be durable before the first model call, without serve.
//!
//! `require_nats_server` requires nextest's process-per-test isolation. EnvGuard
//! restores every override; the sandbox strips ambient HARNX_* variables.

use crate::{common::NatsServerHandle, worker};
use anyhow::{Context, Result};
use harnx_core::{session::SessionLogEntry, session_identity::session_key, tool::ToolCall};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    client::CompletionTokenUsage,
    config::{Config, GlobalConfig},
    nats_session_metadata::{
        session_properties, PropertySource, SessionInitializer, SessionMetadataStore,
        WEB_SESSION_URL_PROPERTY,
    },
    nats_worker::{
        run_worker_daemon, targeted_worker_ready_subject, LocalWorkerTarget, WorkerDaemonConfig,
    },
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig, SessionActivationRoute,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;
use worker::{EnvGuard, CI_SAFE_TIMEOUT};

const BASE: &str = "https://example.test";
const WORKER_ID: &str = "worker-web-session-url";

struct Fixture {
    _server: NatsServerHandle,
    _root: tempfile::TempDir,
    _guards: Vec<EnvGuard>,
    config: GlobalConfig,
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    store: SessionMetadataStore,
}

impl Fixture {
    async fn new(base: Option<&str>) -> Result<Option<Self>> {
        let Some(server) = worker::require_nats_server().await? else {
            return Ok(None);
        };
        let root = tempfile::tempdir()?;
        let guards = vec![
            EnvGuard::set(
                "HARNX_CONFIG_DIR",
                root.path().to_str().context("config path")?,
            ),
            EnvGuard::set("HARNX_NATS_SERVER", "local"),
            EnvGuard::set("HARNX_NATS_URL", server.url()),
            EnvGuard::set("HARNX_NATS_TOKEN", ""),
        ];
        let config = worker::local_nats_runtime_config(server.url());
        config.write().serve_public_url = base.map(str::to_string);
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        Ok(Some(Self {
            _server: server,
            _root: root,
            _guards: guards,
            config,
            client,
            js,
            store,
        }))
    }

    async fn wait_for_turn(&self, agent: &str, id: &str) -> Result<()> {
        let log = harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
            self.js.clone(),
            session_key(Some(agent), id),
            1,
        );
        worker::poll_until(async || {
            let entries = log.load_events_async().await?;
            anyhow::ensure!(
                !entries
                    .iter()
                    .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
                "worker turn failed: {entries:?}"
            );
            Ok(entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. })))
        })
        .await
    }

    async fn agent(&self, name: &str, body: &str) -> Result<()> {
        let path = Config::agent_file(name);
        tokio::fs::create_dir_all(path.parent().context("agent directory")?).await?;
        tokio::fs::write(path, body).await?;
        Ok(())
    }

    async fn daemon(&self, call_fn: AgentCallFn) -> Result<AbortOnDropHandle<Result<()>>> {
        Ok(AbortOnDropHandle::new(
            worker::spawn_worker_daemon_with_call_fn(self.config.clone(), WORKER_ID, call_fn)
                .await?,
        ))
    }

    async fn local_daemon(&self, call_fn: AgentCallFn) -> Result<AbortOnDropHandle<Result<()>>> {
        let mut ready = self
            .client
            .subscribe(targeted_worker_ready_subject(LocalWorkerTarget::new(
                "__local__",
                WORKER_ID,
            )?))
            .await?;
        self.client.flush().await?;
        let config = self.config.clone();
        let daemon_config = WorkerDaemonConfig::local(WORKER_ID)?;
        let mut daemon = AbortOnDropHandle::new(tokio::spawn(async move {
            run_worker_daemon(config, daemon_config, Some(call_fn), None).await
        }));
        worker::await_worker_ready(&mut daemon, &mut ready).await?;
        Ok(daemon)
    }

    async fn session(
        &self,
        initializer: SessionInitializer,
        id: &str,
        local: bool,
    ) -> Result<NatsSession> {
        NatsSession::new(
            NatsSessionConfig {
                cluster: if local { "__local__" } else { "local" }.to_string(),
                initializer,
                session_id: Some(id.to_string()),
                activation_route: if local {
                    SessionActivationRoute::WorkerTargeted {
                        session_scope: "__local__".to_string(),
                        worker_id: WORKER_ID.to_string(),
                    }
                } else {
                    SessionActivationRoute::ClusterShared
                },
            },
            self.client.clone(),
            self.js.clone(),
            create_abort_signal(),
        )
        .await
    }
}

fn worker_property(url: &str) -> Value {
    json!({"value": url, "inherit": false, "source": "configured", "recorded_by": "worker"})
}

async fn assert_property(
    store: &SessionMetadataStore,
    agent: Option<&str>,
    id: &str,
    expected: &Value,
) -> Result<()> {
    let record = store
        .get(&session_key(agent, id))
        .await?
        .context("metadata exists before the model runs")?;
    assert_eq!(record.metadata.session_id, id);
    assert_eq!(record.metadata.agent.name(), agent);
    let properties = session_properties(&record.metadata)?;
    assert_eq!(
        serde_json::to_value(properties.get(WEB_SESSION_URL_PROPERTY))?,
        *expected,
        "URL property at first model call for {agent:?}/{id}"
    );
    Ok(())
}

fn checking_call(
    store: SessionMetadataStore,
    agent: Option<&'static str>,
    id: &'static str,
    expected: Value,
) -> (AgentCallFn, mpsc::UnboundedReceiver<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let call: AgentCallFn = Arc::new(move |_input, _config, _abort| {
        let store = store.clone();
        let expected = expected.clone();
        let tx = tx.clone();
        Box::pin(async move {
            // Read fresh KV metadata, not the worker's pre-recording snapshot.
            assert_property(&store, agent, id, &expected).await?;
            tx.send(()).context("test still waiting for model call")?;
            Ok((
                "done".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    });
    (call, rx)
}

async fn enqueue_and_observe(
    session: &NatsSession,
    rx: &mut mpsc::UnboundedReceiver<()>,
) -> Result<()> {
    session
        .clone()
        .with_external_admission()
        .enqueue_text("start without serve")
        .await?
        .into_activation_result()?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, rx.recv())
        .await?
        .context("first model callback checked metadata")?;
    Ok(())
}

/// Shared setup for tests that check web_session_url before the first model call.
/// Returns the session, daemon handle, and receiver for observing the model call.
async fn setup_web_session_url_test(
    session_id: &'static str,
    expected_property: Value,
    f: &Fixture,
) -> Result<(
    NatsSession,
    AbortOnDropHandle<Result<()>>,
    mpsc::UnboundedReceiver<()>,
)> {
    let session = f
        .session(
            SessionInitializer::named("metis", Default::default()),
            session_id,
            false,
        )
        .await?;
    let (call, rx) = checking_call(
        f.store.clone(),
        Some("metis"),
        session_id,
        expected_property,
    );
    let daemon = f.daemon(call).await?;
    Ok((session, daemon, rx))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_url_is_visible_before_first_model_call() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    f.agent("metis", "test agent").await?;
    let (session, _daemon, mut rx) = setup_web_session_url_test(
        "bare-id",
        worker_property("https://example.test/agents/metis/sessions/bare-id"),
        &f,
    )
    .await?;
    enqueue_and_observe(&session, &mut rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfigured_worker_leaves_url_absent() -> Result<()> {
    let Some(f) = Fixture::new(None).await? else {
        return Ok(());
    };
    f.agent("metis", "test agent").await?;
    let (session, _daemon, mut rx) = setup_web_session_url_test("no-base", Value::Null, &f).await?;
    enqueue_and_observe(&session, &mut rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_preserves_preexisting_inferred_url() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    f.agent("metis", "test agent").await?;
    let session = f
        .session(
            SessionInitializer::named("metis", Default::default()),
            "inferred",
            false,
        )
        .await?;
    let url = "https://inferred.test/agents/metis/sessions/inferred";
    f.store
        .record_web_session_url(session.storage_key(), url, PropertySource::Inferred)
        .await?;
    let expected = json!({"value": url, "inherit": false, "source": "inferred"});
    let (call, mut rx) =
        checking_call(f.store.clone(), Some("metis"), "inferred", expected.clone());
    let _daemon = f.daemon(call).await?;
    enqueue_and_observe(&session, &mut rx).await?;
    f.wait_for_turn("metis", "inferred").await?;
    assert_property(&f.store, Some("metis"), "inferred", &expected).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_encodes_agent_like_serve_and_uses_bare_session_id() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    // SessionRef permits only [A-Za-z0-9_-] IDs; arbitrary session-component
    // encoding is covered by the shared builder and worker helper unit tests.
    f.agent("pantheon/metis", "test agent").await?;
    let session = f
        .session(
            SessionInitializer::named("pantheon/metis", Default::default()),
            "encoded-id",
            false,
        )
        .await?;
    let (call, mut rx) = checking_call(
        f.store.clone(),
        Some("pantheon/metis"),
        "encoded-id",
        worker_property("https://example.test/agents/pantheon%2Fmetis/sessions/encoded-id"),
    );
    let _daemon = f.daemon(call).await?;
    enqueue_and_observe(&session, &mut rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_worker_omits_reserved_cluster_from_agent_display() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    // A different frontend default must not add @__local__ to a local route.
    let _default = EnvGuard::set("HARNX_NATS_SERVER", "other");
    f.agent("metis", "test agent").await?;
    let session = f
        .session(
            SessionInitializer::named("metis", Default::default()),
            "local-id",
            true,
        )
        .await?;
    let (call, mut rx) = checking_call(
        f.store.clone(),
        Some("metis"),
        "local-id",
        worker_property("https://example.test/agents/metis/sessions/local-id"),
    );
    let _daemon = f.local_daemon(call).await?;
    enqueue_and_observe(&session, &mut rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_agent_has_no_web_url_even_with_worker_base() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    let session = f
        .session(
            SessionInitializer::inline(
                "inline instructions",
                Default::default(),
                Default::default(),
            ),
            "inline-id",
            false,
        )
        .await?;
    let (call, mut rx) = checking_call(f.store.clone(), None, "inline-id", Value::Null);
    let _daemon = f.daemon(call).await?;
    enqueue_and_observe(&session, &mut rx).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_target_has_its_own_url_before_first_model_call() -> Result<()> {
    let Some(f) = Fixture::new(Some(BASE)).await? else {
        return Ok(());
    };
    f.agent(
        "alpha",
        "---\nuse_tools: beta_session_handoff\n---\nsource instructions",
    )
    .await?;
    f.agent("beta", "target instructions").await?;
    let source = f
        .session(
            SessionInitializer::named("alpha", Default::default()),
            "parent-id",
            false,
        )
        .await?;
    // A serve-originated parent URL must not become the child's URL.
    let parent_url = "https://parent.test/agents/alpha/sessions/parent-id";
    f.store
        .record_web_session_url(source.storage_key(), parent_url, PropertySource::Inferred)
        .await?;
    let store = f.store.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let call: AgentCallFn = Arc::new(move |input, _config, _abort| {
        let agent = input.agent().name().to_string();
        let store = store.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let tools = match agent.as_str() {
                "alpha" => {
                    assert_property(
                        &store,
                        Some("alpha"),
                        "parent-id",
                        &json!({"value": parent_url, "inherit": false, "source": "inferred"}),
                    )
                    .await?;
                    vec![ToolCall::new(
                        "beta_session_handoff".to_string(),
                        json!({"prompt": "finish delegated work", "session_id": "child-id"}),
                        Some("handoff-url".to_string()),
                        None,
                    )]
                }
                "beta" => {
                    assert_property(
                        &store,
                        Some("beta"),
                        "child-id",
                        &worker_property("https://example.test/agents/beta/sessions/child-id"),
                    )
                    .await?;
                    tx.send(()).context("test waiting for target model")?;
                    vec![]
                }
                _ => anyhow::bail!("unexpected model agent {agent}"),
            };
            Ok((
                "done".to_string(),
                None,
                tools,
                CompletionTokenUsage::default(),
            ))
        })
    });
    let _daemon = f.daemon(call).await?;
    enqueue_and_observe(&source, &mut rx).await?;
    f.wait_for_turn("beta", "child-id").await
}
