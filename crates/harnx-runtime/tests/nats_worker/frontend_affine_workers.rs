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
                session_id,
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
        let turn_a =
            tokio::spawn(
                async move { session_a.run_turn("alpha", Arc::new(NullSink), None).await },
            );
        let turn_b =
            tokio::spawn(async move { session_b.run_turn("beta", Arc::new(NullSink), None).await });
        wait_for_user_count(&self.jetstream, session_id, 2).await?;
        let before = self.total_calls();
        blocker.release().await?;
        let result_a = tokio::time::timeout(CI_SAFE_TIMEOUT, turn_a).await???;
        let result_b = tokio::time::timeout(CI_SAFE_TIMEOUT, turn_b).await???;
        assert!(result_a.error.is_none() && result_b.error.is_none());
        assert_eq!(self.total_calls() - before, 1);
        Ok(())
    }

    async fn assert_retained_wakeup_survives_foreign_holder(&self) -> Result<()> {
        let session_id = "target-retained-after-lease";
        let _initialized = self.session(session_id, WORKER_B).await?;
        let log = NatsSessionLog::new(self.jetstream.clone(), session_id);
        let requested_seq = log
            .append_event_async(&append_user_message_entry("retained", "run after release"))
            .await?;
        let blocker = self.acquire_blocker(session_id, "other-holder").await?;
        publish_targeted_session_activate(
            &self.jetstream,
            LocalWorkerTarget::new("__local__", WORKER_B)?,
            &SessionActivate::targeted(session_id, requested_seq, WORKER_B),
        )
        .await?;
        wait_for_consumer_redelivery(&self.jetstream, WORKER_B).await?;
        let before = self.counter_b.load(Ordering::SeqCst);
        blocker.release().await?;
        wait_until(CI_SAFE_TIMEOUT, || {
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
                .run_turn("hold", Arc::new(NullSink), None)
                .await
        });
        tokio::time::timeout(CI_SAFE_TIMEOUT, self.hold_entered.notified()).await?;
        let injecting_session = self.session(session_id, WORKER_B).await?;
        let injecting_turn = tokio::spawn(async move {
            injecting_session
                .run_turn("injected", Arc::new(NullSink), None)
                .await
        });
        wait_for_user_count(&self.jetstream, session_id, 2).await?;
        self.hold_release.notify_one();
        tokio::time::timeout(CI_SAFE_TIMEOUT, held_turn).await???;
        tokio::time::timeout(CI_SAFE_TIMEOUT, injecting_turn).await???;
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
    let config = Arc::new(RwLock::new(Config::default()));
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
    let log = NatsSessionLog::new(jetstream.clone(), session_id);
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

async fn wait_for_consumer_redelivery(
    jetstream: &async_nats::jetstream::Context,
    worker_id: &str,
) -> Result<()> {
    let stream = jetstream.get_stream("LOCAL_WORK_NOTIFY_V2").await?;
    let consumer = targeted_consumer(&stream, worker_id).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if consumer.get_info().await?.num_redelivered > 0 {
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
