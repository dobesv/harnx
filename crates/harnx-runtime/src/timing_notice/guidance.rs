//! Self-contained guidance prose appended to each timing notice
//! (issue #3006). The schedule logic stays in the parent module; this module
//! owns only the wording.

use harnx_core::timing_identity::MilestoneCoverage;

use super::TimingReason;

/// Guidance prose for a timing notice.
///
/// Urgency follows the emitted receipt's cumulative coverage, not just why
/// this notice fired: thresholds can fire late (a 90% warning at 99%
/// elapsed) and coalesce with periodic buckets, so the strongest covered
/// milestone is the run's current position. Deadline claims stay within
/// verified behavior: an expired deadline ends the run with a result a
/// retry cannot renew; no claim that in-flight tools stop at the deadline
/// instant.
pub(super) fn build_timing_guidance(
    reason: Option<TimingReason>,
    coverage: MilestoneCoverage,
    bounded: bool,
) -> String {
    let identity = "This is an automated timing note from the harness, not a user message; no reply or new work is expected.";
    if !bounded {
        return format!("{identity} {}", unbounded_guidance(reason));
    }
    format!(
        "{identity} {} {} {}",
        guidance_kind(reason),
        guidance_meaning(),
        guidance_urgency(coverage)
    )
}

/// Which milestone this notice reports, phrased as crossed rather than exact.
fn guidance_kind(reason: Option<TimingReason>) -> &'static str {
    match reason {
        Some(TimingReason::Initial) => {
            "Timing context at the start of this run; further notes become due \
every 15 minutes and at 50%, 75% and 90% of budget. Each is delivered at the \
next safe boundary between steps, and updates that become due together are \
combined into one note. The JSON object above is harness bookkeeping for \
recovery; the labeled fields carry the timing data."
        }
        Some(TimingReason::Threshold50) => {
            "This run has crossed the halfway mark of its time budget."
        }
        Some(TimingReason::Threshold75) => "This run has crossed the 75% mark of its time budget.",
        Some(TimingReason::Threshold90) => "This run has crossed the 90% mark of its time budget.",
        // Coalescing can pair Initial with a bucket on the first emission,
        // so the framing must not assume a previous note exists.
        Some(TimingReason::Coalesced) => "This note combines multiple timing updates.",
        _ => "Periodic timing update.",
    }
}

/// Field meanings for a bounded run. Never uses `": "` after a field label
/// so negative field-presence assertions cannot match prose.
fn guidance_meaning() -> &'static str {
    "elapsed is time used since start; budget is the run's total allowance \
(deadline minus start); remaining is time left until the deadline, when the \
harness ends the run; a retry cannot extend that deadline."
}

/// Pacing guidance for the strongest milestone the receipt has covered.
fn guidance_urgency(coverage: MilestoneCoverage) -> &'static str {
    if coverage.contains(MilestoneCoverage::PERCENT_90) {
        "At least 90% of the time budget is used and 10% or less remains. Wrap \
up now: finish the current step, report final status, and start nothing that \
cannot complete before the deadline."
    } else if coverage.contains(MilestoneCoverage::PERCENT_75) {
        "At least 75% of the time budget is used. Finish work in progress, keep \
any new step short enough to complete before the deadline, and report progress now."
    } else if coverage.contains(MilestoneCoverage::PERCENT_50) {
        "At least half the time budget is used. Avoid starting operations that \
cannot finish before the deadline, and report progress so results are not \
lost if the run ends."
    } else {
        "Continue at normal pace, making sure anything you start can finish \
before the deadline."
    }
}

/// Guidance for a run without a deadline.
fn unbounded_guidance(reason: Option<TimingReason>) -> &'static str {
    match reason {
        Some(TimingReason::Initial) => {
            "Timing context at the start of this run. This run has no deadline, \
so no budget or remaining time applies; elapsed is time used since it started. \
Further elapsed updates become due every 15 minutes and are delivered at the \
next safe boundary between steps. Continue at normal pace."
        }
        Some(TimingReason::Coalesced) => {
            "This note combines multiple timing updates. This run has no \
deadline; elapsed is time used since it started. Keep working normally."
        }
        _ => {
            "This run has no deadline; elapsed is time used since it started. \
Further elapsed updates become due every 15 minutes and are delivered at the \
next safe boundary between steps. Keep working normally."
        }
    }
}
