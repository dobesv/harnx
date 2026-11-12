//! One observation deadline, including broker I/O, repair and presentation.
use harnx_toolset::{AutonomousRunContext, ToolInvocationContext};
use std::{future::Future, time::Duration};
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::super::WaitLimit;

// Same per-operation bound as the durable log reader. A snapshot budget gets
// one logical pass under this bound, not a fresh bound per read.
const IO_LIMIT: Duration = Duration::from_secs(10);
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    Expired,
    Cancelled,
    Unavailable,
    /// A new message arrived for the caller. The wait ends with its normal
    /// result; nothing failed.
    NewMessage,
    /// The wait reached the bound it keeps before the caller's run deadline.
    CallerDeadline,
}

/// What ends an observation besides its own limit.
#[derive(Default)]
pub(super) struct BudgetStops {
    /// The call was cancelled.
    pub(super) cancel: CancellationToken,
    /// A new message arrived for the caller's session.
    pub(super) new_message: CancellationToken,
    /// The latest instant a wait may run to: the caller's run deadline less
    /// the margin the caller needs to act on the result.
    pub(super) caller_deadline: Option<Instant>,
}

impl BudgetStops {
    /// The stops the call's transport supplies.
    pub(super) fn for_call(cancel: CancellationToken, context: &ToolInvocationContext) -> Self {
        Self {
            cancel,
            new_message: context.new_message.clone(),
            caller_deadline: context
                .run_context
                .as_ref()
                .and_then(AutonomousRunContext::wait_bound)
                .map(|bound| Instant::now() + bound),
        }
    }
}

pub(super) struct Budget {
    start: Instant,
    /// `None` for a forever wait: only the other stops and the per-operation
    /// I/O cap bound it.
    limit_deadline: Option<Instant>,
    stops: BudgetStops,
}

impl Budget {
    pub(super) fn new(limit: WaitLimit, stops: BudgetStops) -> Self {
        let start = Instant::now();
        let limit_deadline = match limit {
            WaitLimit::Seconds(seconds) => Some(start + Duration::from_secs(seconds)),
            WaitLimit::Forever => None,
        };
        Self {
            start,
            limit_deadline,
            stops,
        }
    }

    /// One logical pass under the shared I/O cap, for reads that must never
    /// wait on the child.
    pub(super) fn snapshot(cancel: CancellationToken) -> Self {
        let start = Instant::now();
        Self {
            start,
            limit_deadline: Some(start + IO_LIMIT),
            stops: BudgetStops {
                cancel,
                ..Default::default()
            },
        }
    }

    pub(super) fn elapsed_ms(&self) -> u64 {
        self.start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// The earlier of the wait's limit and the caller bound.
    fn deadline(&self) -> Option<Instant> {
        match (self.limit_deadline, self.stops.caller_deadline) {
            (Some(limit), Some(caller)) => Some(limit.min(caller)),
            (limit, caller) => limit.or(caller),
        }
    }

    /// Whether the caller bound, not the wait's limit, is what ends the wait.
    fn caller_bound_is_first(&self) -> bool {
        match (self.limit_deadline, self.stops.caller_deadline) {
            (Some(limit), Some(caller)) => caller <= limit,
            (None, caller) => caller.is_some(),
            (_, None) => false,
        }
    }

    pub(super) fn check(&self) -> Result<(), Stop> {
        if self.stops.cancel.is_cancelled() {
            Err(Stop::Cancelled)
        } else if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Err(if self.caller_bound_is_first() {
                Stop::CallerDeadline
            } else {
                Stop::Expired
            })
        } else {
            Ok(())
        }
    }

    pub(super) async fn io<T>(&self, future: impl Future<Output = T>) -> Result<T, Stop> {
        self.check()?;
        let io_deadline = Instant::now() + IO_LIMIT;
        let deadline = self.deadline().map_or(io_deadline, |d| d.min(io_deadline));
        let result = tokio::select! {
            biased;
            _ = self.stops.cancel.cancelled() => Err(Stop::Cancelled),
            result = timeout_at(deadline, future) => result.map_err(|_| Stop::Unavailable),
        };
        // timeout_at polls a ready future before its timer. Decode/render work
        // that crosses the deadline must not escape the shared absolute bound.
        self.check()?;
        if Instant::now() >= deadline {
            return Err(Stop::Unavailable);
        }
        result
    }

    /// Sleep until the next observation. A new message ends the wait here,
    /// between observations, so the result it returns is always a complete
    /// one rather than a read cut short.
    pub(super) async fn poll(&self) -> Result<(), Stop> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.stops.new_message.cancelled() => Err(Stop::NewMessage),
            result = self.io(tokio::time::sleep(POLL_INTERVAL)) => result,
        }
    }
}
