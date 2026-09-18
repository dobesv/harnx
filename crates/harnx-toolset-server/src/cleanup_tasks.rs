//! Server-lifetime ownership of tool handler tasks.
use std::{future::Future, sync::Mutex};
use tokio::task::JoinSet;

/// The task that owns a tool handler outlives the reply it produced, so it
/// cannot live inside the request future. Handles are retained here — including
/// stubborn cooperative handlers — rather than detached, so a caller that walks
/// away never abandons work that is still running. Process shutdown aborts
/// these tasks, but cannot prove external work stopped.
#[derive(Default)]
pub(super) struct CleanupTasks(Mutex<JoinSet<()>>);

impl CleanupTasks {
    pub(super) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.0.lock().unwrap_or_else(|error| error.into_inner());
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                log::warn!("tool handler task failed; its shutdown is unconfirmed: {error}");
            }
        }
        tasks.spawn(future);
    }
}
