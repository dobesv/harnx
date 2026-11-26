//! Effective-activity age buckets and snapshot-estimated retention
//! eligibility, shared by the cluster and agent reports.
use super::report::{AgeCounts, RetentionReport};
use super::snapshot::SessionInput;
use chrono::{DateTime, Utc};
use std::collections::HashSet;

/// Age bucket upper bounds, in days: the issue's `[1d,7d)` and `[7d,30d)`
/// boundaries. The `[0,1d)` and `[30d,∞)` ends are open.
const WEEK_DAYS: i64 = 7;
const MONTH_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// Age bucket boundaries, in days: `[0,1d)`, `[1d,7d)`, `[7d,30d)`,
/// `[30d,∞)`. A timestamp in the future lands in the newest bucket rather
/// than producing a negative age.
pub(super) fn age_bucket(now: DateTime<Utc>, activity: DateTime<Utc>) -> AgeBucket {
    let age = now.signed_duration_since(activity);
    if age < chrono::Duration::days(1) {
        AgeBucket::Under1Day
    } else if age < chrono::Duration::days(WEEK_DAYS) {
        AgeBucket::OneTo7Days
    } else if age < chrono::Duration::days(MONTH_DAYS) {
        AgeBucket::SevenTo30Days
    } else {
        AgeBucket::Over30Days
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgeBucket {
    Under1Day,
    OneTo7Days,
    SevenTo30Days,
    Over30Days,
}

impl AgeCounts {
    pub(super) fn record(&mut self, bucket: AgeBucket) {
        match bucket {
            AgeBucket::Under1Day => self.under_1_day += 1,
            AgeBucket::OneTo7Days => self.one_to_7_days += 1,
            AgeBucket::SevenTo30Days => self.seven_to_30_days += 1,
            AgeBucket::Over30Days => self.over_30_days += 1,
        }
    }
}

fn session_lease_key(storage_key: &str) -> String {
    crate::nats_lease::NatsLeaseConfig::default().key_for_session(storage_key)
}

fn a2a_lease_key(storage_key: &str) -> String {
    format!("sessions/{storage_key}/a2a/lock")
}

/// Snapshot-estimated retention eligibility. The cutoff comparison is strict,
/// matching the periodic cleanup's candidate threshold, and `age_expired =
/// held + eligible` with each candidate counted once.
pub(crate) fn retention_report<'a>(
    now: DateTime<Utc>,
    sessions: impl IntoIterator<Item = &'a SessionInput>,
    live_lease_keys: &HashSet<String>,
    retention_days: Option<u64>,
) -> RetentionReport {
    let mut report = RetentionReport {
        configured_days: retention_days,
        enabled: retention_days.is_some_and(|days| days > 0),
        ..RetentionReport::default()
    };
    let Some(days) = retention_days.filter(|days| *days > 0) else {
        return report;
    };
    let threshold = now
        .timestamp()
        .saturating_sub(days.saturating_mul(SECONDS_PER_DAY as u64) as i64);
    for session in sessions {
        if session.effective_activity.timestamp() >= threshold {
            continue;
        }
        report.age_expired += 1;
        let session_lease = live_lease_keys.contains(&session_lease_key(&session.storage_key));
        let a2a_lease = live_lease_keys.contains(&a2a_lease_key(&session.storage_key));
        let held = session_lease || a2a_lease || session.a2a_holds.any();
        report.held += u64::from(held);
        report.eligible += u64::from(!held);
        report.held_session_lease += u64::from(session_lease);
        report.held_a2a_lease += u64::from(a2a_lease);
        report.held_unresolved_context += u64::from(session.a2a_holds.unresolved_context);
        report.held_unsettled_first_reservation +=
            u64::from(session.a2a_holds.unsettled_first_reservation);
    }
    report
}
