//! Keep broker election running while frontends are idle or awaiting a turn.

use super::{
    acquire_server, connect_authenticated, joiner_is_current, BrokerNoticeSink, SharedNatsServer,
    WaitNotices, WaitPhase,
};
use anyhow::{bail, Context, Result};
use async_nats::Client;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;

const CHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct LocalBrokerStatus {
    pub url: String,
    pub token: String,
    pub nonce: String,
    owner: bool,
}

impl LocalBrokerStatus {
    pub fn is_owner(&self) -> bool {
        self.owner
    }

    fn snapshot(server: &SharedNatsServer) -> Self {
        Self {
            url: server.url.clone(),
            token: server.token.clone(),
            nonce: server.nonce.clone(),
            owner: server.is_owner(),
        }
    }

    fn discovery_is_current(&self) -> bool {
        self.owner || joiner_is_current((&self.url, &self.token, &self.nonce))
    }
}

#[derive(Clone)]
struct ReadyBroker {
    generation: u64,
    endpoint: LocalBrokerStatus,
}

#[derive(Default)]
enum Phase {
    #[default]
    Waiting,
    Ready,
    Failed(Arc<str>),
    Closed,
}

#[derive(Default)]
struct BrokerState {
    phase: Phase,
    last_ready: Option<ReadyBroker>,
}

impl BrokerState {
    fn ready_since(&self, previous: Option<u64>) -> bool {
        match self.phase {
            Phase::Waiting => false,
            Phase::Ready => self
                .last_ready
                .as_ref()
                .is_some_and(|ready| Some(ready.generation) != previous),
            Phase::Closed | Phase::Failed(_) => true,
        }
    }

    fn ready(&self) -> Result<ReadyBroker> {
        match &self.phase {
            Phase::Closed => bail!("shared local NATS is shutting down"),
            Phase::Failed(error) => bail!("{error}"),
            Phase::Waiting | Phase::Ready => self
                .last_ready
                .clone()
                .context("no ready local NATS endpoint"),
        }
    }
}

/// Observer only: it cannot extend the supervisor's lifetime. The last endpoint
/// is useful for diagnostics, but only `connect` returns authenticated readiness.
#[derive(Clone)]
pub(crate) struct BrokerWatch {
    state: watch::Receiver<BrokerState>,
    recheck: watch::Sender<u64>,
    stop: CancellationToken,
}

impl BrokerWatch {
    pub(crate) fn status(&self) -> LocalBrokerStatus {
        self.state
            .borrow()
            .last_ready
            .as_ref()
            .expect("local broker not initialized")
            .endpoint
            .clone()
    }

    pub(crate) async fn connect(mut self) -> Result<(LocalBrokerStatus, Client)> {
        let stop = self.stop.clone();
        tokio::select! {
            biased;
            () = stop.cancelled() => bail!("shared local NATS is shutting down"),
            result = self.connect_current() => result,
        }
    }

    async fn connect_current(&mut self) -> Result<(LocalBrokerStatus, Client)> {
        let mut previous = None;
        loop {
            let ready = self
                .state
                .wait_for(|state| state.ready_since(previous))
                .await
                .context("local NATS supervisor stopped")?
                .ready()?;
            let client = connect_authenticated(&ready.endpoint.url, &ready.endpoint.token).await;
            if let Ok(client) = client {
                if ready.endpoint.discovery_is_current() && self.still_ready(ready.generation) {
                    return Ok((ready.endpoint, client));
                }
            }
            // A connection failure after discovery is also loss of readiness.
            // Ask the same supervisor to recheck, and wait for a newer generation.
            self.recheck.send_if_modified(|failed| {
                if *failed >= ready.generation {
                    return false;
                }
                *failed = ready.generation;
                true
            });
            previous = Some(ready.generation);
        }
    }

    fn still_ready(&self, generation: u64) -> bool {
        let state = self.state.borrow();
        !self.stop.is_cancelled()
            && *self.recheck.borrow() < generation
            && matches!(state.phase, Phase::Ready)
            && state
                .last_ready
                .as_ref()
                .is_some_and(|ready| ready.generation == generation)
    }
}

/// One explicit owner of startup, election, and recovery. Frontend-managed
/// callers share one of these via `LocalBrokerScope`; direct owners may also
/// use it as a guard for the duration of an independent frontend.
pub struct LocalBroker {
    watch: BrokerWatch,
    state: watch::Sender<BrokerState>,
    task: Option<JoinHandle<()>>,
}

impl LocalBroker {
    pub async fn start() -> Result<Self> {
        let broker = Self::launch(CancellationToken::new(), None);
        broker.observer().connect().await?;
        Ok(broker)
    }

    pub(super) fn launch(stop: CancellationToken, report: Option<BrokerNoticeSink>) -> Self {
        let (state, receiver) = watch::channel(BrokerState::default());
        let (recheck, requests) = watch::channel(0);
        let task = tokio::spawn(supervise(state.clone(), requests, stop.clone(), report));
        Self {
            watch: BrokerWatch {
                state: receiver,
                recheck,
                stop,
            },
            state,
            task: Some(task),
        }
    }

    pub(crate) fn observer(&self) -> BrokerWatch {
        self.watch.clone()
    }

    pub fn status(&self) -> LocalBrokerStatus {
        self.watch.status()
    }

    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    /// Stops and joins the producer, including rollback of an owned startup.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.request_stop();
        let result = match &mut self.task {
            Some(task) => task.await.context("join local NATS supervisor"),
            None => Ok(()),
        };
        self.task.take();
        result
    }

    pub(super) fn request_stop(&self) {
        self.state.send_modify(|state| state.phase = Phase::Closed);
        self.watch.stop.cancel();
    }
}

impl Drop for LocalBroker {
    fn drop(&mut self) {
        // Replacing a finished acquisition must leave its failure visible to
        // callers that already subscribed to that attempt.
        if self.is_running() {
            self.request_stop();
        }
    }
}

async fn supervise(
    state: watch::Sender<BrokerState>,
    requests: watch::Receiver<u64>,
    stop: CancellationToken,
    report: Option<BrokerNoticeSink>,
) {
    tokio::select! {
        biased;
        () = stop.cancelled() => {},
        () = run_supervisor(&state, requests, report) => {},
    }
}

async fn run_supervisor(
    state: &watch::Sender<BrokerState>,
    mut requests: watch::Receiver<u64>,
    report: Option<BrokerNoticeSink>,
) {
    let mut generation = 0;
    let mut server = None;
    loop {
        let mut notices = WaitNotices::new(report.clone());
        let current = match server.take() {
            Some(owned) => revalidate_owner(owned, &mut notices).await,
            None => acquire_server(&mut notices).await,
        };
        let mut current = match current {
            Ok(current) => current,
            Err(error) if generation == 0 => {
                publish_phase(state, Phase::Failed(format!("{error:#}").into()));
                return;
            }
            Err(error) => {
                log::warn!("shared local NATS recovery failed; retrying: {error:#}");
                sleep(CHECK_INTERVAL).await;
                continue;
            }
        };
        generation += 1;
        if !publish_ready(state, &current, generation) {
            return;
        }
        let retain = observe_ready(&mut requests, &mut current, generation).await;
        publish_phase(state, Phase::Waiting);
        server = retain.then_some(current);
    }
}

fn publish_phase(state: &watch::Sender<BrokerState>, phase: Phase) {
    state.send_if_modified(|current| {
        if matches!(current.phase, Phase::Closed) {
            return false;
        }
        current.phase = phase;
        true
    });
}

fn publish_ready(
    state: &watch::Sender<BrokerState>,
    server: &SharedNatsServer,
    generation: u64,
) -> bool {
    state.send_if_modified(|state| {
        if matches!(state.phase, Phase::Closed) {
            return false;
        }
        state.phase = Phase::Ready;
        state.last_ready = Some(ReadyBroker {
            generation,
            endpoint: LocalBrokerStatus::snapshot(server),
        });
        true
    })
}

// `true` means the child's not known to have exited; keep its lock and PID
// and repeat readiness. A stale joiner always goes back through election.
async fn observe_ready(
    requests: &mut watch::Receiver<u64>,
    server: &mut SharedNatsServer,
    generation: u64,
) -> bool {
    loop {
        tokio::select! {
            () = sleep(CHECK_INTERVAL) => {
                if !server.owner_or_file_is_current() {
                    return false;
                }
            }
            _ = requests.wait_for(|failed| *failed >= generation) => {
                return server.is_owner() && server.owner_or_file_is_current();
            }
        }
    }
}

async fn revalidate_owner(
    mut server: SharedNatsServer,
    notices: &mut WaitNotices,
) -> Result<SharedNatsServer> {
    while server.owner_or_file_is_current() {
        if connect_authenticated(&server.url, &server.token)
            .await
            .is_ok()
        {
            return Ok(server);
        }
        if let Some(pid) = server.server_process_id() {
            notices.waiting(WaitPhase::Readiness(pid));
        }
        sleep(super::RETRY_DELAY).await;
    }
    drop(server);
    acquire_server(notices).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined_server() -> SharedNatsServer {
        SharedNatsServer {
            url: "nats://127.0.0.1:4222".into(),
            token: "unused".into(),
            nonce: "unchanged".into(),
            owner: None,
        }
    }

    #[test]
    fn nats_readiness_invalidates_old_generation_even_when_metadata_is_unchanged() {
        let (state, receiver) = watch::channel(BrokerState::default());
        let (recheck, _) = watch::channel(0);
        let observer = BrokerWatch {
            state: receiver,
            recheck,
            stop: CancellationToken::new(),
        };
        let joined = joined_server();
        assert!(publish_ready(&state, &joined, 1));
        assert!(observer.still_ready(1));
        publish_phase(&state, Phase::Waiting);
        assert!(!observer.still_ready(1));
        assert!(publish_ready(&state, &joined, 2));
        assert!(!observer.still_ready(1));
        assert!(observer.still_ready(2));
        publish_phase(&state, Phase::Closed);
        assert!(!publish_ready(&state, &joined, 3));
        assert!(!observer.still_ready(2));
    }
}
