//! Process-lifetime ownership of asynchronous resource cleanup.
use std::{future::Future, sync::Mutex};
use tokio::task::JoinSet;

/// Resource owners keep this outside request/turn futures. Handles are retained,
/// including stubborn cooperative tasks; dropping a reply never detaches them.
/// Process shutdown aborts async tasks, but cannot prove external work stopped.
#[derive(Default)]
pub struct CleanupTasks(Mutex<JoinSet<()>>);

impl CleanupTasks {
    /// Process-owned adapters have no turn lifetime. The durable reconciler is
    /// separately restarted by each worker; these handles are only local evidence.
    pub fn process() -> &'static Self {
        static TASKS: std::sync::OnceLock<CleanupTasks> = std::sync::OnceLock::new();
        TASKS.get_or_init(Self::default)
    }

    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.0.lock().unwrap_or_else(|error| error.into_inner());
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                log::warn!("cleanup task failed; physical completion is unconfirmed: {error}");
            }
        }
        tasks.spawn(future);
    }

    /// Test/diagnostic count, including completed handles not yet reaped.
    pub fn len(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
