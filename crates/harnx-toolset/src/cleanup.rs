//! Per-call cleanup reporting for adapters that cannot confirm remote shutdown.
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct InvocationCleanup(Arc<Mutex<Option<String>>>);

tokio::task_local! {
    pub static INVOCATION_CLEANUP: InvocationCleanup;
}

impl InvocationCleanup {
    pub fn last_error(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Ending a bridge waiter is not evidence that the remote request stopped.
/// Report this inside the invocation before returning from best-effort cleanup.
pub fn unconfirmed(reason: impl Into<String>) {
    let _ = INVOCATION_CLEANUP.try_with(|cleanup| {
        *cleanup.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(reason.into());
    });
}
