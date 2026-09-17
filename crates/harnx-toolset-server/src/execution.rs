//! The owner task outlives its reply. Acceptance never waits for handler cleanup.
use anyhow::{ensure, Result};
use harnx_toolset::{CancellationGuarantee, InterruptedCall, ToolInvokeError, ToolRequest};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::sync::oneshot;

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
use tokio_util::sync::CancellationToken;

const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

/// Handle on a call this process is still running, so a control message can
/// stop it and tell the interrupted reply which cancellation caused the stop.
#[derive(Clone)]
pub(super) struct ActiveCall {
    pub session_id: String,
    pub cancel: CancellationToken,
    cancellation_id: Arc<Mutex<Option<String>>>,
}

impl ActiveCall {
    /// Name the cancellation that stopped this call. The first one wins: a
    /// retry of the same stop must not relabel an interruption already decided.
    pub fn set_cancellation_id(&self, id: &str) {
        let mut slot = cancellation_id(&self.cancellation_id);
        slot.get_or_insert_with(|| id.to_owned());
    }

    /// Both handles describe the same call when they share its cancellation
    /// slot, which is allocated once per invocation.
    pub fn is_same_call(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancellation_id, &other.cancellation_id)
    }
}

/// One server-owned tool invocation: the identity it answers under, and the
/// cancellation slot it shares with the [`ActiveCall`] handle control uses.
pub(super) struct InvocationExecution {
    pub session_id: String,
    pub call_id: String,
    cancellation_id: Arc<Mutex<Option<String>>>,
}

impl InvocationExecution {
    pub fn claim(request: &ToolRequest) -> Result<Self> {
        ensure!(
            request.operation_id == request.call_id,
            "operation ID must match attested call ID"
        );
        Ok(Self {
            session_id: request
                .parent_session_id
                .clone()
                .unwrap_or_else(|| request.call_id.clone()),
            call_id: request.call_id.clone(),
            cancellation_id: Arc::default(),
        })
    }

    /// The handle control messages cancel this invocation through.
    pub fn active_call(&self, cancel: CancellationToken) -> ActiveCall {
        ActiveCall {
            session_id: self.session_id.clone(),
            cancel,
            cancellation_id: self.cancellation_id.clone(),
        }
    }

    fn interruption(&self) -> Result<Value, ToolInvokeError> {
        Err(ToolInvokeError::Interrupted(InterruptedCall {
            cancellation_id: cancellation_id(&self.cancellation_id).take(),
            reason: "tool invocation cancelled".into(),
        }))
    }

    /// Whether a control message named a cancellation for this call. Control
    /// names the cancellation before it answers its sender, so a named slot
    /// means the call was already decided and told to somebody.
    fn cancellation_accepted(&self) -> bool {
        cancellation_id(&self.cancellation_id).is_some()
    }

    /// Called only in the server-lifetime CleanupTasks set. It owns the future,
    /// while the request path owns only a oneshot receiver.
    pub async fn invoke(
        self,
        cancel: CancellationToken,
        guarantee: CancellationGuarantee,
        future: impl Future<Output = Result<Value, ToolInvokeError>>,
        reply: oneshot::Sender<Result<Value, ToolInvokeError>>,
    ) {
        let cleanup = harnx_toolset::cleanup::InvocationCleanup::default();
        let mut future =
            Box::pin(harnx_toolset::cleanup::INVOCATION_CLEANUP.scope(cleanup.clone(), future));
        let mut started = false;
        let handled = {
            let mut observed = Box::pin(std::future::poll_fn(|cx| {
                started = true;
                future.as_mut().poll(cx)
            }));
            run(&cancel, observed.as_mut()).await
        };
        let finished = handled.is_some();
        // An accepted cancellation decides the call at the moment control
        // accepts it, not at the poll that notices the token: `run` reads the
        // token once per poll and then hands the same poll to the handler, so
        // a cancel landing in between leaves a ready handler free to win a
        // race whose answer has already been published.
        let result = match handled {
            Some(result) if !self.cancellation_accepted() => result,
            _ => self.interruption(),
        };
        let _ = reply.send(result);
        // Retain work already started, but never start an unpolled handler
        // merely to clean it up after acceptance.
        if !finished && started && guarantee == CancellationGuarantee::Cooperative {
            drain_handler(future.as_mut()).await;
        }
        drop(future);
        if let Some(reason) = cleanup.last_error() {
            log::warn!(
                "tool handler for call '{}' did not confirm cleanup: {reason}",
                self.call_id
            );
        }
    }
}

async fn run<F>(cancel: &CancellationToken, mut future: Pin<&mut F>) -> Option<F::Output>
where
    F: Future<Output = Result<Value, ToolInvokeError>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = &mut future => Some(result),
    }
}

async fn drain_handler<F>(mut future: Pin<&mut F>)
where
    F: Future<Output = Result<Value, ToolInvokeError>>,
{
    if tokio::time::timeout(CLEANUP_BUDGET, &mut future)
        .await
        .is_err()
    {
        log::warn!("tool handler has not stopped within the cleanup budget; still retaining it");
        // Retain and poll a cooperative future, even forever. Dropping a
        // JoinHandle would detach it; aborting spawn_blocking cannot stop it.
        let _ = future.await;
    }
}

fn cancellation_id(slot: &Mutex<Option<String>>) -> std::sync::MutexGuard<'_, Option<String>> {
    slot.lock().unwrap_or_else(|error| error.into_inner())
}
