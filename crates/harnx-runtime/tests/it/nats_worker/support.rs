use super::*;
use std::future::Future;
use tokio_util::task::AbortOnDropHandle;

pub(super) type WorkerDaemonHandle = AbortOnDropHandle<Result<()>>;

pub(super) async fn progress_or_daemon_exit<F, T>(
    label: &str,
    progress: F,
    worker_one: &mut WorkerDaemonHandle,
    worker_two: &mut WorkerDaemonHandle,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = progress => result,
        stopped = &mut *worker_one => {
            anyhow::bail!("worker-one daemon stopped during {label}: {stopped:?}")
        }
        stopped = &mut *worker_two => {
            anyhow::bail!("worker-two daemon stopped during {label}: {stopped:?}")
        }
    }
}

pub(super) fn spawn_test_worker(
    server_url: &str,
    worker_id: &str,
    lease: &NatsLeaseConfig,
    counter: Arc<AtomicUsize>,
) -> (WorkerDaemonHandle, harnx_healthz::Readiness) {
    let mut daemon = WorkerDaemonConfig::managing("local", worker_id);
    daemon.lease = lease.clone();
    let config = local_nats_runtime_config(server_url);
    let readiness = harnx_healthz::Readiness::default();
    let handle = AbortOnDropHandle::new(tokio::spawn({
        let readiness = readiness.clone();
        async move {
            run_worker_daemon(
                config,
                daemon,
                Some(counting_stub_call_fn(counter)),
                Some(readiness),
            )
            .await
        }
    }));
    (handle, readiness)
}

pub(super) struct WorkerPair {
    pub(super) worker_one: WorkerDaemonHandle,
    pub(super) worker_two: WorkerDaemonHandle,
    readiness_one: harnx_healthz::Readiness,
    readiness_two: harnx_healthz::Readiness,
    counter_one: Arc<AtomicUsize>,
    counter_two: Arc<AtomicUsize>,
}

impl WorkerPair {
    pub(super) fn spawn(server_url: &str, lease: &NatsLeaseConfig) -> Self {
        let counter_one = Arc::new(AtomicUsize::new(0));
        let counter_two = Arc::new(AtomicUsize::new(0));
        let (worker_one, readiness_one) =
            spawn_test_worker(server_url, "worker-one", lease, Arc::clone(&counter_one));
        let (worker_two, readiness_two) =
            spawn_test_worker(server_url, "worker-two", lease, Arc::clone(&counter_two));
        Self {
            worker_one,
            worker_two,
            readiness_one,
            readiness_two,
            counter_one,
            counter_two,
        }
    }

    pub(super) async fn wait_until_ready(&mut self) -> Result<()> {
        let readiness_one = self.readiness_one.clone();
        let readiness_two = self.readiness_two.clone();
        progress_or_daemon_exit(
            "startup",
            wait_until(CI_SAFE_TIMEOUT, move || {
                readiness_one.is_ready() && readiness_two.is_ready()
            }),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    pub(super) async fn wait_for_execution_count(
        &mut self,
        label: &str,
        expected: usize,
    ) -> Result<()> {
        let counter_one = Arc::clone(&self.counter_one);
        let counter_two = Arc::clone(&self.counter_two);
        progress_or_daemon_exit(
            label,
            wait_until(CI_SAFE_TIMEOUT, move || {
                counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst) >= expected
            }),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    pub(super) async fn wait_for_session_cleanup(
        &mut self,
        label: &str,
        jetstream: &async_nats::jetstream::Context,
        session_id: &str,
    ) -> Result<()> {
        progress_or_daemon_exit(
            label,
            wait_for_worker_session_cleanup(jetstream, session_id),
            &mut self.worker_one,
            &mut self.worker_two,
        )
        .await
    }

    pub(super) fn execution_count(&self) -> usize {
        self.counter_one.load(Ordering::SeqCst) + self.counter_two.load(Ordering::SeqCst)
    }

    pub(super) fn assert_running(&self) {
        assert!(
            !self.worker_one.is_finished(),
            "worker-one daemon stopped unexpectedly"
        );
        assert!(
            !self.worker_two.is_finished(),
            "worker-two daemon stopped unexpectedly"
        );
    }

    pub(super) async fn abort_and_assert_cancelled(self) {
        self.worker_one.abort();
        self.worker_two.abort();
        let stopped_one = self.worker_one.await;
        let stopped_two = self.worker_two.await;
        assert!(
            matches!(&stopped_one, Err(error) if error.is_cancelled()),
            "worker-one daemon did not stop by cancellation: {stopped_one:?}"
        );
        assert!(
            matches!(&stopped_two, Err(error) if error.is_cancelled()),
            "worker-two daemon did not stop by cancellation: {stopped_two:?}"
        );
    }
}
