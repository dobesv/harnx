//! One observation deadline, including broker I/O, repair and presentation.
use std::{future::Future, time::Duration};
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

// Same per-operation bound as the durable log reader. A zero-second poll gets
// one logical snapshot under this separate bound, not a fresh bound per read.
const IO_LIMIT: Duration = Duration::from_secs(10);
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    Expired,
    Cancelled,
    Unavailable,
}

pub(super) struct Budget {
    start: Instant,
    deadline: Instant,
    zero: bool,
    cancel: CancellationToken,
}

impl Budget {
    pub(super) fn new(seconds: u64, cancel: CancellationToken) -> Self {
        let start = Instant::now();
        let zero = seconds == 0;
        Self {
            start,
            deadline: start
                + if zero {
                    IO_LIMIT
                } else {
                    Duration::from_secs(seconds)
                },
            zero,
            cancel,
        }
    }

    pub(super) fn elapsed_ms(&self) -> u64 {
        self.start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub(super) fn check(&self) -> Result<(), Stop> {
        if self.cancel.is_cancelled() {
            Err(Stop::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(Stop::Expired)
        } else {
            Ok(())
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.zero
    }

    pub(super) async fn io<T>(&self, future: impl Future<Output = T>) -> Result<T, Stop> {
        self.check()?;
        let deadline = self.deadline.min(Instant::now() + IO_LIMIT);
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(Stop::Cancelled),
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

    pub(super) async fn poll(&self) -> Result<(), Stop> {
        self.io(tokio::time::sleep(POLL_INTERVAL)).await
    }
}
