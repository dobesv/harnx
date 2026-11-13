//! Reserved versioned timing identity for durable timing notices.
//!
//! Timing notices carry structured identity so workers can recognize them
//! without parsing human-readable prose, and so recovery can identify
//! which milestones were already emitted and recover timing origin.
//!
//! # Design
//!
//! - **Format prefix**: `[Runtime note] ⏱ ` followed by a versioned header
//! - **Structured receipt**: JSON object with origin, invocation identity, milestones
//! - **Classifier**: Pure predicate that validates header structure after prefix
//! - **Versioned**: Schema can evolve; `v1` suffix provides migration path
//! - **Recoverable**: Receipt contains all data needed for raw log recovery
//!
//! # Classifier behavior
//!
//! The classifier is content-based: it recognizes the exact versioned header
//! only when followed by a parseable receipt, and ignores trailing prose. A
//! bare prefix, malformed receipt, or unversioned runtime-note marker is not
//! classified as a timing notice. Conversely, matching user-authored text is
//! classified the same way as a runtime-authored notice; the receipt is not
//! authenticated and does not establish message provenance. This module does
//! not reserve the prefix at prompt admission or reject or rewrite prompts that
//! contain it.
//!
//! Callers use this predicate to keep recognized timing notices out of
//! executable-prompt classification and to recover timing state from receipts.
//! Receipts record origin, invocation identity, covered milestones, and highest
//! elapsed bucket.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Prefix used in the serialized format for runtime timing notices.
///
/// The prefix alone is not enough to classify a message. The classifier also
/// requires the versioned header and a parseable receipt, and does not verify
/// who authored the text. This format marker is not enforced by prompt admission.
pub const TIMING_NOTE_PREFIX: &str = "[Runtime note] ⏱ ";

/// Current timing identity schema version.
pub const TIMING_SCHEMA_VERSION: &str = "v1";

/// Prefix of the structured message id carried by durable timing notices.
/// Reserved: callers may not admit prompts under this prefix, or the notices
/// would be indistinguishable from executable prompts in the log.
pub const TIMING_MESSAGE_ID_PREFIX: &str = "timing-v1-";

/// Combined versioned header for timing notices.
pub const TIMING_HEADER: &str = "[Runtime note] ⏱ v1 ";

/// Separator between JSON receipt and human-readable timing context.
/// Allows classifier to parse receipt while preserving prose description.
pub const TIMING_CONTEXT_SEPARATOR: &str = " | ";

/// Bitmask for timing milestones that have been covered.
///
/// Each milestone is recorded once; the bitmask tracks which thresholds
/// have already been emitted. This enables coalescing missed milestones
/// and avoiding duplicate emissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneCoverage(u32);

impl MilestoneCoverage {
    /// No milestones covered yet.
    pub const NONE: Self = Self(0);

    /// Initial timing context has been emitted.
    pub const INITIAL: Self = Self(1 << 0);

    /// 50% deadline threshold has been emitted.
    pub const PERCENT_50: Self = Self(1 << 1);

    /// 75% deadline threshold has been emitted.
    pub const PERCENT_75: Self = Self(1 << 2);

    /// 90% deadline threshold has been emitted.
    pub const PERCENT_90: Self = Self(1 << 3);

    /// All deadline percentage milestones.
    pub const ALL_PERCENTAGES: Self = Self(0b1110);

    /// Create coverage from a raw bitmask.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Get the raw bitmask.
    pub fn bits(&self) -> u32 {
        self.0
    }

    /// Check if a milestone is covered.
    pub fn contains(&self, milestone: Self) -> bool {
        self.0 & milestone.0 != 0
    }

    /// Add a milestone to coverage.
    pub fn add(&mut self, milestone: Self) {
        self.0 |= milestone.0;
    }

    /// Return which percentage milestones are covered, regardless of elapsed buckets.
    pub fn percentage_coverage(&self) -> Self {
        Self(self.0 & Self::ALL_PERCENTAGES.0)
    }
}

impl Default for MilestoneCoverage {
    fn default() -> Self {
        Self::NONE
    }
}

impl std::ops::BitOr for MilestoneCoverage {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for MilestoneCoverage {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Structured receipt for a timing notice.
///
/// Encoded in the timing notice itself so workers can recover progress
/// from raw log entries without parsing human-readable text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingReceipt {
    /// Schema version for future migration.
    pub version: String,
    /// Run origin timestamp (admitted_at or fallback for unmanaged).
    pub origin: DateTime<Utc>,
    /// Invocation identity for this run.
    pub invocation_id: String,
    /// Milestones that have been covered.
    pub coverage: MilestoneCoverage,
    /// Highest 900-second elapsed bucket emitted (0 = none yet).
    /// For a run with elapsed 1847s, this would be 2 (1800-1899 bucket).
    pub highest_elapsed_bucket: u32,
}

impl TimingReceipt {
    /// Create a new receipt with the current schema version.
    pub fn new(
        origin: DateTime<Utc>,
        invocation_id: String,
        coverage: MilestoneCoverage,
        highest_elapsed_bucket: u32,
    ) -> Self {
        Self {
            version: TIMING_SCHEMA_VERSION.to_string(),
            origin,
            invocation_id,
            coverage,
            highest_elapsed_bucket,
        }
    }

    /// Create an initial receipt (initial notice emitted, no elapsed buckets).
    pub fn initial(origin: DateTime<Utc>, invocation_id: String) -> Self {
        Self::new(origin, invocation_id, MilestoneCoverage::INITIAL, 0)
    }

    /// Serialize to JSON for embedding in timing notice.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse from JSON string.
    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    /// Parse from a complete timing notice line.
    ///
    /// The line format is:
    /// `[Runtime note] ⏱ v1 <JSON receipt> | <human-readable context>`
    ///
    /// Returns `None` if the line doesn't have the versioned format header
    /// or the receipt JSON is malformed. Trailing prose is ignored.
    pub fn from_notice_line(line: &str) -> Option<Self> {
        let after_prefix = line.strip_prefix(TIMING_HEADER)?;
        // Find the separator or end of line to isolate the JSON
        let json_part = if let Some(sep_pos) = after_prefix.find(TIMING_CONTEXT_SEPARATOR) {
            &after_prefix[..sep_pos]
        } else {
            // For backwards compatibility, treat rest as JSON (old format)
            after_prefix
        };
        Self::from_json(json_part.trim())
    }

    /// Extract the timing context separator position from a notice line.
    /// Returns None if no separator found (old format or malformed).
    pub fn context_start(line: &str) -> Option<usize> {
        let after_prefix = line.strip_prefix(TIMING_HEADER)?;
        after_prefix
            .find(TIMING_CONTEXT_SEPARATOR)
            .map(|pos| TIMING_HEADER.len() + pos + TIMING_CONTEXT_SEPARATOR.len())
    }
}

/// Check if a message text is a timing notice.
///
/// This classifier checks for the exact versioned header and a parseable
/// receipt, ignoring trailing prose. A bare prefix or malformed receipt is
/// not classified. Matching text is classified regardless of whether it came
/// from the runtime or a user; this predicate does not authenticate provenance.
/// Callers that use this predicate treat recognized messages as timing context
/// rather than executable prompts.
pub fn is_timing_notice(text: &str) -> bool {
    if !text.starts_with(TIMING_HEADER) {
        return false;
    }
    // Validate receipt structure (ignores trailing prose)
    TimingReceipt::from_notice_line(text).is_some()
}

/// Compact one-line display form of a timing notice, for UI rendering only.
///
/// Turns a raw notice line into something like
/// `⏱ 526m 40s elapsed · 913m 19s left of 1440m · 90% threshold passed`.
/// Duration values are reused verbatim from the notice's human-readable
/// context, so the compact form never disagrees with the model-facing text.
/// Returns `None` when the line is not a parseable timing notice, or when the
/// line carries no readable context (old receipt-only lines fall back to the
/// caller's generic runtime-note handling).
///
/// This reads the note text, which the admission-side checks in this module
/// deliberately never do. That is fine here: rendering makes no admission or
/// execution decision, so a user-authored lookalike only loses some display
/// verbosity and nothing about the message's executability changes.
pub fn compact_timing_display(line: &str) -> Option<String> {
    let receipt = TimingReceipt::from_notice_line(line)?;
    let context = TimingReceipt::context_start(line).map_or("", |start| &line[start..]);
    let fields = TimingDisplayFields::parse(context);
    let mut parts: Vec<String> = Vec::new();
    if let Some(elapsed) = fields.elapsed {
        parts.push(format!("⏱ {elapsed} elapsed"));
    }
    if let (Some(remaining), Some(budget)) = (fields.remaining, fields.budget) {
        parts.push(format!("{remaining} left of {budget}"));
    }
    // Milestone coverage is cumulative, so a covered threshold stays true for
    // every later notice. Surface the highest covered one: the threshold
    // emission is the notice's "time is running low" signal, and without it
    // the compact form would hide it behind plain elapsed/remaining numbers.
    if let Some(percent) = covered_threshold_percent(&receipt.coverage) {
        parts.push(format!("{percent}% threshold passed"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" · "))
}

/// Highest covered deadline threshold, as a percentage of the budget.
fn covered_threshold_percent(coverage: &MilestoneCoverage) -> Option<u32> {
    if coverage.contains(MilestoneCoverage::PERCENT_90) {
        Some(90)
    } else if coverage.contains(MilestoneCoverage::PERCENT_75) {
        Some(75)
    } else if coverage.contains(MilestoneCoverage::PERCENT_50) {
        Some(50)
    } else {
        None
    }
}

/// The display-relevant fields of a timing notice's human-readable context.
struct TimingDisplayFields<'a> {
    elapsed: Option<&'a str>,
    remaining: Option<&'a str>,
    budget: Option<&'a str>,
}

impl<'a> TimingDisplayFields<'a> {
    /// Parse the `label: value` pairs of the notice context.
    ///
    /// Timestamp values contain colons but never `": "`, and duration values
    /// contain neither, so splitting each comma-separated part at the first
    /// `": "` cannot cut into a value.
    fn parse(context: &'a str) -> Self {
        let mut fields = Self {
            elapsed: None,
            remaining: None,
            budget: None,
        };
        for (label, value) in context.split(", ").filter_map(|part| part.split_once(": ")) {
            match label {
                "elapsed" => fields.elapsed = Some(value),
                "remaining" => fields.remaining = Some(value),
                "budget" => fields.budget = Some(value),
                _ => {}
            }
        }
        fields
    }
}

/// Cache key for timing state: (storage identifier, session id, invocation id).
///
/// Invocation cache is keyed by storage/session plus invocation identity
/// to support multi-tenant scenarios and prevent leakage between sessions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimingCacheKey {
    /// Storage identifier (e.g., cluster or local identifier).
    pub storage: String,
    /// Session identity (session ID string).
    pub session_id: String,
    /// Invocation identity for this run.
    pub invocation_id: String,
}

impl TimingCacheKey {
    /// Create a new cache key.
    pub fn new(storage: String, session_id: String, invocation_id: String) -> Self {
        Self {
            storage,
            session_id,
            invocation_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_not_timing_notice() {
        assert!(!is_timing_notice(""));
        assert!(!is_timing_notice(" "));
        assert!(!is_timing_notice("[Runtime note]"));
    }

    #[test]
    fn user_prose_with_timing_words_is_not_timing_notice() {
        assert!(!is_timing_notice("Time remaining: 5 minutes"));
        assert!(!is_timing_notice("You have 50% of your budget remaining"));
        assert!(!is_timing_notice(
            "[Runtime note] This is a regular runtime note"
        ));
        assert!(!is_timing_notice(
            "[Runtime note] ⏱ this is not a valid receipt"
        ));
    }

    #[test]
    fn valid_timing_notice_is_recognized() {
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::initial(origin, "inv-123".to_string());
        let notice = format!(
            "{}{}{}Elapsed: 0s, Budget: 1h",
            TIMING_HEADER,
            receipt.to_json(),
            TIMING_CONTEXT_SEPARATOR
        );
        assert!(is_timing_notice(&notice));
    }

    #[test]
    fn receipt_roundtrips_through_json() {
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::new(
            origin,
            "inv-abc".to_string(),
            MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50,
            3,
        );
        let json = receipt.to_json();
        let parsed = TimingReceipt::from_json(&json).expect("roundtrip");
        assert_eq!(receipt, parsed);
        assert_eq!(receipt.origin, parsed.origin);
        assert_eq!(receipt.invocation_id, parsed.invocation_id);
    }

    #[test]
    fn receipt_parses_from_notice_line_with_trailing_context() {
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::new(
            origin,
            "inv-xyz".to_string(),
            MilestoneCoverage::INITIAL
                | MilestoneCoverage::PERCENT_50
                | MilestoneCoverage::PERCENT_75,
            5,
        );
        // Notice with trailing prose context
        let notice = format!(
            "{}{}{}Elapsed: 27m, Remaining: 33m, Budget: 1h",
            TIMING_HEADER,
            receipt.to_json(),
            TIMING_CONTEXT_SEPARATOR
        );
        let parsed = TimingReceipt::from_notice_line(&notice).expect("parse");
        assert_eq!(receipt, parsed);
        assert_eq!(receipt.origin, parsed.origin);
    }

    #[test]
    fn receipt_parses_from_notice_line_without_context() {
        // Old format without separator should still work for backcompat
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::initial(origin, "inv-old".to_string());
        let notice = format!("{}{}", TIMING_HEADER, receipt.to_json());
        let parsed = TimingReceipt::from_notice_line(&notice).expect("parse");
        assert_eq!(receipt, parsed);
    }

    #[test]
    fn invalid_receipt_json_returns_none() {
        let line = format!("{}not valid json", TIMING_HEADER);
        assert!(TimingReceipt::from_notice_line(&line).is_none());
    }

    #[test]
    fn compact_display_requires_receipt_and_context() {
        // Malformed receipt: not a timing notice at all.
        let line = format!("{}not valid json | elapsed: 5m", TIMING_HEADER);
        assert!(compact_timing_display(&line).is_none());
        // Old receipt-only format without the context separator has nothing
        // to show; the caller falls back to generic runtime-note handling.
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::initial(origin, "inv-old".to_string());
        let bare = format!("{}{}", TIMING_HEADER, receipt.to_json());
        assert!(compact_timing_display(&bare).is_none());
        // Full line compacts with verbatim duration values.
        let notice = format!(
            "{}{}{}start: 2024-01-15T12:00:00Z, current: 2024-01-15T12:05:00Z, elapsed: 5m, \
deadline: 2024-01-15T13:00:00Z, remaining: 55m, budget: 1h",
            TIMING_HEADER,
            receipt.to_json(),
            TIMING_CONTEXT_SEPARATOR
        );
        assert_eq!(
            compact_timing_display(&notice).as_deref(),
            Some("⏱ 5m elapsed · 55m left of 1h")
        );
    }

    #[test]
    fn coverage_bitmask_operations() {
        let mut coverage = MilestoneCoverage::NONE;
        assert!(!coverage.contains(MilestoneCoverage::INITIAL));
        assert!(!coverage.contains(MilestoneCoverage::PERCENT_50));

        coverage.add(MilestoneCoverage::INITIAL);
        assert!(coverage.contains(MilestoneCoverage::INITIAL));
        assert!(!coverage.contains(MilestoneCoverage::PERCENT_50));

        coverage.add(MilestoneCoverage::PERCENT_50);
        assert!(coverage.contains(MilestoneCoverage::INITIAL));
        assert!(coverage.contains(MilestoneCoverage::PERCENT_50));
        assert!(!coverage.contains(MilestoneCoverage::PERCENT_90));

        // Adding twice is idempotent
        coverage.add(MilestoneCoverage::PERCENT_50);
        assert!(coverage.contains(MilestoneCoverage::PERCENT_50));
    }

    #[test]
    fn coverage_percentage_mask() {
        let coverage = MilestoneCoverage::INITIAL | MilestoneCoverage::PERCENT_50;
        let pct_only = coverage.percentage_coverage();
        assert!(!pct_only.contains(MilestoneCoverage::INITIAL));
        assert!(pct_only.contains(MilestoneCoverage::PERCENT_50));

        // Coverage with no percentage milestones
        let initial_only = MilestoneCoverage::INITIAL;
        assert!(!initial_only
            .percentage_coverage()
            .contains(MilestoneCoverage::PERCENT_50));
    }

    #[test]
    fn constants_have_correct_values() {
        assert_eq!(MilestoneCoverage::NONE.bits(), 0);
        assert_eq!(MilestoneCoverage::INITIAL.bits(), 1);
        assert_eq!(MilestoneCoverage::PERCENT_50.bits(), 2);
        assert_eq!(MilestoneCoverage::PERCENT_75.bits(), 4);
        assert_eq!(MilestoneCoverage::PERCENT_90.bits(), 8);
        assert_eq!(MilestoneCoverage::ALL_PERCENTAGES.bits(), 14);
    }

    #[test]
    fn cache_key_includes_all_scoping() {
        let key1 = TimingCacheKey::new(
            "cluster-a".to_string(),
            "session-1".to_string(),
            "inv-1".to_string(),
        );
        let key2 = TimingCacheKey::new(
            "cluster-a".to_string(),
            "session-1".to_string(),
            "inv-2".to_string(),
        );
        let key3 = TimingCacheKey::new(
            "cluster-b".to_string(),
            "session-1".to_string(),
            "inv-1".to_string(),
        );

        // Different invocation_ids are different keys
        assert_ne!(key1, key2);
        // Different storage identifiers are different keys
        assert_ne!(key1, key3);
    }

    #[test]
    fn receipt_contains_origin_for_recovery() {
        let origin: DateTime<Utc> = "2024-01-15T08:30:00Z".parse().unwrap();
        let invocation = "test-invocation-123";
        let receipt = TimingReceipt::initial(origin, invocation.to_string());

        // Receipt must contain origin and invocation_id for raw log recovery
        assert_eq!(receipt.origin, origin);
        assert_eq!(receipt.invocation_id, invocation);

        // Verify serialization preserves these fields
        let json = receipt.to_json();
        assert!(json.contains("origin"), "JSON must contain origin field");
        assert!(
            json.contains("invocation_id"),
            "JSON must contain invocation_id field"
        );
        assert!(
            json.contains(invocation),
            "JSON must contain invocation_id value"
        );
    }

    #[test]
    fn classifier_ignores_trailing_prose_after_separator() {
        let origin: DateTime<Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
        let receipt = TimingReceipt::initial(origin, "inv-789".to_string());

        // Notice with human-readable context after JSON
        let notice = format!(
            "{}{}{}Time remaining: 30 minutes. Please wrap up your work.",
            TIMING_HEADER,
            receipt.to_json(),
            TIMING_CONTEXT_SEPARATOR
        );

        assert!(
            is_timing_notice(&notice),
            "Classifier must accept notices with trailing prose"
        );

        // Verify we can still parse the receipt
        let parsed = TimingReceipt::from_notice_line(&notice).expect("parse with prose");
        assert_eq!(parsed.origin, origin);
        assert_eq!(parsed.invocation_id, "inv-789");
    }
}
