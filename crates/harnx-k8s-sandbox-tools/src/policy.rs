use anyhow::Error;
use rand::RngExt;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Classification of the most recent boundary failure, independent of why retries ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Local deadline expired while waiting on boundary I/O.
    Timeout,
    /// Connection or protocol transport failed.
    Transport,
    /// Remote service explicitly reported a retryable condition.
    RemoteTransient,
    /// Request cannot succeed unchanged, such as validation or authorization failure.
    Permanent,
    /// Gateway invariant, decoding, or serialization failed.
    Internal,
}

/// Reason an operation ended after applying cancellation, deadline, and retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// Failure policy rejected another attempt.
    Failed,
    /// Caller cancellation interrupted the operation.
    Cancelled,
    /// Overall operation deadline expired.
    DeadlineExceeded,
    /// Retry attempt budget was consumed.
    AttemptsExhausted,
}

/// Terminal operation classification across policy outcome and boundary failure axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalClass {
    /// Policy condition that ended the operation.
    pub end_reason: EndReason,
    /// Classification of the last observed failure.
    pub last_kind: FailureKind,
}

impl TerminalClass {
    /// Groups the terminal policy outcome with its last boundary failure.
    pub fn new(end_reason: EndReason, last_kind: FailureKind) -> Self {
        Self {
            end_reason,
            last_kind,
        }
    }
}

/// Terminal policy error retaining both end reason and typed source chain.
#[derive(Debug)]
pub struct TerminalError {
    /// Policy condition that ended the operation.
    pub end_reason: EndReason,
    /// Classification of the last observed failure.
    pub last_kind: FailureKind,
    /// Stable operation phase name suitable for diagnostics.
    pub phase: &'static str,
    /// Number of boundary attempts that started.
    pub attempts: usize,
    source: Error,
}

impl TerminalError {
    /// Creates a terminal error while preserving the typed source in its error chain.
    pub fn new(
        class: TerminalClass,
        phase: &'static str,
        attempts: usize,
        source: impl Into<Error>,
    ) -> Self {
        Self {
            end_reason: class.end_reason,
            last_kind: class.last_kind,
            phase,
            attempts,
            source: source.into(),
        }
    }
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ended after {} attempt(s) ({:?}, {:?}): {:#}",
            self.phase, self.attempts, self.end_reason, self.last_kind, self.source
        )
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Samples a delay up to the exponential backoff bound.
pub trait JitterSampler: Send + Sync {
    /// Returns a duration in the inclusive range from zero through `upper`.
    fn sample(&self, upper: Duration) -> Duration;
}

#[derive(Debug, Default)]
pub struct RandomJitter;

impl JitterSampler for RandomJitter {
    fn sample(&self, upper: Duration) -> Duration {
        let upper_nanos = upper.as_nanos().min(u64::MAX as u128) as u64;
        if upper_nanos == 0 {
            return Duration::ZERO;
        }
        // Keep ThreadRng in this synchronous scope; it is not Send across await points.
        let nanos = rand::rng().random_range(0..=upper_nanos);
        Duration::from_nanos(nanos)
    }
}

/// Bounded full-jitter exponential backoff policy.
#[derive(Clone)]
pub struct BackoffConfig {
    /// Initial upper bound before exponential growth.
    pub base: Duration,
    /// Maximum jitter upper bound.
    pub cap: Duration,
    /// Total attempt count, including the initial attempt.
    pub max_attempts: usize,
    /// Randomness source, replaceable by deterministic test samplers.
    pub sampler: Arc<dyn JitterSampler>,
}

impl fmt::Debug for BackoffConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackoffConfig")
            .field("base", &self.base)
            .field("cap", &self.cap)
            .field("max_attempts", &self.max_attempts)
            .finish_non_exhaustive()
    }
}

impl BackoffConfig {
    /// Creates a policy and clamps `max_attempts` to at least the initial attempt.
    pub fn new(base: Duration, cap: Duration, max_attempts: usize) -> Self {
        Self {
            base,
            cap,
            max_attempts: max_attempts.max(1),
            sampler: Arc::new(RandomJitter),
        }
    }

    /// Samples full jitter for a retry and clips it to the remaining operation budget.
    pub fn delay(&self, retry_index: usize, remaining: Duration) -> Duration {
        let shift = u32::try_from(retry_index).unwrap_or(u32::MAX).min(127);
        let factor = 1_u128.checked_shl(shift).unwrap_or(u128::MAX);
        let exponential_nanos = self.base.as_nanos().saturating_mul(factor);
        let upper_nanos = exponential_nanos.min(self.cap.as_nanos());
        let upper = duration_from_nanos(upper_nanos).min(remaining);
        self.sampler.sample(upper).min(remaining)
    }

    #[cfg(test)]
    pub fn with_sampler(mut self, sampler: Arc<dyn JitterSampler>) -> Self {
        self.sampler = sampler;
        self
    }
}

fn duration_from_nanos(nanos: u128) -> Duration {
    let secs = (nanos / 1_000_000_000).min(u64::MAX as u128) as u64;
    let subsec_nanos = if secs == u64::MAX {
        999_999_999
    } else {
        (nanos % 1_000_000_000) as u32
    };
    Duration::new(secs, subsec_nanos)
}

pub fn operation_metric(boundary: &'static str, operation: &str, outcome: &'static str) {
    metrics::counter!(
        "harnx_sandbox_gateway_operation_total",
        "boundary" => boundary,
        "operation" => operation.to_string(),
        "outcome" => outcome,
    )
    .increment(1);
}

pub fn retry_metric(boundary: &'static str, operation: &str, reason: &'static str) {
    metrics::counter!(
        "harnx_sandbox_gateway_retries_total",
        "boundary" => boundary,
        "operation" => operation.to_string(),
        "reason" => reason,
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct ScriptedJitter(Mutex<Vec<Duration>>);

    impl JitterSampler for ScriptedJitter {
        fn sample(&self, upper: Duration) -> Duration {
            self.0.lock().remove(0).min(upper)
        }
    }

    #[test]
    fn backoff_is_exponential_capped_and_clipped() {
        let sampler = Arc::new(ScriptedJitter(Mutex::new(vec![
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(20),
        ])));
        let config = BackoffConfig::new(Duration::from_secs(1), Duration::from_secs(5), 4)
            .with_sampler(sampler);

        assert_eq!(
            config.delay(0, Duration::from_secs(30)),
            Duration::from_secs(1)
        );
        assert_eq!(
            config.delay(2, Duration::from_secs(30)),
            Duration::from_secs(4)
        );
        assert_eq!(
            config.delay(100, Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn category_metrics_have_only_bounded_labels() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            operation_metric("mcp", "bash_exec", "sandbox_error");
            operation_metric("mcp", "bash_exec", "timeout");
            operation_metric("k8s", "status", "cancelled");
            operation_metric("k8s", "status", "retry_exhausted");
            retry_metric("k8s", "status", "remote_transient");
        });
        let snapshot = snapshotter.snapshot().into_vec();

        assert_eq!(snapshot.len(), 5);
        let mut outcomes = snapshot
            .iter()
            .flat_map(|(key, _, _, _)| key.key().labels())
            .filter(|label| label.key() == "outcome")
            .map(|label| label.value().to_string())
            .collect::<Vec<_>>();
        outcomes.sort();
        assert_eq!(
            outcomes,
            ["cancelled", "retry_exhausted", "sandbox_error", "timeout"]
        );
        assert!(snapshot
            .iter()
            .all(|(_, _, _, value)| *value == DebugValue::Counter(1)));
        assert!(snapshot.iter().all(|(key, _, _, _)| {
            key.key()
                .labels()
                .all(|label| matches!(label.key(), "boundary" | "operation" | "outcome" | "reason"))
        }));
    }
}
