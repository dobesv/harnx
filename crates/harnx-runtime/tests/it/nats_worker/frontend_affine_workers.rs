use super::*;

const WORKER_A: &str = "local-affinity-a";
const WORKER_B: &str = "local-affinity-b";

struct TargetedFixture {
    _server: harnx_runtime::nats_local_server::SharedNatsServer,
    _guards: Vec<EnvVarGuard>,
    _root: tempfile::TempDir,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    counter_a: Arc<AtomicUsize>,
    counter_b: Arc<AtomicUsize>,
    hold_entered: Arc<Notify>,
    hold_release: Arc<Notify>,
    daemon_a: tokio::task::JoinHandle<Result<()>>,
    daemon_b: tokio::task::JoinHandle<Result<()>>,
}

impl TargetedFixture {
    async fn start() -> Result<Option<Self>> {
        require_nextest();
        if !nats_server_available() {
            eprintln!("Skipping test: nats-server not available");
            return Ok(None);
        }
        let root = tempfile::tempdir()?;
        let mut guards = isolated_local_environment(root.path())?;
        let server = harnx_runtime::nats_local_server::ensure_shared_server().await?;
        guards.push(EnvVarGuard::set_value("HARNX_NATS_URL", &server.url));
        guards.push(EnvVarGuard::set_value("HARNX_NATS_TOKEN", &server.token));
        let client = async_nats::ConnectOptions::new()
            .token(server.token.clone())
            .connect(&server.url)
            .await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let mut ready_a = subscribe_to_worker(&client, WORKER_A).await?;
        let mut ready_b = subscribe_to_worker(&client, WORKER_B).await?;
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));
        let hold_entered = Arc::new(Notify::new());
        let hold_release = Arc::new(Notify::new());
        let call_a = gated_call_fn(
            Arc::clone(&counter_a),
            Arc::clone(&hold_entered),
            Arc::clone(&hold_release),
        );
        let daemon_a = spawn_targeted_daemon(WORKER_A, call_a);
        let daemon_b =
            spawn_targeted_daemon(WORKER_B, counting_stub_call_fn(Arc::clone(&counter_b)));
        await_readiness(&mut ready_a, "A").await?;
        await_readiness(&mut ready_b, "B").await?;
        Ok(Some(Self {
            _server: server,
            _guards: guards,
            _root: root,
            client,
            jetstream,
            counter_a,
            counter_b,
            hold_entered,
            hold_release,
            daemon_a,
            daemon_b,
        }))
    }

    async fn session(&self, session_id: &str, worker_id: &str) -> Result<NatsSession> {
        NatsSession::new(
            NatsSessionConfig {
                cluster: "__local__".to_string(),
                initializer: harnx_runtime::SessionInitializer::inline(
                    "targeted test agent",
                    Default::default(),
                    Default::default(),
                ),
                session_id: Some(session_id.to_string()),
                activation_route: harnx_runtime::SessionActivationRoute::WorkerTargeted {
                    session_scope: "__local__".to_string(),
                    worker_id: worker_id.to_string(),
                },
            },
            self.client.clone(),
            self.jetstream.clone(),
            create_abort_signal(),
        )
        .await
    }

    async fn acquire_blocker(
        &self,
        session_id: &str,
        worker_id: &str,
    ) -> Result<harnx_runtime::nats_lease::NatsSessionLease> {
        harnx_runtime::nats_lease::NatsSessionLease::acquire(
            harnx_runtime::nats_lease::NatsLeaseAcquireParams {
                jetstream: self.jetstream.clone(),
                session_id: &harnx_core::session_identity::session_key(None, session_id),
                worker_id: worker_id.to_string(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            },
        )
        .await?
        .context("test blocker failed to acquire idle session lease")
    }

    async fn assert_topology_and_poison_termination(&self) -> Result<()> {
        let stream = self.jetstream.get_stream("LOCAL_WORK_NOTIFY_V2").await?;
        assert_eq!(
            stream.cached_info().config.subjects,
            ["session_scope.__local__.workers.*.sessions.notify"]
        );
        assert_eq!(
            stream.cached_info().config.retention,
            async_nats::jetstream::stream::RetentionPolicy::Interest
        );
        assert_eq!(
            stream.cached_info().config.storage,
            async_nats::jetstream::stream::StorageType::File
        );
        let subject = targeted_notify_subject(LocalWorkerTarget::new("__local__", WORKER_B)?);
        self.jetstream
            .publish(subject.clone(), "not-json".into())
            .await?
            .await?;
        let misrouted = SessionActivate::targeted("poison", 1, WORKER_A);
        self.jetstream
            .publish(subject, serde_json::to_vec(&misrouted)?.into())
            .await?
            .await?;
        wait_for_consumer_idle(&stream, WORKER_B).await
    }

    async fn assert_idle_target_affinity(&self) -> Result<()> {
        self.session("target-only-a", WORKER_A)
            .await?
            .clone()
            .with_external_admission()
            .run_turn("only A", Arc::new(NullSink), None)
            .await?;
        assert_eq!(self.counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(self.counter_b.load(Ordering::SeqCst), 0);
        Ok(())
    }

    async fn assert_concurrent_idle_submissions_fold(&self) -> Result<()> {
        let session_id = "target-concurrent-idle";
        let blocker = self.acquire_blocker(session_id, "test-blocker").await?;
        let session_a = self.session(session_id, WORKER_A).await?;
        let session_b = self.session(session_id, WORKER_B).await?;
        let turn_a = tokio::spawn(async move {
            session_a
                .with_external_admission()
                .run_turn("alpha", Arc::new(NullSink), None)
                .await
        });
        wait_for_user_count(&self.jetstream, session_id, 1).await?;
        let conflict = session_b
            .clone()
            .with_external_admission()
            .run_turn("beta", Arc::new(NullSink), None)
            .await
            .unwrap_err();
        assert!(conflict.to_string().contains("session busy"));
        let before = self.total_calls();
        blocker.release().await?;
        let result_a = tokio::time::timeout(CI_SAFE_TIMEOUT, turn_a).await???;
        assert!(result_a.error.is_none());
        let result_b = session_b
            .with_external_admission()
            .run_turn("beta", Arc::new(NullSink), None)
            .await?;
        assert!(result_b.error.is_none());
        assert_eq!(self.total_calls() - before, 2);
        Ok(())
    }

    async fn assert_retained_wakeup_survives_foreign_holder(&self) -> Result<()> {
        let session_id = "target-retained-after-lease";
        let _initialized = self.session(session_id, WORKER_B).await?;
        let log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(None, session_id),
            1,
        );
        let requested_seq =
            crate::worker::append_admitted_fixture_user(&log, "retained", "run after release")
                .await?;
        let blocker = self.acquire_blocker(session_id, "other-holder").await?;
        publish_targeted_session_activate(
            &self.jetstream,
            LocalWorkerTarget::new("__local__", WORKER_B)?,
            &SessionActivate::targeted(
                harnx_core::session_identity::session_key(None, session_id),
                requested_seq,
                WORKER_B,
            ),
        )
        .await?;
        wait_for_targeted_delivery(&self.jetstream, WORKER_B, 1).await?;
        let before = self.counter_b.load(Ordering::SeqCst);
        // While the foreign holder keeps the session, the activation parks on
        // the worker instead of cycling through the broker, and its turn
        // never starts.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(self.counter_b.load(Ordering::SeqCst), before);
        blocker.release().await?;
        // The bound is shorter than any busy-NAK round trip (10-12s delay
        // plus redelivery), so the turn starting within it proves the parked
        // delivery woke on the lease-key watch rather than a broker
        // redelivery.
        wait_until(Duration::from_secs(8), || {
            self.counter_b.load(Ordering::SeqCst) > before
        })
        .await?;
        assert_eq!(self.counter_b.load(Ordering::SeqCst), before + 1);
        Ok(())
    }

    async fn assert_lease_holder_consumes_injection(&self) -> Result<()> {
        let session_id = "target-injection-held-by-a";
        let before_a = self.counter_a.load(Ordering::SeqCst);
        let before_b = self.counter_b.load(Ordering::SeqCst);
        let held_session = self.session(session_id, WORKER_A).await?;
        let held_turn = tokio::spawn(async move {
            held_session
                .clone()
                .with_external_admission()
                .run_turn("hold", Arc::new(NullSink), None)
                .await
        });
        tokio::time::timeout(CI_SAFE_TIMEOUT, self.hold_entered.notified()).await?;
        let injecting_session = self.session(session_id, WORKER_B).await?;
        let injecting_turn = tokio::spawn(async move {
            injecting_session
                .clone()
                .with_external_admission()
                .with_steering_admission()
                .run_turn("injected", Arc::new(NullSink), None)
                .await
        });
        wait_for_user_count(&self.jetstream, session_id, 2).await?;
        self.hold_release.notify_one();
        tokio::time::timeout(CI_SAFE_TIMEOUT, held_turn).await???;
        tokio::time::timeout(CI_SAFE_TIMEOUT, injecting_turn).await???;
        let log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(None, session_id),
            1,
        );
        let entries = log.load_events_async().await?;
        assert_eq!(user_message_texts(&entries), ["hold", "injected"]);
        assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry,
            SessionLogEntry::Message { role, content, .. }
                if role.is_user() && harnx_core::timing_identity::is_timing_notice(&content.to_text())
        )).count(), 1, "steering shares the active invocation's timing notice");
        assert_eq!(self.counter_a.load(Ordering::SeqCst), before_a + 2);
        assert_eq!(self.counter_b.load(Ordering::SeqCst), before_b);
        Ok(())
    }

    fn total_calls(&self) -> usize {
        self.counter_a.load(Ordering::SeqCst) + self.counter_b.load(Ordering::SeqCst)
    }
}

impl Drop for TargetedFixture {
    fn drop(&mut self) {
        self.daemon_a.abort();
        self.daemon_b.abort();
    }
}

fn nats_server_available() -> bool {
    std::env::var_os("NATS_SERVER_BIN")
        .map(PathBuf::from)
        .is_some_and(|path| path.is_file())
        || which::which("nats-server").is_ok()
}

fn isolated_local_environment(root: &Path) -> Result<Vec<EnvVarGuard>> {
    let mut guards = Vec::new();
    for (name, directory) in [
        ("HARNX_CONFIG_DIR", "config"),
        ("HARNX_DATA_DIR", "data"),
        ("HARNX_STATE_DIR", "state"),
    ] {
        let path = root.join(directory);
        std::fs::create_dir_all(&path)?;
        guards.push(EnvVarGuard::set_path(name, &path));
    }
    Ok(guards)
}

async fn subscribe_to_worker(
    client: &async_nats::Client,
    worker_id: &str,
) -> Result<async_nats::Subscriber> {
    let subscriber = client
        .subscribe(targeted_worker_ready_subject(LocalWorkerTarget::new(
            "__local__",
            worker_id,
        )?))
        .await?;
    client.flush().await?;
    Ok(subscriber)
}

async fn await_readiness(subscriber: &mut async_nats::Subscriber, label: &str) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, subscriber.next())
        .await?
        .with_context(|| format!("worker {label} readiness subscription closed"))?;
    Ok(())
}

fn spawn_targeted_daemon(
    worker_id: &'static str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
) -> tokio::task::JoinHandle<Result<()>> {
    let config = Arc::new(ConfigLock::new(Config::default()));
    tokio::spawn(async move {
        run_worker_daemon(
            config,
            WorkerDaemonConfig::local(worker_id)?,
            Some(call_fn),
            None,
        )
        .await
    })
}

fn gated_call_fn(
    counter: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let held_once = Arc::new(AtomicBool::new(false));
    Arc::new(move |input, _config, _abort| {
        let (counter, entered, release) = (
            Arc::clone(&counter),
            Arc::clone(&entered),
            Arc::clone(&release),
        );
        let should_hold = input.text().contains("hold") && !held_once.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            if should_hold {
                entered.notify_one();
                release.notified().await;
            }
            Ok((
                "done".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn wait_for_user_count(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
    expected: usize,
) -> Result<()> {
    let log = NatsSessionLog::new_with_replicas(
        jetstream.clone(),
        harnx_core::session_identity::session_key(None, session_id),
        1,
    );
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if user_message_texts(&log.load_events_async().await?).len() == expected {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn targeted_consumer(
    stream: &async_nats::jetstream::stream::Stream,
    worker_id: &str,
) -> Result<async_nats::jetstream::consumer::PullConsumer> {
    stream
        .get_consumer(&targeted_consumer_name(worker_id)?)
        .await
        .map_err(|error| anyhow::anyhow!("get targeted consumer: {error}"))
}

async fn wait_for_consumer_idle(
    stream: &async_nats::jetstream::stream::Stream,
    worker_id: &str,
) -> Result<()> {
    let consumer = targeted_consumer(stream, worker_id).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let info = consumer.get_info().await?;
            if info.num_pending == 0 && info.num_ack_pending == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

/// Wait until the targeted worker's consumer has dispatched `minimum`
/// deliveries.
async fn wait_for_targeted_delivery(
    jetstream: &async_nats::jetstream::Context,
    worker_id: &str,
    minimum: u64,
) -> Result<()> {
    let stream = jetstream.get_stream("LOCAL_WORK_NOTIFY_V2").await?;
    let consumer = targeted_consumer(&stream, worker_id).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.delivered.consumer_sequence >= minimum {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targeted_local_workers_preserve_frontend_affinity_and_lease_handoff() -> Result<()> {
    let Some(fixture) = TargetedFixture::start().await? else {
        return Ok(());
    };
    fixture.assert_topology_and_poison_termination().await?;
    fixture.assert_idle_target_affinity().await?;
    fixture.assert_concurrent_idle_submissions_fold().await?;
    fixture
        .assert_retained_wakeup_survives_foreign_holder()
        .await?;
    fixture.assert_lease_holder_consumes_injection().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targeted_local_transport_rejects_an_incompatible_existing_stream() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: "LOCAL_WORK_NOTIFY_V2".to_string(),
            subjects: vec!["wrong.>".to_string()],
            retention: async_nats::jetstream::stream::RetentionPolicy::Limits,
            storage: async_nats::jetstream::stream::StorageType::Memory,
            ..Default::default()
        })
        .await?;
    let activation = SessionActivate::targeted("s1", 1, "local-test");
    let error = publish_targeted_session_activate(
        &jetstream,
        LocalWorkerTarget::new("__local__", "local-test")?,
        &activation,
    )
    .await
    .expect_err("incompatible local-v2 stream must fail clearly");
    assert!(
        error.to_string().contains("incompatible subjects"),
        "unexpected topology error: {error:#}"
    );
    Ok(())
}

/// A targeted worker with fault seams and a busy-waiter probe wired in, so
/// the two targeted error-path regressions below can inject failures
/// deterministically and assert nothing parks or launches behind the
/// settlement.
struct TargetedFaultFixture {
    _root: tempfile::TempDir,
    _guards: Vec<EnvVarGuard>,
    _server: harnx_runtime::nats_local_server::SharedNatsServer,
    jetstream: async_nats::jetstream::Context,
    calls: Arc<AtomicUsize>,
    probe: Arc<BusyWaiterProbe>,
    faults: Arc<ActivationFaults>,
    worker: tokio::task::JoinHandle<Result<()>>,
}

impl TargetedFaultFixture {
    /// Spawns the worker, or returns `None` when no nats-server is available.
    async fn start(worker_id: &'static str) -> Result<Option<Self>> {
        require_nextest();
        if !nats_server_available() {
            eprintln!("Skipping test: nats-server not available");
            return Ok(None);
        }
        let root = tempfile::tempdir()?;
        let mut guards = isolated_local_environment(root.path())?;
        let server = harnx_runtime::nats_local_server::ensure_shared_server().await?;
        guards.push(EnvVarGuard::set_value("HARNX_NATS_URL", &server.url));
        guards.push(EnvVarGuard::set_value("HARNX_NATS_TOKEN", &server.token));
        let client = async_nats::ConnectOptions::new()
            .token(server.token.clone())
            .connect(&server.url)
            .await?;
        let jetstream = async_nats::jetstream::new(client.clone());
        let mut ready = subscribe_to_worker(&client, worker_id).await?;
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = Arc::new(BusyWaiterProbe::default());
        let faults = Arc::new(ActivationFaults::default());
        let call_fn = counting_stub_call_fn(Arc::clone(&calls));
        let daemon = WorkerDaemonConfig::local(worker_id)?
            .with_activation_faults_for_test(Arc::clone(&faults))
            .with_busy_waiter_probe_for_test(Arc::clone(&probe));
        let config = Arc::new(ConfigLock::new(Config::default()));
        let worker =
            tokio::spawn(
                async move { run_worker_daemon(config, daemon, Some(call_fn), None).await },
            );
        await_readiness(&mut ready, worker_id).await?;
        Ok(Some(Self {
            _root: root,
            _guards: guards,
            _server: server,
            jetstream,
            calls,
            probe,
            faults,
            worker,
        }))
    }

    async fn seed_prompt(&self, session_id: &str, text: &str) -> Result<u64> {
        use harnx_runtime::nats_session_metadata::{
            SessionInitializer, SessionMetadata, SessionMetadataStore, SessionOverrides,
        };
        let store = SessionMetadataStore::ensure(&self.jetstream, 1).await?;
        store
            .create(&SessionMetadata::new(
                session_id,
                SessionInitializer::inline("", Default::default(), SessionOverrides::default()),
            ))
            .await?;
        let log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(None, session_id),
            1,
        );
        crate::worker::append_admitted_fixture_user(&log, "fault-path-user", text).await
    }

    async fn publish(&self, session_id: &str, requested_seq: u64) -> Result<()> {
        let worker_id = "targeted-fault-worker";
        publish_targeted_session_activate(
            &self.jetstream,
            LocalWorkerTarget::new("__local__", worker_id)?,
            &SessionActivate::targeted(
                harnx_core::session_identity::session_key(None, session_id),
                requested_seq,
                worker_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn redeliveries(&self) -> Result<usize> {
        let stream = self.jetstream.get_stream("LOCAL_WORK_NOTIFY_V2").await?;
        let consumer = targeted_consumer(&stream, "targeted-fault-worker").await?;
        Ok(consumer.get_info().await?.num_redelivered as usize)
    }
}

/// A targeted lease-holder preflight error must settle the delivery exactly
/// once: NAK it back and end the claim. Continuing the claim after that NAK
/// would launch a turn on a delivery the broker already owns, and parking it
/// would consume waiter capacity on a delivery nothing will settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targeted_lease_holder_read_error_settles_without_parking_or_launching() -> Result<()> {
    let Some(fixture) = TargetedFaultFixture::start("targeted-fault-worker").await? else {
        return Ok(());
    };
    fixture
        .faults
        .fail_targeted_lease_holder_read
        .store(true, Ordering::SeqCst);
    let session_id = "targeted-holder-read-error";
    let prompt = fixture
        .seed_prompt(session_id, "run after read error")
        .await?;
    fixture.publish(session_id, prompt).await?;
    let before_redeliveries = fixture.redeliveries().await?;
    // The NAK is delayed (100ms doubling to 2s); give it at least two cycles
    // so a claim that wrongly continued would have launched by now.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "a settled delivery must not launch a turn"
    );
    assert_eq!(
        fixture.probe.parked.load(Ordering::SeqCst),
        0,
        "a settled delivery must not park a busy waiter"
    );
    assert_eq!(
        fixture.probe.slots.load(Ordering::SeqCst),
        0,
        "a settled delivery must not leave a waiter slot"
    );
    assert!(
        fixture.redeliveries().await? > before_redeliveries,
        "the settled delivery must come back through the broker"
    );
    // Recovering the fault lets the next redelivery claim cleanly: exactly
    // one turn ever runs for this session.
    fixture
        .faults
        .fail_targeted_lease_holder_read
        .store(false, Ordering::SeqCst);
    wait_until(CI_SAFE_TIMEOUT, || {
        fixture.calls.load(Ordering::SeqCst) >= 1
    })
    .await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    fixture.worker.abort();
    let _ = fixture.worker.await;
    Ok(())
}

/// A targeted lease-acquisition error (after the preflights passed) must
/// settle the delivery exactly once: NAK it back and end the claim. Treating
/// that NAKed delivery as merely busy would park it while JetStream already
/// owns it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targeted_lease_acquire_error_settles_without_parking_or_launching() -> Result<()> {
    let Some(fixture) = TargetedFaultFixture::start("targeted-fault-worker").await? else {
        return Ok(());
    };
    fixture
        .faults
        .fail_targeted_lease_acquire
        .store(true, Ordering::SeqCst);
    let session_id = "targeted-acquire-error";
    let prompt = fixture
        .seed_prompt(session_id, "run after acquire error")
        .await?;
    fixture.publish(session_id, prompt).await?;
    let before_redeliveries = fixture.redeliveries().await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "a settled delivery must not launch a turn"
    );
    assert_eq!(
        fixture.probe.parked.load(Ordering::SeqCst),
        0,
        "a NAKed delivery must not park a busy waiter"
    );
    assert_eq!(
        fixture.probe.slots.load(Ordering::SeqCst),
        0,
        "a NAKed delivery must not leave a waiter slot"
    );
    assert!(
        fixture.redeliveries().await? > before_redeliveries,
        "the settled delivery must come back through the broker"
    );
    fixture
        .faults
        .fail_targeted_lease_acquire
        .store(false, Ordering::SeqCst);
    wait_until(CI_SAFE_TIMEOUT, || {
        fixture.calls.load(Ordering::SeqCst) >= 1
    })
    .await?;
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    fixture.worker.abort();
    let _ = fixture.worker.await;
    Ok(())
}
