use super::SandboxManager;
use crate::kubernetes::kubernetes_failure;
use crate::policy::{
    operation_metric, retry_metric, EndReason, FailureKind, TerminalClass, TerminalError,
};
use anyhow::Result;
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub(super) struct WaitContext<'a> {
    pub operation: &'static str,
    pub cancel: &'a CancellationToken,
    pub deadline: tokio::time::Instant,
}

impl<'a> WaitContext<'a> {
    pub fn new(
        operation: &'static str,
        cancel: &'a CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Self {
        Self {
            operation,
            cancel,
            deadline,
        }
    }

    pub fn unbounded(operation: &'static str, cancel: &'a CancellationToken) -> Self {
        Self::new(operation, cancel, far_future())
    }

    pub fn with_operation(self, operation: &'static str) -> Self {
        Self { operation, ..self }
    }
}

impl SandboxManager {
    pub(super) async fn wait(&self, context: &WaitContext<'_>, duration: Duration) -> Result<()> {
        bounded(context, tokio::time::sleep(duration)).await
    }

    pub(super) async fn retry_api<T, F, Fut>(
        &self,
        context: &WaitContext<'_>,
        mut make_request: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut attempt = 1;
        loop {
            let result = self.cancellable_api(context, make_request()).await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let (kind, retry_after) = kubernetes_failure(&error);
                    if !retryable(kind) {
                        operation_metric("k8s", context.operation, "permanent_error");
                        return Err(terminal_error(
                            TerminalClass::new(EndReason::Failed, kind),
                            context.operation,
                            attempt,
                            error,
                        ));
                    }
                    if attempt >= self.backoff.max_attempts {
                        operation_metric("k8s", context.operation, "retry_exhausted");
                        return Err(terminal_error(
                            TerminalClass::new(EndReason::AttemptsExhausted, kind),
                            context.operation,
                            attempt,
                            error,
                        ));
                    }
                    let delay = self.retry_delay(context, attempt, retry_after);
                    self.wait(context, delay).await?;
                    retry_metric("k8s", context.operation, failure_reason(kind));
                    attempt += 1;
                }
            }
        }
    }

    pub(super) async fn cancellable_api<T>(
        &self,
        context: &WaitContext<'_>,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        bounded(context, future).await?
    }

    fn retry_delay(
        &self,
        context: &WaitContext<'_>,
        attempt: usize,
        retry_after: Option<Duration>,
    ) -> Duration {
        let remaining = context
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        self.backoff
            .delay(attempt - 1, remaining)
            .max(retry_after.unwrap_or_default())
            .min(remaining)
    }
}

async fn bounded<T>(context: &WaitContext<'_>, future: impl Future<Output = T>) -> Result<T> {
    check_wait(context)?;
    tokio::select! {
        biased;
        _ = context.cancel.cancelled() => Err(triggered_error(context.operation, EndReason::Cancelled)),
        _ = tokio::time::sleep_until(context.deadline) => Err(triggered_error(context.operation, EndReason::DeadlineExceeded)),
        result = future => Ok(result),
    }
}

fn check_wait(context: &WaitContext<'_>) -> Result<()> {
    if context.cancel.is_cancelled() {
        return Err(triggered_error(context.operation, EndReason::Cancelled));
    }
    if tokio::time::Instant::now() >= context.deadline {
        return Err(triggered_error(
            context.operation,
            EndReason::DeadlineExceeded,
        ));
    }
    Ok(())
}

fn retryable(kind: FailureKind) -> bool {
    matches!(
        kind,
        FailureKind::Timeout | FailureKind::Transport | FailureKind::RemoteTransient
    )
}

fn terminal_error(
    class: TerminalClass,
    operation: &'static str,
    attempts: usize,
    error: anyhow::Error,
) -> anyhow::Error {
    TerminalError::new(class, operation, attempts, error).into()
}

fn far_future() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60)
}

fn triggered_error(operation: &'static str, reason: EndReason) -> anyhow::Error {
    let (kind, outcome, message) = match reason {
        EndReason::Cancelled => (FailureKind::Internal, "cancelled", "operation cancelled"),
        EndReason::DeadlineExceeded => (
            FailureKind::Timeout,
            "timeout",
            "operation deadline exceeded",
        ),
        _ => (FailureKind::Internal, "permanent_error", "operation failed"),
    };
    operation_metric("k8s", operation, outcome);
    TerminalError::new(
        TerminalClass::new(reason, kind),
        operation,
        0,
        anyhow::anyhow!(message),
    )
    .into()
}

pub(super) fn is_deadline(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<TerminalError>()
            .is_some_and(|error| error.end_reason == EndReason::DeadlineExceeded)
    })
}

fn failure_reason(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Timeout => "timeout",
        FailureKind::Transport => "transport",
        FailureKind::RemoteTransient => "remote_transient",
        FailureKind::Permanent => "permanent",
        FailureKind::Internal => "internal",
    }
}
