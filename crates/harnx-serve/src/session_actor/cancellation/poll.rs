use super::*;
use futures_util::{future::BoxFuture, FutureExt};

#[cfg(test)]
#[path = "poll_tests.rs"]
mod tests;

/// One owned read task, polled alongside the actor mailbox. Commands invalidate
/// its snapshot without dropping the read: status reconciliation can write to
/// the execution graph, and its older result must not overwrite command state.
/// The task must progress while commands await the shared broker client; a
/// suspended in-loop future could hold a client lock needed by the command.
pub(in crate::session_actor) struct CancellationPoller {
    read: Box<dyn FnMut() -> BoxFuture<'static, RefreshResult> + Send>,
    pending: AbortOnDropHandle<RefreshResult>,
    current: bool,
}

impl CancellationPoller {
    pub(in crate::session_actor) fn new(
        mut read: impl FnMut() -> BoxFuture<'static, RefreshResult> + Send + 'static,
    ) -> Self {
        let pending = AbortOnDropHandle::new(tokio::spawn(bounded_refresh(read())));
        Self {
            read: Box::new(read),
            pending,
            current: true,
        }
    }

    pub(in crate::session_actor) fn invalidate(&mut self) {
        self.current = false;
    }

    pub(in crate::session_actor) async fn next(&mut self) -> Option<RefreshResult> {
        let result = (&mut self.pending)
            .await
            .unwrap_or_else(|error| Err(error.into()));
        let current = std::mem::replace(&mut self.current, true);
        let read = (self.read)();
        self.pending = AbortOnDropHandle::new(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            bounded_refresh(read).await
        }));
        current.then_some(result)
    }
}

pub(in crate::session_actor) fn poller(
    config: SessionActorConfig,
    session_id: String,
) -> CancellationPoller {
    CancellationPoller::new(move || {
        let config = config.clone();
        let session_id = session_id.clone();
        async move {
            if config.call_fn.is_some() {
                std::future::pending().await
            } else {
                read_cancellation(&config.base_config, &session_id).await
            }
        }
        .boxed()
    })
}
