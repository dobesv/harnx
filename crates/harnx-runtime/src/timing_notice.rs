//! Deterministic timing notice schedule for active runs.
//!
//! Pure evaluation that takes explicit timestamps and produces timing
//! notices with structured receipts. No clocks, no timers, no side effects.
//! Each notice line pairs the structured receipt and machine-parsed timing
//! fields with self-contained guidance prose explaining what the note is,
//! what the fields mean, and how to act on them (issue #3006).
//!
//! # Schedule
//!
//! - **Initial**: First notice at run start (elapsed = 0)
//! - **Thresholds**: 50%, 75%, 90% of budget (deadline - admitted_at)
//! - **Periodic**: Every 900 seconds of elapsed time
//!
//! # Coalescing
//!
//! Missed milestones are coalesced into one notice that covers all
//! unemitted thresholds and the highest elapsed bucket. No backfill flood.
//!
//! # No Deadline
//!
//! Runs without deadlines (unmanaged/lima runs) get initial and periodic
//! notices only, no percentage thresholds.
//!
//! # Cache Keying
//!
//! State is keyed by (storage, session, invocation) and resets on:
//! - New run (new invocation_id)
//! - Fork (different parent context)
//!
//! State persists across:
//! - Retries (same invocation_id)
//! - Compaction (recovery from raw log)

use chrono::{DateTime, Utc};
use std::time::Duration;

use harnx_core::timing_identity::{
    MilestoneCoverage, TimingCacheKey, TimingReceipt, TIMING_CONTEXT_SEPARATOR, TIMING_HEADER,
};

use crate::nats_session_metadata::{InvocationIdentity, RunLimitsRecord};

mod guidance;

use guidance::build_timing_guidance;

/// Seconds per elapsed bucket (900s = 15 minutes).
pub const ELAPSED_BUCKET_SECS: u64 = 900;

/// Timing state for one invocation.
///
/// Tracks which milestones have been emitted so the scheduler can
/// avoid duplicates and coalesce missed thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingState {
    /// Unique cache key for this timing state.
    pub key: TimingCacheKey,
    /// Fallback origin for unmanaged runs (captured once at logical start).
    pub fallback_origin: Option<DateTime<Utc>>,
    /// Highest emission with receipt.
    pub last_receipt: Option<TimingReceipt>,
}

impl TimingState {
    /// Create state for a new invocation.
    pub fn new(key: TimingCacheKey) -> Self {
        Self {
            key,
            fallback_origin: None,
            last_receipt: None,
        }
    }

    /// Create state with a fallback origin for unmanaged runs.
    pub fn with_fallback_origin(key: TimingCacheKey, origin: DateTime<Utc>) -> Self {
        Self {
            key,
            fallback_origin: Some(origin),
            last_receipt: None,
        }
    }

    /// Check if this state matches a given key (for reset detection).
    pub fn matches_key(&self, key: &TimingCacheKey) -> bool {
        self.key == *key
    }

    /// Reset state for a new key.
    pub fn reset_for_key(&mut self, key: TimingCacheKey) {
        self.key = key;
        self.fallback_origin = None;
        self.last_receipt = None;
    }
}

/// Input to the timing schedule evaluator.
#[derive(Debug, Clone)]
pub struct TimingInput {
    /// Current time (explicit for deterministic testing).
    pub now: DateTime<Utc>,
    /// Run identity and deadline (from immutable admission record).
    pub run_limits: Option<RunLimitsRecord>,
    /// Origin captured once at logical start when no admission record exists.
    pub fallback_origin: Option<DateTime<Utc>>,
    /// Current invocation identity.
    pub invocation_id: InvocationIdentity,
    /// Previously emitted progress (recovered from raw log).
    pub committed_progress: Option<TimingReceipt>,
}

impl TimingInput {
    /// Create timing input from a run limits record.
    pub fn from_run_limits(now: DateTime<Utc>, run_limits: RunLimitsRecord) -> Self {
        let invocation_id = run_limits.invocation_id.clone();
        Self {
            now,
            run_limits: Some(run_limits),
            fallback_origin: None,
            invocation_id,
            committed_progress: None,
        }
    }

    /// Create input for an unmanaged run using its once-captured logical origin.
    pub fn unmanaged(
        now: DateTime<Utc>,
        origin: DateTime<Utc>,
        invocation_id: InvocationIdentity,
    ) -> Self {
        Self {
            now,
            run_limits: None,
            fallback_origin: Some(origin),
            invocation_id,
            committed_progress: None,
        }
    }

    /// Get the effective origin (admitted_at or fallback).
    pub fn origin(&self) -> Option<DateTime<Utc>> {
        self.run_limits
            .as_ref()
            .map(|r| r.admitted_at)
            .or(self.fallback_origin)
    }

    /// Get the effective deadline, if any.
    pub fn deadline(&self) -> Option<DateTime<Utc>> {
        self.run_limits.as_ref().and_then(|r| r.deadline)
    }

    /// Get elapsed time since origin.
    ///
    /// Returns 0 if origin is not available or if now < origin (backward time).
    pub fn elapsed(&self) -> Duration {
        let origin = match self.origin() {
            Some(o) => o,
            None => return Duration::ZERO,
        };
        if self.now < origin {
            return Duration::ZERO;
        }
        self.now
            .signed_duration_since(origin)
            .to_std()
            .unwrap_or(Duration::ZERO)
    }

    /// Get remaining time until deadline.
    ///
    /// Returns `None` if no deadline or already expired.
    /// Returns `None` if remaining would be negative (already expired).
    pub fn remaining(&self) -> Option<Duration> {
        positive_duration_between(self.deadline()?, self.now)
    }

    /// Get total budget (deadline - admitted_at).
    ///
    /// Returns `None` if no deadline or nonpositive budget.
    fn budget(&self) -> Option<Duration> {
        positive_duration_between(self.deadline()?, self.origin()?)
    }

    /// Check if run is expired (deadline reached or passed).
    pub fn is_expired(&self) -> bool {
        match self.deadline() {
            Some(deadline) => self.now >= deadline,
            None => false,
        }
    }
}

fn positive_duration_between(end: DateTime<Utc>, start: DateTime<Utc>) -> Option<Duration> {
    end.signed_duration_since(start)
        .to_std()
        .ok()
        .filter(|duration| !duration.is_zero())
}

/// Output from the timing schedule evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingOutput {
    /// Whether a notice should be emitted.
    pub emit: bool,
    /// Receipt for the notice (if emitting).
    pub receipt: Option<TimingReceipt>,
    /// Complete timing notice line with receipt and human-readable context.
    pub notice_line: Option<String>,
    /// Reason for emission (for testing/debugging).
    pub reason: Option<TimingReason>,
}

/// Reason for timing notice emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingReason {
    /// Initial notice at run start.
    Initial,
    /// 50% threshold reached.
    Threshold50,
    /// 75% threshold reached.
    Threshold75,
    /// 90% threshold reached.
    Threshold90,
    /// Periodic elapsed bucket.
    PeriodicBucket(u32),
    /// Multiple milestones coalesced.
    Coalesced,
    /// No emission needed (already covered).
    NoChange,
}

/// Evaluate the timing schedule and determine what notice to emit.
///
/// This is a pure function: it does not read clocks or write state.
/// The caller is responsible for providing explicit timestamps and
/// committed progress.
///
/// # Expiry Guard
///
/// If the run is expired (now >= deadline), no notice is emitted.
/// The expiry guard is NOT bypassed for zero-budget or nonpositive budget runs.
/// If deadline <= admitted_at, the budget is invalid and no timing notices are emitted.
pub fn evaluate_timing(input: &TimingInput) -> TimingOutput {
    let Some(origin) = input.origin() else {
        return TimingOutput::no_notice(None);
    };
    if input.now < origin {
        return TimingOutput::no_notice(None);
    }
    // Keep expiry authoritative, including nonpositive budgets.
    if input.is_expired() {
        return TimingOutput::no_notice(Some(TimingReason::NoChange));
    }
    let elapsed = input.elapsed();
    let current_bucket = (elapsed.as_secs() / ELAPSED_BUCKET_SECS) as u32;
    let committed_bucket = input
        .committed_progress
        .as_ref()
        .map(|receipt| receipt.highest_elapsed_bucket)
        .unwrap_or(0);
    let milestones = due_milestones(input, elapsed, current_bucket, committed_bucket);
    if milestones.coverage == MilestoneCoverage::NONE && current_bucket <= committed_bucket {
        return TimingOutput::no_notice(Some(TimingReason::NoChange));
    }
    let committed_coverage = input
        .committed_progress
        .as_ref()
        .map(|receipt| receipt.coverage)
        .unwrap_or(MilestoneCoverage::NONE);
    let receipt = TimingReceipt::new(
        origin,
        input.invocation_id.as_str().to_string(),
        committed_coverage | milestones.coverage,
        current_bucket.max(committed_bucket),
    );
    let notice_line = format!(
        "{}{}{}{}",
        TIMING_HEADER,
        receipt.to_json(),
        TIMING_CONTEXT_SEPARATOR,
        build_timing_context(input, origin, milestones.reason, receipt.coverage)
    );
    TimingOutput {
        emit: true,
        receipt: Some(receipt),
        notice_line: Some(notice_line),
        reason: milestones.reason,
    }
}

impl TimingOutput {
    fn no_notice(reason: Option<TimingReason>) -> Self {
        Self {
            emit: false,
            receipt: None,
            notice_line: None,
            reason,
        }
    }
}

#[derive(Default)]
struct DueMilestones {
    coverage: MilestoneCoverage,
    reason: Option<TimingReason>,
}

impl DueMilestones {
    fn add(&mut self, coverage: MilestoneCoverage, reason: TimingReason) {
        self.coverage |= coverage;
        self.reason = Some(match self.reason {
            None => reason,
            Some(_) => TimingReason::Coalesced,
        });
    }

    fn cover_percentages(&mut self, fraction: f64, committed: MilestoneCoverage) {
        let thresholds = [
            (
                0.50,
                MilestoneCoverage::PERCENT_50,
                TimingReason::Threshold50,
            ),
            (
                0.75,
                MilestoneCoverage::PERCENT_75,
                TimingReason::Threshold75,
            ),
            (
                0.90,
                MilestoneCoverage::PERCENT_90,
                TimingReason::Threshold90,
            ),
        ];
        for (threshold, coverage, reason) in thresholds {
            if fraction >= threshold && !committed.contains(coverage) {
                self.add(coverage, reason);
            }
        }
    }
}

fn due_milestones(
    input: &TimingInput,
    elapsed: Duration,
    current_bucket: u32,
    committed_bucket: u32,
) -> DueMilestones {
    let committed = input
        .committed_progress
        .as_ref()
        .map(|receipt| receipt.coverage)
        .unwrap_or(MilestoneCoverage::NONE);
    let mut due = DueMilestones::default();
    if !committed.contains(MilestoneCoverage::INITIAL) {
        due.add(MilestoneCoverage::INITIAL, TimingReason::Initial);
    }
    if let Some(budget) = input.budget() {
        due.cover_percentages(elapsed.as_secs_f64() / budget.as_secs_f64(), committed);
    }
    // Track only the highest missed bucket; never backfill a notice flood.
    if current_bucket > committed_bucket {
        due.add(
            MilestoneCoverage::NONE,
            TimingReason::PeriodicBucket(current_bucket),
        );
    }
    due
}

/// Build the human-readable timing context: timing fields, then guidance.
///
/// Fields are machine-parsed by the compact display helpers; guidance lives
/// after a second `TIMING_CONTEXT_SEPARATOR`, which the display parser
/// stops at, so prose wording cannot corrupt the fields.
fn build_timing_context(
    input: &TimingInput,
    origin: DateTime<Utc>,
    reason: Option<TimingReason>,
    coverage: MilestoneCoverage,
) -> String {
    let mut parts = vec![];

    // Wall-clock timestamps first
    parts.push(format!("start: {}", origin.format("%Y-%m-%dT%H:%M:%SZ")));
    parts.push(format!(
        "current: {}",
        input.now.format("%Y-%m-%dT%H:%M:%SZ")
    ));

    // Elapsed time
    parts.push(format_duration(input.elapsed(), "elapsed"));

    // Deadline and related fields (if deadline exists)
    if let Some(deadline) = input.deadline() {
        parts.push(format!(
            "deadline: {}",
            deadline.format("%Y-%m-%dT%H:%M:%SZ")
        ));
        if let Some(remaining) = input.remaining() {
            parts.push(format_duration(remaining, "remaining"));
        }
        if let Some(budget) = input.budget() {
            parts.push(format_duration(budget, "budget"));
        }
    }

    let fields = parts.join(", ");
    let guidance = build_timing_guidance(reason, coverage, input.deadline().is_some());
    format!("{fields}{TIMING_CONTEXT_SEPARATOR}{guidance}")
}

/// Format a duration for human-readable output.
fn format_duration(d: Duration, label: &str) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}: {}s", label, secs)
    } else {
        let mins = secs / 60;
        let leftover_secs = secs % 60;
        if leftover_secs == 0 {
            format!("{}: {}m", label, mins)
        } else {
            format!("{}: {}m {}s", label, mins, leftover_secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nats_session_metadata::{RunIdentity, RunLimitsRecord};
    use harnx_core::config_data::{RunLimitsConfig, RunLimitsTimeout};
    use harnx_core::timing_identity::{is_timing_notice, TimingCacheKey};

    fn fixed_time() -> DateTime<Utc> {
        "2024-01-15T12:00:00Z".parse().unwrap()
    }

    fn make_run_limits(admitted_at: DateTime<Utc>, timeout_secs: u64) -> RunLimitsRecord {
        RunLimitsRecord::admit_root(
            RunIdentity::new(),
            InvocationIdentity::new(),
            admitted_at,
            RunLimitsConfig {
                timeout_secs: RunLimitsTimeout::Finite(
                    std::num::NonZeroU64::new(timeout_secs).expect("positive timeout"),
                ),
            },
            None,
            crate::nats_session_metadata::CallTimeoutOverride::Omitted,
        )
        .unwrap()
    }

    fn make_cache_key(invocation_id: &InvocationIdentity) -> TimingCacheKey {
        TimingCacheKey::new(
            "test-storage".to_string(),
            "test-session".to_string(),
            invocation_id.as_str().to_string(),
        )
    }

    fn make_timing_input(
        run_limits: RunLimitsRecord,
        now: DateTime<Utc>,
        committed_progress: Option<TimingReceipt>,
    ) -> TimingInput {
        let invocation_id = run_limits.invocation_id.clone();
        TimingInput {
            fallback_origin: None,
            now,
            run_limits: Some(run_limits),
            invocation_id,
            committed_progress,
        }
    }

    fn evaluate_with_progress(
        timeout_secs: u64,
        elapsed_secs: i64,
        coverage: MilestoneCoverage,
        bucket: u32,
    ) -> TimingOutput {
        let admitted = fixed_time();
        let limits = make_run_limits(admitted, timeout_secs);
        let committed = TimingReceipt::new(
            admitted,
            limits.invocation_id.as_str().to_string(),
            coverage,
            bucket,
        );
        let now = admitted + chrono::Duration::seconds(elapsed_secs);
        evaluate_timing(&make_timing_input(limits, now, Some(committed)))
    }

    fn evaluate_at_initial_deadline(timeout_secs: u64, deadline_offset_secs: i64) -> TimingOutput {
        let admitted = fixed_time();
        let mut limits = make_run_limits(admitted, timeout_secs);
        limits.deadline = Some(admitted + chrono::Duration::seconds(deadline_offset_secs));
        evaluate_timing(&make_timing_input(limits, admitted, None))
    }

    fn assert_initial_receipt(receipt: &TimingReceipt, expected_origin: DateTime<Utc>) {
        assert!(receipt.coverage.contains(MilestoneCoverage::INITIAL));
        assert_eq!(receipt.highest_elapsed_bucket, 0);
        assert_eq!(receipt.origin, expected_origin);
    }

    fn assert_notice_timestamp(notice: &str, label: &str, time: DateTime<Utc>, message: &str) {
        assert!(
            notice.contains(&format!("{label}: {}", time.format("%Y-%m-%dT%H:%M:%SZ"))),
            "{message}"
        );
    }

    fn assert_initial_notice_start_elapsed(
        notice: &str,
        admitted: DateTime<Utc>,
        elapsed_message: &str,
    ) {
        assert_notice_timestamp(
            notice,
            "start",
            admitted,
            "notice must contain start timestamp",
        );
        assert_notice_timestamp(
            notice,
            "current",
            admitted,
            "notice must contain current timestamp",
        );
        assert!(notice.contains("elapsed: 0s"), "{elapsed_message}");
    }

    fn assert_notice_deadline_fields(notice: &str, deadline: DateTime<Utc>) {
        assert_notice_timestamp(
            notice,
            "deadline",
            deadline,
            "notice must contain exact deadline timestamp",
        );
        assert!(
            notice.contains("remaining:"),
            "notice must contain remaining duration"
        );
        assert!(
            notice.contains("budget:"),
            "notice must contain budget duration"
        );
    }

    fn assert_coverage_contains_initial_and(receipt: &TimingReceipt, milestone: MilestoneCoverage) {
        assert!(receipt.coverage.contains(MilestoneCoverage::INITIAL));
        assert!(receipt.coverage.contains(milestone));
    }

    fn assert_all_percentage_thresholds_covered(receipt: &TimingReceipt) {
        assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_50));
        assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_75));
        assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_90));
    }

    fn assert_no_deadline_notice_omits_limits(notice: &str, messages: [&str; 3]) {
        assert!(!notice.contains("deadline:"), "{}", messages[0]);
        assert!(!notice.contains("remaining:"), "{}", messages[1]);
        assert!(!notice.contains("budget:"), "{}", messages[2]);
    }

    fn assert_periodic_notice_timestamps(
        notice: &str,
        admitted: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        assert_notice_timestamp(notice, "start", admitted, "must contain start timestamp");
        assert_notice_timestamp(notice, "current", now, "must contain current timestamp");
        assert!(
            notice.contains("elapsed: 15m"),
            "must show 15 minutes elapsed"
        );
    }

    fn assert_initial_coverage_and_origin(receipt: &TimingReceipt, expected_origin: DateTime<Utc>) {
        assert!(receipt.coverage.contains(MilestoneCoverage::INITIAL));
        assert_eq!(receipt.origin, expected_origin);
    }

    fn assert_unlimited_notice_timestamps(notice: &str, origin: DateTime<Utc>) {
        assert_notice_timestamp(notice, "start", origin, "must contain start timestamp");
        assert_notice_timestamp(notice, "current", origin, "must contain current timestamp");
    }

    fn assert_deadline_notice_timestamps(
        notice: &str,
        admitted: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) {
        assert_notice_timestamp(
            notice,
            "start",
            admitted,
            "must contain start timestamp matching admitted_at",
        );
        assert_notice_timestamp(
            notice,
            "current",
            admitted,
            "must contain current timestamp matching now",
        );
        assert_notice_timestamp(
            notice,
            "deadline",
            deadline,
            "must contain deadline timestamp matching run_limits.deadline",
        );
    }

    fn assert_deadline_notice_durations(notice: &str) {
        assert!(notice.contains("elapsed:"), "must contain elapsed");
        assert!(notice.contains("remaining:"), "must contain remaining");
        assert!(notice.contains("budget:"), "must contain budget");
    }

    fn assert_notice_line_structure(line: &str) {
        assert!(line.starts_with(TIMING_HEADER));
        assert!(line.contains(TIMING_CONTEXT_SEPARATOR));
        assert!(line.contains(r#""version":"v1""#));
    }

    fn assert_notice_receipt_json_keys(line: &str) {
        assert!(line.contains(r#""coverage""#));
        assert!(line.contains(r#""origin""#));
        assert!(line.contains(r#""invocation_id""#));
    }

    fn assert_notice_human_readable_context(line: &str) {
        assert!(line.contains("elapsed:"));
        assert!(line.contains("remaining:"));
        assert!(line.contains("budget:"));
    }

    fn assert_periodic_bucket_receipt(receipt: &TimingReceipt, origin: DateTime<Utc>, bucket: u32) {
        assert_eq!(receipt.origin, origin);
        assert_eq!(receipt.highest_elapsed_bucket, bucket);
        assert_eq!(receipt.coverage, MilestoneCoverage::INITIAL);
    }

    #[test]
    fn initial_notice_at_run_start() {
        let admitted = fixed_time();
        let run_limits = make_run_limits(admitted, 3600);
        let input = make_timing_input(run_limits, admitted, None);

        let output = evaluate_timing(&input);
        assert!(output.emit);
        assert!(output.reason.is_some());
        let receipt = output.receipt.unwrap();
        assert_initial_receipt(&receipt, admitted);

        let notice = output.notice_line.unwrap();
        assert_initial_notice_start_elapsed(
            &notice,
            admitted,
            "notice must contain elapsed duration",
        );
        let deadline = admitted + chrono::Duration::seconds(3600);
        assert_notice_deadline_fields(&notice, deadline);
    }

    #[test]
    fn no_duplicate_initial_notice() {
        let output = evaluate_with_progress(3600, 0, MilestoneCoverage::INITIAL, 0);
        assert!(!output.emit);
        assert_eq!(output.reason, Some(TimingReason::NoChange));
    }

    #[test]
    fn threshold_50_percent_at_boundary() {
        let admitted = fixed_time();
        let output = evaluate_with_progress(3600, 1800, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert_coverage_contains_initial_and(&receipt, MilestoneCoverage::PERCENT_50);
        assert!(matches!(output.reason, Some(TimingReason::Coalesced)));
        assert_eq!(receipt.origin, admitted);
    }

    #[test]
    fn threshold_75_percent_at_boundary() {
        let output = evaluate_with_progress(
            3600,
            2700,
            MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50,
            0,
        );
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_75));
    }

    #[test]
    fn threshold_90_percent_at_boundary() {
        let output = evaluate_with_progress(
            3600,
            3240,
            MilestoneCoverage::INITIAL
                | MilestoneCoverage::PERCENT_50
                | MilestoneCoverage::PERCENT_75,
            0,
        );
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert!(receipt.coverage.contains(MilestoneCoverage::PERCENT_90));
    }

    #[test]
    fn skipped_thresholds_coalesced() {
        let output = evaluate_with_progress(3600, 3240, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        assert_eq!(output.reason, Some(TimingReason::Coalesced));
        let receipt = output.receipt.unwrap();
        assert_all_percentage_thresholds_covered(&receipt);
    }

    #[test]
    fn no_emission_when_already_covered() {
        let output = evaluate_with_progress(
            3600,
            2000,
            MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50,
            2,
        );
        assert!(
            !output.emit,
            "expected no emission at 55% when 50% and bucket 2 already covered"
        );
        assert_eq!(output.reason, Some(TimingReason::NoChange));
    }

    #[test]
    fn same_timestamp_duplicate_returns_none() {
        let output = evaluate_with_progress(
            3600,
            2000,
            MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50,
            2,
        );
        assert!(!output.emit);
    }

    #[test]
    fn backward_time_returns_no_emission() {
        let admitted = fixed_time();
        let run_limits = make_run_limits(admitted, 3600);
        let now = admitted - chrono::Duration::seconds(100);
        let input = make_timing_input(run_limits, now, None);

        let output = evaluate_timing(&input);
        assert!(!output.emit);
        assert!(output.receipt.is_none());
    }

    #[test]
    fn no_deadline_emits_initial_and_periodic_only() {
        let admitted = fixed_time();
        let mut run_limits = make_run_limits(admitted, 10);
        run_limits.deadline = None; // No deadline
        let invocation_id = run_limits.invocation_id.clone();

        // Initial
        let input = make_timing_input(run_limits.clone(), admitted, None);
        let output = evaluate_timing(&input);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert!(receipt.coverage.contains(MilestoneCoverage::INITIAL));
        assert!(!receipt.coverage.contains(MilestoneCoverage::PERCENT_50));

        let notice = output.notice_line.expect("must have notice line");
        assert_initial_notice_start_elapsed(&notice, admitted, "notice must contain elapsed");
        assert_no_deadline_notice_omits_limits(
            &notice,
            [
                "unlimited run must NOT contain deadline timestamp",
                "unlimited run must NOT contain remaining duration",
                "unlimited run must NOT contain budget duration",
            ],
        );

        // Periodic at bucket 1 (900s)
        let now = admitted + chrono::Duration::seconds(900);
        let committed = TimingReceipt::initial(admitted, invocation_id.as_str().to_string());
        let periodic_input = make_timing_input(run_limits, now, Some(committed));

        let output = evaluate_timing(&periodic_input);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert_eq!(receipt.highest_elapsed_bucket, 1);
    }

    #[test]
    fn expired_run_emits_no_notice() {
        let output = evaluate_with_progress(60, 120, MilestoneCoverage::INITIAL, 0);
        assert!(!output.emit, "expired run must not emit timing notice");
        assert_eq!(output.reason, Some(TimingReason::NoChange));
    }

    #[test]
    fn nonpositive_budget_no_emission() {
        let output = evaluate_at_initial_deadline(10, 0);
        assert!(
            !output.emit,
            "nonpositive budget (deadline <= origin) must not emit timing notice"
        );
    }

    #[test]
    fn deadline_before_admitted_is_expired() {
        let output = evaluate_at_initial_deadline(10, -10);
        assert!(
            !output.emit,
            "deadline < admitted_at must not emit timing notice"
        );
    }

    #[test]
    fn periodic_bucket_at_900_seconds() {
        let admitted = fixed_time();
        let now = admitted + chrono::Duration::seconds(900);
        let output = evaluate_with_progress(7200, 900, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert_eq!(receipt.highest_elapsed_bucket, 1);

        let notice = output.notice_line.expect("must have notice line");
        assert_periodic_notice_timestamps(&notice, admitted, now);
    }

    #[test]
    fn skipped_buckets_advance_to_current() {
        let output = evaluate_with_progress(7200, 2800, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert_eq!(receipt.highest_elapsed_bucket, 3);
    }

    #[test]
    fn child_run_uses_own_admitted_at_not_parent() {
        let parent_admitted = fixed_time();
        let child_admitted = parent_admitted + chrono::Duration::seconds(1800);

        let parent_limits = make_run_limits(parent_admitted, 7200);
        let parent_deadline = parent_limits.deadline.unwrap();

        let child_limits = RunLimitsRecord::admit_child(
            &parent_limits,
            InvocationIdentity::new(),
            crate::nats_session_metadata::InvocationEdgeKind::Delegation,
            child_admitted,
            RunLimitsConfig::default(),
            None,
            crate::nats_session_metadata::CallTimeoutOverride::Omitted,
        )
        .unwrap();

        assert!(child_limits.deadline.unwrap() <= parent_deadline);
        assert_eq!(child_limits.admitted_at, child_admitted);

        let now = child_admitted + chrono::Duration::seconds(60);
        let input = make_timing_input(child_limits, now, None);

        let output = evaluate_timing(&input);
        assert!(output.emit);
        let receipt = output.receipt.unwrap();
        assert_initial_coverage_and_origin(&receipt, child_admitted);
    }

    #[test]
    fn state_initialization_for_new_key() {
        let invocation_id = InvocationIdentity::new();
        let key = make_cache_key(&invocation_id);
        let state = TimingState::new(key.clone());
        assert!(state.matches_key(&key));
        assert!(state.last_receipt.is_none());
    }

    #[test]
    fn state_reset_clears_progress() {
        let invocation_id1 = InvocationIdentity::new();
        let key1 = make_cache_key(&invocation_id1);
        let mut state = TimingState::new(key1.clone());
        state.last_receipt = Some(TimingReceipt::initial(
            fixed_time(),
            invocation_id1.as_str().to_string(),
        ));

        let invocation_id2 = InvocationIdentity::new();
        let key2 = make_cache_key(&invocation_id2);
        state.reset_for_key(key2.clone());
        assert!(state.last_receipt.is_none());
        assert!(state.matches_key(&key2));
        assert!(!state.matches_key(&key1));
    }

    #[test]
    fn notice_line_compacts_for_display() {
        // Contract test: evaluate_timing builds the notice line, and the
        // display helper in harnx-core must parse it into a compact one-liner.
        // If the line format drifts, compact_display degrades to its
        // truncated-first-line fallback.
        let output = evaluate_at_initial_deadline(1440 * 60, 1440 * 60);
        let line = output.notice_line.expect("initial notice");
        let compact =
            harnx_core::runtime_note::compact_display(&line).expect("notice line must compact");
        assert_eq!(compact, "⏱ 0s elapsed · 1440m left of 1440m");
        assert!(!compact.contains("[Runtime note]"), "compact: {compact}");
    }

    #[test]
    fn is_timing_notice_classifier_accepts_trailing_context() {
        let origin = fixed_time();
        let receipt = TimingReceipt::initial(origin, "inv-test".to_string());
        let notice = format!(
            "{}{}{}Elapsed: 0s, Remaining: 1h, Budget: 1h",
            TIMING_HEADER,
            receipt.to_json(),
            TIMING_CONTEXT_SEPARATOR
        );
        assert!(
            is_timing_notice(&notice),
            "classifier must accept notices with trailing context"
        );
    }

    #[test]
    fn unmanaged_run_notice_has_start_current_no_deadline() {
        let origin: DateTime<Utc> = "2024-03-20T10:00:00Z".parse().unwrap();
        let input = TimingInput::unmanaged(origin, origin, InvocationIdentity::new());

        let output = evaluate_timing(&input);
        assert!(output.emit);

        let notice = output.notice_line.expect("must have notice line");
        assert_unlimited_notice_timestamps(&notice, origin);
        assert_no_deadline_notice_omits_limits(
            &notice,
            [
                "unlimited run must NOT have deadline timestamp",
                "unlimited run must NOT have remaining",
                "unlimited run must NOT have budget",
            ],
        );
    }

    #[test]
    fn deadline_run_notice_includes_all_timestamp_fields() {
        let admitted: DateTime<Utc> = "2024-06-15T14:00:00Z".parse().unwrap();
        let run_limits = make_run_limits(admitted, 3600);
        let actual_deadline = run_limits.deadline.expect("run_limits has deadline");
        let input = make_timing_input(run_limits, admitted, None);

        let output = evaluate_timing(&input);
        assert!(output.emit);

        let notice = output.notice_line.expect("must have notice line");
        assert_deadline_notice_timestamps(&notice, admitted, actual_deadline);
        assert_deadline_notice_durations(&notice);
    }

    #[test]
    fn unmanaged_run_emits_initial() {
        let admitted = fixed_time();
        let mut input = TimingInput::unmanaged(admitted, admitted, InvocationIdentity::new());
        assert!(input.deadline().is_none());
        assert!(input.run_limits.is_none());
        let initial = evaluate_timing(&input);
        assert!(initial.emit);
        input.committed_progress = initial.receipt;
        input.now = admitted + chrono::Duration::seconds(900);
        let periodic = evaluate_timing(&input);
        assert!(periodic.emit);
        let receipt = periodic.receipt.unwrap();
        assert_periodic_bucket_receipt(&receipt, admitted, 1);
    }

    #[test]
    fn notice_line_includes_human_readable_context() {
        let admitted = fixed_time();
        let run_limits = make_run_limits(admitted, 3600);
        let input = make_timing_input(run_limits, admitted, None);

        let output = evaluate_timing(&input);
        assert!(output.emit);

        let notice_line = output.notice_line.expect("must have notice line");
        assert_notice_line_structure(&notice_line);
        assert_notice_receipt_json_keys(&notice_line);
        assert_notice_human_readable_context(&notice_line);
    }

    #[test]
    fn receipt_origin_matches_run_limits_admitted_at() {
        let admitted: DateTime<Utc> = "2024-03-15T08:45:30Z".parse().unwrap();
        let run_limits = make_run_limits(admitted, 1800);
        let input = make_timing_input(run_limits, admitted, None);

        let output = evaluate_timing(&input);
        let receipt = output.receipt.expect("must have receipt");
        assert_eq!(
            receipt.origin, admitted,
            "receipt origin must match run_limits.admitted_at for recovery"
        );
    }

    #[test]
    fn expiry_guard_not_bypassed_at_elapsed_zero_when_expired() {
        let output = evaluate_at_initial_deadline(1, 0);
        assert!(
            !output.emit,
            "expiry guard must not emit notice when deadline <= now, even at elapsed=0"
        );
        assert_eq!(
            output.reason,
            Some(TimingReason::NoChange),
            "expired run must return NoChange, not Initial"
        );
    }

    #[test]
    fn cache_key_prevents_cross_session_leakage() {
        let inv1 = InvocationIdentity::new();

        let key1 = TimingCacheKey::new(
            "cluster-a".to_string(),
            "session-1".to_string(),
            inv1.as_str().to_string(),
        );
        let key2 = TimingCacheKey::new(
            "cluster-a".to_string(),
            "session-2".to_string(),
            inv1.as_str().to_string(),
        );

        assert_ne!(key1, key2, "cache key must include session for isolation");
    }

    #[test]
    fn initial_notice_carries_self_contained_guidance() {
        let admitted = fixed_time();
        let run_limits = make_run_limits(admitted, 3600);
        let input = make_timing_input(run_limits, admitted, None);

        let notice = evaluate_timing(&input).notice_line.expect("notice line");

        // Initial framing is start-of-run, never the periodic kind.
        assert!(
            notice.contains("Timing context at the start of this run"),
            "initial guidance must carry start-of-run framing: {notice}"
        );
        assert!(
            !notice.contains("Periodic timing update"),
            "initial guidance must not be framed as periodic: {notice}"
        );
        // Guidance identifies itself as harness context, not a user request.
        assert!(
            notice.contains("automated timing note from the harness, not a user message"),
            "guidance must say the note is harness-generated: {notice}"
        );
        // Field meanings are restated so the note is understandable alone.
        assert!(
            notice.contains("budget is the run's total allowance (deadline minus start)"),
            "guidance must explain budget: {notice}"
        );
        assert!(
            notice.contains("remaining is time left until the deadline"),
            "guidance must explain remaining: {notice}"
        );
        // Deadline claims stay within verified behavior: the run ends and a
        // retry cannot extend it; no claim about stopping tools mid-flight.
        assert!(
            notice.contains("a retry cannot extend that deadline"),
            "guidance must state retries cannot extend the deadline: {notice}"
        );
        assert!(
            !notice.contains("forcibly stop"),
            "guidance must not claim tools are forcibly stopped: {notice}"
        );
        // Pre-threshold coverage keeps normal pacing, not wrap-up urgency.
        assert!(
            notice.contains("Continue at normal pace"),
            "initial guidance must permit normal pacing: {notice}"
        );
        assert!(
            !notice.contains("Wrap up now"),
            "initial guidance must not demand wrap-up: {notice}"
        );
        // Structure: receipt | fields | guidance.
        assert_eq!(
            notice.matches(TIMING_CONTEXT_SEPARATOR).count(),
            2,
            "notice must separate fields from guidance: {notice}"
        );
    }

    #[test]
    fn unbounded_initial_guidance_explains_no_deadline() {
        let origin = fixed_time();
        let input = TimingInput::unmanaged(origin, origin, InvocationIdentity::new());

        let notice = evaluate_timing(&input).notice_line.expect("notice line");

        assert!(
            notice.contains("This run has no deadline"),
            "unbounded guidance must state there is no deadline: {notice}"
        );
        assert!(
            notice.contains("elapsed is time used since it started"),
            "unbounded initial guidance must explain elapsed explicitly: {notice}"
        );
        assert!(
            notice.contains("Timing context at the start of this run"),
            "unbounded initial guidance must carry start-of-run framing: {notice}"
        );
        assert!(
            !notice.contains("Periodic timing update"),
            "unbounded initial guidance must not be framed as periodic: {notice}"
        );
        assert!(
            notice.contains("every 15 minutes"),
            "unbounded guidance must mention the periodic cadence: {notice}"
        );
        assert!(
            notice.contains("Continue at normal pace"),
            "unbounded initial guidance must permit normal pacing: {notice}"
        );
    }

    #[test]
    fn threshold_50_guidance_warns_about_long_operations() {
        // Committed bucket 2 covers the current bucket so only the 50%
        // threshold fires and the reason-specific prose is exercised.
        let output = evaluate_with_progress(3600, 1800, MilestoneCoverage::INITIAL, 2);
        assert!(output.emit);
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("crossed the halfway mark of its time budget"),
            "50% guidance must report the crossed milestone: {notice}"
        );
        assert!(
            notice.contains("At least half the time budget is used"),
            "50% urgency must reflect a crossed threshold, not an exact figure: {notice}"
        );
        assert!(
            notice.contains("Avoid starting operations that cannot finish before the deadline"),
            "50% guidance must be actionable about long operations: {notice}"
        );
    }

    #[test]
    fn threshold_75_guidance_finish_and_report_tier() {
        // Committed bucket 3 covers the current bucket so only the 75%
        // threshold fires; both the 75% explanation and the 75%-specific
        // urgency must be selected, distinct from the 50% and 90% tiers.
        let output = evaluate_with_progress(
            3600,
            2700,
            MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50,
            3,
        );
        assert!(output.emit);
        assert!(matches!(output.reason, Some(TimingReason::Threshold75)));
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("crossed the 75% mark of its time budget"),
            "75% guidance must report the crossed milestone: {notice}"
        );
        assert!(
            notice.contains("At least 75% of the time budget is used"),
            "75% urgency must reflect the crossed threshold: {notice}"
        );
        assert!(
            notice.contains("report progress now"),
            "75% urgency must carry the finish-and-report action: {notice}"
        );
        // Distinct from neighboring tiers.
        assert!(
            !notice.contains("At least half the time budget is used"),
            "75% must not fall through to 50% urgency: {notice}"
        );
        assert!(
            !notice.contains("At least 90% of the time budget is used"),
            "75% must not jump to 90% urgency: {notice}"
        );
        assert!(
            !notice.contains("Wrap up now"),
            "75% must not carry 90% wrap-up urgency: {notice}"
        );
    }

    #[test]
    fn threshold_90_guidance_demands_wrap_up() {
        let output = evaluate_with_progress(
            3600,
            3240,
            MilestoneCoverage::INITIAL
                | MilestoneCoverage::PERCENT_50
                | MilestoneCoverage::PERCENT_75,
            3,
        );
        assert!(output.emit);
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("crossed the 90% mark of its time budget"),
            "90% guidance must report the crossed milestone: {notice}"
        );
        assert!(
            notice.contains("At least 90% of the time budget is used and 10% or less remains"),
            "90% urgency must hold when the threshold fires late: {notice}"
        );
        assert!(
            notice.contains("Wrap up now"),
            "90% guidance must demand wrapping up: {notice}"
        );
    }

    #[test]
    fn late_threshold_fires_with_at_least_wording() {
        // A 90% warning can fire at 99% elapsed (long tool, missed boundary):
        // the prose must stay true, so it reports a crossed mark and uses
        // "at least"/"or less" instead of exact figures.
        let output = evaluate_with_progress(
            3600,
            3564,
            MilestoneCoverage::INITIAL
                | MilestoneCoverage::PERCENT_50
                | MilestoneCoverage::PERCENT_75,
            3,
        );
        assert!(output.emit);
        assert!(matches!(output.reason, Some(TimingReason::Threshold90)));
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("At least 90% of the time budget is used and 10% or less remains"),
            "late threshold must keep 'at least' wording: {notice}"
        );
        assert!(
            !notice.contains("only 10% remains"),
            "late threshold must not claim exactly 10% remains: {notice}"
        );
    }

    #[test]
    fn coalesced_above_90_keeps_strongest_urgency() {
        // Periodic and all three thresholds coalesce into one notice whose
        // cumulative coverage includes 90%; the guidance must not fall back
        // to generic progress wording.
        let output = evaluate_with_progress(3600, 3240, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        assert!(matches!(output.reason, Some(TimingReason::Coalesced)));
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("combines multiple timing updates"),
            "coalesced guidance must say it combines updates: {notice}"
        );
        assert!(
            !notice.contains("Periodic timing update"),
            "coalesced guidance must not be framed as periodic: {notice}"
        );
        assert!(
            !notice.contains("Timing context at the start of this run"),
            "coalesced guidance must not be framed as initial: {notice}"
        );
        assert!(
            notice.contains("At least 90% of the time budget is used"),
            "coalesced guidance must reflect the strongest covered milestone: {notice}"
        );
        assert!(
            notice.contains("Wrap up now"),
            "coalesced guidance must deliver wrap-up urgency: {notice}"
        );
    }

    #[test]
    fn post_threshold_periodic_keeps_wrap_up_urgency() {
        // A periodic bucket notice emitted after 90% is already covered must
        // not revert to normal-pace wording; coverage is cumulative.
        let output = evaluate_with_progress(
            7200,
            3700,
            MilestoneCoverage::INITIAL
                | MilestoneCoverage::PERCENT_50
                | MilestoneCoverage::PERCENT_75
                | MilestoneCoverage::PERCENT_90,
            3,
        );
        assert!(output.emit);
        assert!(matches!(
            output.reason,
            Some(TimingReason::PeriodicBucket(4))
        ));
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("Periodic timing update"),
            "kind sentence must report the periodic trigger: {notice}"
        );
        assert!(
            !notice.contains("Timing context at the start of this run"),
            "periodic guidance must not be framed as initial: {notice}"
        );
        assert!(
            !notice.contains("combines multiple timing updates"),
            "periodic guidance must not be framed as coalesced: {notice}"
        );
        assert!(
            notice.contains("At least 90% of the time budget is used"),
            "post-90% periodic urgency must stay at wrap-up level: {notice}"
        );
        assert!(
            notice.contains("Wrap up now"),
            "post-90% periodic guidance must demand wrapping up: {notice}"
        );
        assert!(
            !notice.contains("Continue at normal pace"),
            "post-90% periodic guidance must not revert to normal pacing: {notice}"
        );
    }

    #[test]
    fn periodic_bounded_guidance_keeps_fields_machine_parsed() {
        let admitted = fixed_time();
        let now = admitted + chrono::Duration::seconds(900);
        let output = evaluate_with_progress(7200, 900, MilestoneCoverage::INITIAL, 0);
        assert!(output.emit);
        let notice = output.notice_line.expect("notice line");

        // Guidance for a bounded periodic notice explains both fields.
        assert!(
            notice.contains("Periodic timing update"),
            "periodic guidance must be framed as periodic, not initial: {notice}"
        );
        assert!(
            !notice.contains("Timing context at the start of this run"),
            "periodic guidance must not be framed as initial: {notice}"
        );
        assert!(
            notice.contains("elapsed is time used since start"),
            "periodic guidance must explain elapsed: {notice}"
        );
        assert!(
            notice.contains("remaining is time left until the deadline"),
            "periodic guidance must explain remaining: {notice}"
        );
        // Pre-threshold periodic coverage keeps normal pacing.
        assert!(
            notice.contains("Continue at normal pace"),
            "pre-threshold periodic guidance must permit normal pacing: {notice}"
        );

        // The fields segment before the second separator stays compact-
        // display parseable and carries no prose.
        let after_receipt = notice
            .strip_prefix(TIMING_HEADER)
            .and_then(|rest| rest.split_once(TIMING_CONTEXT_SEPARATOR))
            .map(|(_, fields_and_guidance)| fields_and_guidance)
            .expect("first separator");
        let (fields, guidance) = after_receipt
            .split_once(TIMING_CONTEXT_SEPARATOR)
            .expect("second separator");
        assert_eq!(
            fields,
            format!(
                "start: {}, current: {}, elapsed: 15m, deadline: {}, remaining: 105m, budget: 120m",
                admitted.format("%Y-%m-%dT%H:%M:%SZ"),
                now.format("%Y-%m-%dT%H:%M:%SZ"),
                (admitted + chrono::Duration::seconds(7200)).format("%Y-%m-%dT%H:%M:%SZ"),
            ),
            "fields segment must stay prose-free: {notice}"
        );
        assert!(
            guidance.starts_with("This is an automated timing note"),
            "guidance must start with the identity sentence: {notice}"
        );
        assert_eq!(
            notice.matches(TIMING_CONTEXT_SEPARATOR).count(),
            2,
            "no stray separators in guidance: {notice}"
        );
    }

    #[test]
    fn unbounded_periodic_guidance_mentions_elapsed_only() {
        let origin = fixed_time();
        let mut input = TimingInput::unmanaged(origin, origin, InvocationIdentity::new());
        input.committed_progress = evaluate_timing(&input).receipt;
        input.now = origin + chrono::Duration::seconds(900);

        let notice = evaluate_timing(&input).notice_line.expect("notice line");
        assert!(
            notice.contains("This run has no deadline"),
            "unbounded periodic guidance must restate the missing deadline: {notice}"
        );
        // Distinguish periodic framing from initial and coalesced unbounded
        // arms, which share the no-deadline language.
        assert!(
            !notice.contains("Timing context at the start of this run"),
            "unbounded periodic guidance must not be framed as initial: {notice}"
        );
        assert!(
            !notice.contains("combines multiple timing updates"),
            "unbounded periodic guidance must not be framed as coalesced: {notice}"
        );
        assert!(
            !notice.contains("remaining:"),
            "unbounded notice must not carry a remaining field: {notice}"
        );
    }

    #[test]
    fn unbounded_coalesced_guidance_combines_updates() {
        // An unmanaged run first evaluated after 15 minutes coalesces the
        // initial notice with the first elapsed bucket; the combined-update
        // framing must hold even though no previous note exists.
        let origin = fixed_time();
        let now = origin + chrono::Duration::seconds(900);
        let input = TimingInput::unmanaged(now, origin, InvocationIdentity::new());

        let output = evaluate_timing(&input);
        assert!(output.emit);
        assert!(matches!(output.reason, Some(TimingReason::Coalesced)));
        let notice = output.notice_line.expect("notice line");
        assert!(
            notice.contains("combines multiple timing updates"),
            "unbounded coalesced guidance must say it combines updates: {notice}"
        );
        assert!(
            !notice.contains("Timing context at the start of this run"),
            "unbounded coalesced guidance must not be framed as initial: {notice}"
        );
        assert!(
            !notice.contains("Periodic timing update"),
            "unbounded coalesced guidance must not be framed as periodic: {notice}"
        );
        assert!(
            notice.contains("This run has no deadline"),
            "unbounded coalesced guidance must restate the missing deadline: {notice}"
        );
        assert!(
            !notice.contains("remaining:"),
            "unbounded notice must not carry a remaining field: {notice}"
        );
    }
}
