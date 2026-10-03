//! Real macro -> command -> NATS worker execution. Only the model completion is a
//! deterministic stub; admissions, transcript writes and KV reads use the broker.
use crate::{common::NatsServerHandle, worker};
use anyhow::{Context, Result};
use harnx_core::{event::NullSink, sink::with_agent_event_sink};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    config::{macro_execute, ConfigLock, GlobalConfig, NatsRouting},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{
        AdmissionOrigin, InvocationEdgeKind, RunLimitsRecord, SessionMetadataStore,
    },
    utils::{create_abort_signal, AbortSignal},
};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

#[derive(Clone, Debug)]
struct ObservedStep {
    prompt: String,
    storage: String,
    record: RunLimitsRecord,
}

#[derive(Clone)]
struct ModelGate {
    prompt: String,
    entered: CancellationToken,
    release: CancellationToken,
}

struct Fixture {
    _server: NatsServerHandle,
    _env: Vec<worker::EnvGuard>,
    files: tempfile::TempDir,
    config: GlobalConfig,
    js: async_nats::jetstream::Context,
    store: SessionMetadataStore,
    observations: Arc<Mutex<Vec<ObservedStep>>>,
    gate: Arc<Mutex<Option<ModelGate>>>,
    _daemon: AbortOnDropHandle<Result<()>>,
}

impl Fixture {
    async fn new(policy: &str, block: Option<CancellationToken>) -> Result<Self> {
        harnx_core::require_nextest();
        let server = worker::require_nats_server()
            .await?
            .context("nats-server required for macro coverage")?;
        let files = tempfile::tempdir()?;
        let mut env = vec![worker::EnvGuard::set(
            "HARNX_CONFIG_DIR",
            files.path().to_str().unwrap(),
        )];
        let scope =
            harnx_core::instance::ServerScope::from_string(uuid::Uuid::new_v4().to_string());
        env.extend(worker::EnvGuard::tool_server_environment(
            &scope,
            server.url(),
        ));
        std::fs::create_dir(files.path().join("agents"))?;
        std::fs::create_dir(files.path().join("macros"))?;
        for (name, timeout) in [("helper", "604800"), ("short", "30")] {
            std::fs::write(
                files.path().join(format!("agents/{name}.md")),
                format!("---\nrun_limits:\n  timeout_secs: {timeout}\n---\nMacro test target\n"),
            )?;
        }
        let mut runtime = worker::local_nats_config(worker::NatsServerSpec {
            name: "local",
            url: server.url(),
            token: None,
        });
        runtime.nats_routing = NatsRouting::Cluster("local".into());
        runtime.data.run_limits = serde_yaml::from_str(policy)?;
        let config = Arc::new(ConfigLock::new(runtime));
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        let observations = Arc::new(Mutex::new(Vec::new()));
        let gate: Arc<Mutex<Option<ModelGate>>> = Arc::new(Mutex::new(None));
        let call_fn: AgentCallFn = {
            let observations = observations.clone();
            let gate = gate.clone();
            Arc::new(move |input, config, _abort| {
                let cfg = config.read();
                let observed = ObservedStep {
                    prompt: input.text(),
                    storage: cfg.session.as_ref().unwrap().storage_key(),
                    record: cfg
                        .run_context
                        .clone()
                        .expect("worker froze invocation before model"),
                };
                let gate = gate.lock().clone().filter(|g| g.prompt == observed.prompt);
                let observations = observations.clone();
                let block = block.clone();
                Box::pin(async move {
                    let blocked = observed.prompt == "blocked step";
                    observations.lock().push(observed);
                    if let Some(gate) = gate {
                        gate.entered.cancel();
                        gate.release.cancelled().await;
                    }
                    if blocked {
                        let _drop = block
                            .expect("blocked completion has a drop probe")
                            .drop_guard();
                        std::future::pending::<()>().await;
                    }
                    Ok(("step finished".into(), None, vec![], Default::default()))
                })
            })
        };
        let daemon =
            worker::spawn_worker_daemon_with_call_fn(config.clone(), "macro-worker", call_fn)
                .await?;
        Ok(Self {
            _server: server,
            _env: env,
            files,
            config,
            js,
            store,
            observations,
            gate,
            _daemon: AbortOnDropHandle::new(daemon),
        })
    }

    fn hold_model(&self, prompt: &str) -> ModelGate {
        let gate = ModelGate {
            prompt: prompt.into(),
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        };
        *self.gate.lock() = Some(gate.clone());
        gate
    }

    fn write_macro(&self, name: &str, steps: &[&str]) -> Result<()> {
        std::fs::write(
            self.files.path().join(format!("macros/{name}.yaml")),
            serde_yaml::to_string(&serde_json::json!({"steps": steps}))?,
        )?;
        Ok(())
    }

    async fn run(&self, name: &str, abort: AbortSignal) -> Result<()> {
        tokio::time::timeout(
            worker::CI_SAFE_TIMEOUT,
            with_agent_event_sink(
                Arc::new(NullSink),
                macro_execute(&self.config, name, None, abort),
            ),
        )
        .await
        .context("macro test backstop expired")?
    }

    async fn persisted_step(&self, step: &ObservedStep) -> Result<RunLimitsRecord> {
        // A fresh store/client reads the actual immutable KV payload, not config's copy.
        let store = SessionMetadataStore::ensure(
            &async_nats::jetstream::new(async_nats::connect(self._server.url()).await?),
            1,
        )
        .await?;
        let record = store
            .get_invocation_limits(&step.storage, step.record.invocation_id.as_str())
            .await?
            .context("step record persisted")?;
        assert_eq!(record, step.record);
        let intent = store
            .admission(&step.storage, step.record.invocation_id.as_str())
            .await?
            .context("step intent persisted")?;
        assert_eq!(intent.run_id, record.run_id);
        assert_eq!(intent.admitted_at, record.admitted_at);
        Ok(record)
    }
}

#[test]
fn finite_macro_root_is_persisted_before_steps_and_nested_deadlines_do_not_renew() -> Result<()> {
    on_macro_stack(|| async {
        let fixture = Fixture::new("timeout_secs: 120", None).await?;
        fixture.write_macro(
            "inner",
            &[
                ".agent short@local",
                "inner first",
                ".macro deep",
                "inner last",
            ],
        )?;
        fixture.write_macro("deep", &["deep step"])?;
        fixture.write_macro(
            "outer",
            &[
                ".agent helper@local",
                "outer first",
                ".macro inner",
                "outer last",
            ],
        )?;
        let gate = fixture.hold_model("outer first");
        let before = chrono::Utc::now();
        let (result, original) = tokio::join!(fixture.run("outer", create_abort_signal()), async {
            tokio::time::timeout(worker::CI_SAFE_TIMEOUT, gate.entered.cancelled()).await?;
            let first = fixture.observations.lock()[0].clone();
            let root_storage =
                worker::storage_key(&format!("macro-{}", first.record.run_id.as_str()));
            let root = fixture
                .store
                .get_run_limits(&root_storage, first.record.run_id.as_str())
                .await?
                .context("root already persisted while first model is blocked")?;
            assert_eq!(
                fixture
                    .store
                    .get_invocation_limits(&root_storage, root.invocation_id.as_str())
                    .await?,
                Some(root.clone())
            );
            fixture.config.write().data.run_limits = serde_yaml::from_str("timeout_secs: 240")?;
            gate.release.cancel();
            Ok::<_, anyhow::Error>(root)
        });
        result?;
        let original = original?;
        let steps = fixture.observations.lock().clone();
        assert_eq!(
            steps.iter().map(|s| s.prompt.as_str()).collect::<Vec<_>>(),
            [
                "outer first",
                "inner first",
                "deep step",
                "inner last",
                "outer last"
            ]
        );
        let first = fixture.persisted_step(&steps[0]).await?;
        let root_id = first
            .parent_invocation
            .as_ref()
            .context("first step inherits macro root")?
            .invocation_id
            .clone();
        let root_storage = worker::storage_key(&format!("macro-{}", first.run_id.as_str()));
        let root = fixture
            .store
            .get_run_limits(&root_storage, first.run_id.as_str())
            .await?
            .context("macro root persisted before first step")?;
        assert_eq!(
            root, original,
            "config reload cannot replace the admitted macro record"
        );
        assert!(root.admitted_at >= before && root.admitted_at <= first.admitted_at);
        assert_eq!(root.invocation_id, root_id);
        assert!(root.parent_invocation.is_none());
        assert_eq!(
            root.deadline,
            Some(root.admitted_at + chrono::Duration::seconds(120))
        );
        assert_eq!(
            fixture
                .store
                .get_invocation_limits(&root_storage, root_id.as_str())
                .await?,
            Some(root.clone())
        );
        let mut parent = root.clone();
        for step in &steps {
            let record = fixture.persisted_step(step).await?;
            let link = record
                .parent_invocation
                .as_ref()
                .context("macro continuation parent")?;
            assert_eq!(link.edge_kind, InvocationEdgeKind::MacroContinuation);
            assert_eq!(
                link.invocation_id, parent.invocation_id,
                "nested macro must return its latest scope to enclosing macro"
            );
            assert_eq!(record.run_id, root.run_id);
            assert!(record.deadline <= parent.deadline);
            parent = record;
        }
        assert_eq!(steps[2].record.deadline, steps[1].record.deadline);
        assert_eq!(steps[3].record.deadline, steps[1].record.deadline);
        assert_eq!(steps[4].record.deadline, steps[1].record.deadline);
        assert_eq!(
            fixture
                .store
                .get_run_limits(&root_storage, root.run_id.as_str())
                .await?,
            Some(root.clone())
        );
        fixture.write_macro("next", &[".agent helper@local", "new external macro"])?;
        fixture.run("next", create_abort_signal()).await?;
        let next = fixture.observations.lock().last().unwrap().clone();
        assert_ne!(next.record.run_id, root.run_id);
        let next_storage = worker::storage_key(&format!("macro-{}", next.record.run_id.as_str()));
        let next_root = fixture
            .store
            .get_run_limits(&next_storage, next.record.run_id.as_str())
            .await?
            .unwrap();
        assert_eq!(
            next_root.deadline,
            Some(next_root.admitted_at + chrono::Duration::seconds(240))
        );
        assert_eq!(
            fixture
                .store
                .get_run_limits(&root_storage, root.run_id.as_str())
                .await?,
            Some(root)
        );
        assert!(
            fixture.config.read().run_context.is_none(),
            "macro execution keeps frontend context isolated"
        );
        Ok(())
    })
}

#[test]
fn default_and_nonpositive_macros_freeze_24_hour_roots_and_external_calls_are_independent(
) -> Result<()> {
    on_macro_stack(|| async {
        let fixture = Fixture::new("{}", None).await?;
        fixture.write_macro(
            "work",
            &[".agent helper@local", "first step", "second step"],
        )?;
        for policy in [
            "{}",
            "timeout_secs: null",
            "timeout_secs: 0",
            "timeout_secs: -1",
            "timeout_secs: -9223372036854775808",
        ] {
            fixture.config.write().data.run_limits = serde_yaml::from_str(policy)?;
            fixture.run("work", create_abort_signal()).await?;
        }
        let steps = fixture.observations.lock().clone();
        assert_eq!(steps.len(), 10);
        let mut runs = std::collections::HashSet::new();
        for pair in steps.as_chunks::<2>().0 {
            assert!(runs.insert(pair[0].record.run_id.clone()));
            let storage = harnx_core::session_identity::session_key(
                None,
                &format!("macro-{}", pair[0].record.run_id.as_str()),
            );
            let root = fixture
                .store
                .get_run_limits(&storage, pair[0].record.run_id.as_str())
                .await?
                .context("default macro root missing")?;
            assert!(root.parent_invocation.is_none());
            assert_eq!(
                root.deadline,
                Some(root.admitted_at + chrono::Duration::seconds(86400))
            );
            assert_eq!(
                pair[0]
                    .record
                    .parent_invocation
                    .as_ref()
                    .unwrap()
                    .invocation_id,
                root.invocation_id
            );
            assert_eq!(pair[0].record.deadline, root.deadline);
            assert_eq!(pair[1].record.deadline, root.deadline);
            assert_eq!(pair[0].record.run_id, pair[1].record.run_id);
            assert_eq!(
                pair[1]
                    .record
                    .parent_invocation
                    .as_ref()
                    .unwrap()
                    .invocation_id,
                pair[0].record.invocation_id
            );
            assert_eq!(
                pair[1].record.parent_invocation.as_ref().unwrap().edge_kind,
                InvocationEdgeKind::MacroContinuation
            );
            let intent = fixture
                .store
                .admission(&pair[0].storage, pair[0].record.invocation_id.as_str())
                .await?
                .unwrap();
            assert_eq!(intent.origin, AdmissionOrigin::Inherited);
            fixture.persisted_step(&pair[0]).await?;
            fixture.persisted_step(&pair[1]).await?;
        }
        Ok(())
    })
}

#[test]
fn macro_deadline_drops_blocked_step_and_never_executes_later_steps() -> Result<()> {
    on_macro_stack(|| async {
        let dropped = CancellationToken::new();
        // The finite deadline is the behavior under test, not a startup backstop.
        // Leave setup margin for broker/worker contention; drop notification avoids
        // a separate fixed sleep after expiry.
        let fixture = Fixture::new("timeout_secs: 10", Some(dropped.clone())).await?;
        fixture.write_macro(
            "blocked",
            &[
                ".agent helper@local",
                "blocked step",
                ".not-a-command",
                "forbidden later work",
            ],
        )?;
        let abort = create_abort_signal();
        let error = fixture.run("blocked", abort.clone()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("autonomous macro deadline expired"),
            "{error:#}"
        );
        assert!(abort.aborted());
        tokio::time::timeout(worker::CI_SAFE_TIMEOUT, dropped.cancelled()).await?;
        let steps = fixture.observations.lock().clone();
        assert_eq!(steps.len(), 1, "no later model step dispatched");
        let record = fixture.persisted_step(&steps[0]).await?;
        assert!(record.is_expired_at(chrono::Utc::now()));
        worker::poll_until(async || {
            Ok(NatsSessionLog::new(fixture.js.clone(), &steps[0].storage)
                .load_events_latest_async()
                .await?
                .iter()
                .any(|(_, e)| matches!(e, harnx_core::session::SessionLogEntry::Cancel { .. })))
        })
        .await?;
        fixture.write_macro("fresh", &[".agent helper@local", "fresh work"])?;
        fixture.run("fresh", create_abort_signal()).await?;
        let fresh = fixture.observations.lock().last().unwrap().clone();
        assert_eq!(fresh.prompt, "fresh work");
        assert_ne!(fresh.record.run_id, record.run_id);
        assert!(!fresh.record.is_expired_at(chrono::Utc::now()));
        assert_eq!(fixture.persisted_step(&steps[0]).await?, record);
        assert!(fixture.config.read().run_context.is_none());
        Ok(())
    })
}

#[test]
fn expired_inherited_macro_rejects_even_a_ready_command_before_dispatch() -> Result<()> {
    on_macro_stack(|| async {
        use harnx_runtime::nats_session_metadata::{
            CallTimeoutOverride, InvocationIdentity, RunIdentity,
        };
        let fixture = Fixture::new("{}", None).await?;
        fixture.write_macro("expired", &[".not-a-command", "forbidden later work"])?;
        let parent = RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            chrono::Utc::now() - chrono::Duration::seconds(30),
            Default::default(),
            None,
            CallTimeoutOverride::from_optional(Some(1)),
        )?;
        fixture
            .store
            .put_run_limits("expired-caller", &parent)
            .await?;
        fixture
            .store
            .put_invocation_limits("expired-caller", &parent)
            .await?;
        fixture.config.write().run_context = Some(parent.clone());
        let abort = create_abort_signal();
        let error = fixture.run("expired", abort.clone()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("autonomous macro deadline expired"),
            "ready command was executed after expiry: {error:#}"
        );
        assert!(abort.aborted());
        assert!(fixture.observations.lock().is_empty());
        assert_eq!(
            fixture
                .store
                .get_run_limits("expired-caller", parent.run_id.as_str())
                .await?,
            Some(parent.clone())
        );
        assert_eq!(fixture.config.read().run_context.as_ref(), Some(&parent));
        Ok(())
    })
}

fn on_macro_stack<F: std::future::Future<Output = Result<()>>>(
    make_future: impl FnOnce() -> F + Send,
) -> Result<()> {
    // Debug dot-command futures are large, and nested macros poll them recursively.
    // Match the worker's 8MiB stack without changing every test's process environment.
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .thread_stack_size(8 * 1024 * 1024)
                    .enable_all()
                    .build()?
                    .block_on(make_future())
            })?;
        match thread.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[test]
fn inherited_macro_uses_real_caller_run_without_minting_or_retiming_a_root() -> Result<()> {
    on_macro_stack(|| async {
        let fixture = Fixture::new("timeout_secs: 120", None).await?;
        let parent = harnx_runtime::NatsSession::from_global_config(
            harnx_runtime::NatsSessionConfig {
                cluster: "local".into(),
                initializer: harnx_runtime::SessionInitializer::named("helper", Default::default()),
                session_id: None,
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            &fixture.config,
            create_abort_signal(),
        )
        .await?
        .with_external_admission()
        .with_admission_timeout(Some(120));
        let result = parent
            .run_turn("caller instruction", Arc::new(NullSink), None)
            .await?;
        let intent = fixture
            .store
            .prompt_admission(parent.storage_key(), &result.user_msg_id)
            .await?
            .unwrap();
        let original = fixture
            .store
            .get_run_limits(parent.storage_key(), intent.run_id.as_str())
            .await?
            .unwrap();
        fixture.config.write().run_context = Some(original.clone());
        fixture.config.write().data.run_limits = serde_yaml::from_str("timeout_secs: 1")?;
        fixture.write_macro("inner", &["nested inherited step"])?;
        fixture.write_macro(
            "inherited",
            &[
                ".agent helper@local",
                "inherited first",
                ".macro inner",
                "inherited last",
            ],
        )?;
        fixture.run("inherited", create_abort_signal()).await?;
        let steps = fixture.observations.lock().clone();
        assert_eq!(
            steps.iter().map(|s| s.prompt.as_str()).collect::<Vec<_>>(),
            [
                "caller instruction",
                "inherited first",
                "nested inherited step",
                "inherited last"
            ]
        );
        let mut prior = original.clone();
        for step in &steps[1..] {
            let record = fixture.persisted_step(step).await?;
            assert_eq!(record.run_id, original.run_id);
            assert_eq!(
                record.deadline, original.deadline,
                "new global 1s policy cannot replace inherited scope"
            );
            assert_eq!(
                record.parent_invocation.as_ref().unwrap().invocation_id,
                prior.invocation_id
            );
            assert_eq!(
                record.parent_invocation.as_ref().unwrap().edge_kind,
                InvocationEdgeKind::MacroContinuation
            );
            let admission = fixture
                .store
                .admission(&step.storage, record.invocation_id.as_str())
                .await?
                .unwrap();
            assert_eq!(admission.origin, AdmissionOrigin::Inherited);
            prior = record;
        }
        assert_eq!(fixture.config.read().run_context.as_ref(), Some(&original));
        assert_eq!(
            fixture
                .store
                .get_run_limits(parent.storage_key(), original.run_id.as_str())
                .await?,
            Some(original.clone())
        );
        assert!(
            fixture
                .store
                .get_run_limits(
                    &worker::storage_key(&format!("macro-{}", original.run_id.as_str())),
                    original.run_id.as_str()
                )
                .await?
                .is_none(),
            "inherited execution did not manufacture a macro root record"
        );
        Ok(())
    })
}
