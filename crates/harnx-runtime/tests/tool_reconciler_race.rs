//! Timing-sensitive `ServerReconciler` behavior: a session that arrives
//! mid-teardown of the name it wants, a session ending while its own start is
//! still in flight, and starting several servers concurrently rather than
//! sequentially. See `tool_reconciler.rs` for the plain refcount/linger rules
//! and `tool_reconciler_e2e.rs` for the real-worker end-to-end coverage.

use async_trait::async_trait;
use harnx_runtime::config::ToolServerConfig;
use harnx_runtime::nats_worker::server_reconciler::{ServerLauncher, ServerReconciler};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn tool_server(name: &str) -> ToolServerConfig {
    // `ToolServerConfig` does not derive `Default`, so every field is
    // explicit. Same shape as `fn tool_server` in `tool_reconciler.rs` and
    // the `daemon.rs` test module — keep them in sync.
    ToolServerConfig {
        name: name.to_string(),
        command: format!("harnx-{name}-server"),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    }
}

/// Which call [`GatedLauncher`] blocks on: different tests need to hold open
/// different windows (a teardown in flight vs. a start in flight).
#[derive(Clone, Copy)]
enum GatedCall {
    Start,
    Stop,
}

/// A launcher whose `start` or `stop` (per `gate`) blocks on a gate the test
/// controls, so a test can hold that call open for exactly as long as it
/// needs and no longer — no sleeps, no timing guesses. `entered` fires the
/// moment the gated call is entered (so the test knows it is safe to act
/// "during" it), and the call itself doesn't return until the test sends on
/// `release`.
struct GatedLauncher {
    gate: GatedCall,
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl GatedLauncher {
    fn new(
        gate: GatedCall,
    ) -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let launcher = Arc::new(Self {
            gate,
            started: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        (launcher, entered_rx, release_tx)
    }

    /// Signal `entered`, then block until the test sends on `release`.
    async fn hold_gate(&self) {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.await;
        }
    }
}

#[async_trait]
impl ServerLauncher for GatedLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started.lock().unwrap().push(server.name.clone());
        if matches!(self.gate, GatedCall::Start) {
            self.hold_gate().await;
        }
        Ok(())
    }
    async fn stop(&self, config_name: &str) {
        self.stopped.lock().unwrap().push(config_name.to_string());
        if matches!(self.gate, GatedCall::Stop) {
            self.hold_gate().await;
        }
    }
}

/// A session that asks for a server while `sweep` is mid-teardown of that
/// same name must end up with a running, registered server of its own —
/// not silently joined to an entry that teardown is about to delete out
/// from under it (see `server_reconciler.rs` module docs on `Slot`).
///
/// The whole scenario is driven by two oneshot gates rather than any sleep:
/// `entered_stop` proves the test has actually reached the window between
/// "teardown begun" and "entry removed" before it acts, and `release_stop`
/// lets teardown finish only once the racing session has had its chance to
/// observe that window.
#[tokio::test]
async fn a_session_that_arrives_mid_teardown_gets_a_fresh_running_server() {
    harnx_core::require_nextest();
    let (launcher, entered_stop, release_stop) = GatedLauncher::new(GatedCall::Stop);
    let reconciler = Arc::new(ServerReconciler::new(launcher.clone(), Duration::ZERO));

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;

    // `linger` is zero, so dropping s1's only use of "time" sweeps
    // immediately and calls `stop`, which the fake now blocks inside until
    // `release_stop` is sent below. Run it on its own task since it won't
    // return until then.
    let ending = tokio::spawn({
        let reconciler = reconciler.clone();
        async move { reconciler.session_ended("s1").await }
    });

    entered_stop
        .await
        .expect("session_ended's sweep should have called stop on 'time'");

    // s2 asks for the same name while that teardown is still in flight.
    let joining = tokio::spawn({
        let reconciler = reconciler.clone();
        async move {
            reconciler
                .session_started("s2", vec![tool_server("time")])
                .await;
        }
    });

    release_stop
        .send(())
        .expect("stop is still parked on this gate waiting to be released");

    ending.await.expect("session_ended task panicked");
    joining.await.expect("session_started task panicked");

    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "a session that raced a teardown for the same name must end up with \
         a running, registered server, not silently none"
    );
    assert_eq!(
        launcher.started.lock().unwrap().as_slice(),
        ["time", "time"],
        "the racing session must trigger its own fresh start once the old \
         process is actually gone, not just join an entry that sweep is \
         about to delete out from under it"
    );
    assert_eq!(launcher.stopped.lock().unwrap().as_slice(), ["time"]);
}

/// Regression for a server left permanently pinned as "running" for a session
/// that already ended: `claim_users` (registration) must complete, and be
/// visible to `session_ended`, before the matching `start_claimed` (the slow
/// part — spawning the process and waiting for it to register) ever runs. A
/// caller that instead bundles both into one background task and abandons it
/// on a timeout can lose this race: `session_ended` runs first, finds
/// nothing to release, and the registration that lands afterward pins the
/// server as a user-less "running" entry forever (see
/// `WorkerRuntime::start_session_tool_servers`, which used to do exactly
/// that).
#[tokio::test]
async fn claim_users_registers_before_the_matching_start_completes() {
    harnx_core::require_nextest();
    let (launcher, entered_start, release_start) = GatedLauncher::new(GatedCall::Start);
    let reconciler = Arc::new(ServerReconciler::new(launcher.clone(), Duration::ZERO));

    let to_start = reconciler
        .claim_users("s1", vec![tool_server("time")])
        .await;
    assert_eq!(
        to_start
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        ["time"],
        "the first user of a fresh name must be told to start it"
    );
    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "claim_users must register the entry before start_claimed ever runs"
    );

    let starting = tokio::spawn({
        let reconciler = reconciler.clone();
        async move { reconciler.start_claimed(to_start).await }
    });
    entered_start
        .await
        .expect("start_claimed should have called launcher.start");

    // The session ends while its own server start is still in flight. If
    // registration had instead been deferred into the same background task
    // as the start, there would be nothing here for `session_ended` to
    // release, and "time" would stay pinned with no real user.
    reconciler.session_ended("s1").await;

    release_start
        .send(())
        .expect("start is still parked on this gate waiting to be released");
    starting.await.expect("start_claimed task panicked");

    // Zero linger: session_ended's sweep should already have torn "time"
    // down once its user count hit zero.
    assert!(
        reconciler.running().await.is_empty(),
        "the server must not stay pinned as running with no real user"
    );
    assert_eq!(launcher.stopped.lock().unwrap().as_slice(), ["time"]);
}

/// A launcher whose `start` takes a fixed amount of time, for pinning
/// `session_started`'s concurrency: several of these in one call should take
/// about as long as the slowest one, not their sum.
#[derive(Default)]
struct SlowLauncher {
    delay: Duration,
    started: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for SlowLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        tokio::time::sleep(self.delay).await;
        self.started.lock().unwrap().push(server.name.clone());
        Ok(())
    }
    async fn stop(&self, _config_name: &str) {}
}

#[tokio::test]
async fn session_started_starts_multiple_servers_concurrently_not_sequentially() {
    harnx_core::require_nextest();
    let launcher = Arc::new(SlowLauncher {
        delay: Duration::from_millis(200),
        started: Mutex::new(Vec::new()),
    });
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    let began = Instant::now();
    reconciler
        .session_started(
            "s1",
            vec![
                tool_server("time"),
                tool_server("plans"),
                tool_server("weather"),
            ],
        )
        .await;
    let elapsed = began.elapsed();

    assert_eq!(launcher.started.lock().unwrap().len(), 3);
    assert!(
        elapsed < Duration::from_millis(450),
        "three 200ms server starts should overlap (~200ms total) rather than sum \
         sequentially (~600ms); the activation ack window can't afford the sum once \
         more than a couple of servers are configured. took {elapsed:?}"
    );
}
