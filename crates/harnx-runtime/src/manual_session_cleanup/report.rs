//! Report and option types for the manual expired-session cleanup pass, and
//! the pure parts of candidate selection the pass shares with the worker
//! cleanup's age semantics. The `harnx delete expired-sessions` CLI (T3)
//! serializes these; the runtime pass in the module root fills them in.
use crate::nats_session_metadata::{ListedSession, SessionAgentSource};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// Default bound on cascade admissions, matching the orphan-CLI precedent:
/// one manual pass touches at most this many sessions, including attempts
/// that fail or get skipped late, so `--max 1` can never affect a second one.
pub const DEFAULT_MANUAL_CLEANUP_MAX: usize = 100;

/// Resolved options for one manual cleanup invocation. `days` is nonzero (the
/// CLI resolves `--days` or the configured retention before calling);
/// `agent` is an exact named-agent match and never selects inline sessions;
/// `max` bounds cascade admissions, not successful deletions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualCleanupOptions {
    pub days: u64,
    pub agent: Option<String>,
    pub max: usize,
    pub dry_run: bool,
}

impl Default for ManualCleanupOptions {
    fn default() -> Self {
        Self {
            days: 0,
            agent: None,
            max: DEFAULT_MANUAL_CLEANUP_MAX,
            dry_run: false,
        }
    }
}

impl ManualCleanupOptions {
    /// Validation the CLI performs before connecting; the runtime entry
    /// point repeats it so no caller can bypass it.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.days == 0 {
            anyhow::bail!(
                "manual expired-session cleanup requires nonzero retention days \
                 (--days or a configured cleanup_remote_sessions_days > 0)"
            );
        }
        if self.max == 0 {
            anyhow::bail!("manual expired-session cleanup requires a nonzero --max");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualCleanupMode {
    DryRun,
    Delete,
}

/// One candidate's final disposition. `late` marks a decision that raced a
/// live change after the pass admitted the session; `partial_possible` marks
/// outcomes where the deletion cascade may have removed some session state
/// before it failed — the report never presents those as clean no-op skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCleanupStatus {
    /// The deletion cascade completed and purged the canonical record.
    Deleted,
    /// Dry-run: the read-only guards passed; a real pass would delete.
    WouldDelete,
    /// A known live hold (fresh activity, session or A2A lease, A2A
    /// admission state, or a late fence refusal). Never an error.
    SkippedActive,
    /// The canonical record's revision changed or vanished. Pre-mutation
    /// skips did not mutate anything; a late final-CAS rejection did.
    SkippedRevisionChanged,
    /// The deletion cascade failed after admission; effects may exist.
    Failed,
    /// A read-only guard errored (e.g. malformed A2A state the pass must
    /// inspect). Nothing was mutated for this session.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCleanupResult {
    pub storage_key: String,
    pub status: SessionCleanupStatus,
    /// Pre-delete session-stream bytes; zero when no stream exists. Preview
    /// selections and successful real deletions carry it; for real runs it
    /// is the size the cascade was about to reclaim, not all reclaimed disk.
    pub stream_bytes: u64,
    pub late: bool,
    pub partial_possible: bool,
    pub missing_metadata: bool,
    pub detail: Option<String>,
}

impl SessionCleanupResult {
    /// Constructor taking the required fields; outcome sites fill the rest
    /// by direct field assignment — each site sets exactly the flags it
    /// knows about, so no builder layer is worth its weight.
    pub(crate) fn new(storage_key: String, status: SessionCleanupStatus) -> Self {
        Self {
            storage_key,
            status,
            stream_bytes: 0,
            late: false,
            partial_possible: false,
            missing_metadata: false,
            detail: None,
        }
    }

    /// Detail from an error chain, formatted with its causes.
    pub(crate) fn error_detail(error: &anyhow::Error) -> Option<String> {
        Some(format!("{error:#}"))
    }
}

/// Always-present summary the CLI prints before exit-status details.
/// `scanned` matches the worker's candidate count: every age-expired
/// candidate in the selected scope from the initial snapshot, including
/// ones the cap or a hold kept the pass from attempting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualCleanupReport {
    pub mode: ManualCleanupMode,
    pub days: u64,
    pub agent_filter: Option<String>,
    pub max: usize,
    pub scanned: usize,
    pub attempted: usize,
    pub deleted: usize,
    pub skipped_active: usize,
    pub skipped_revision_changed: usize,
    pub errors: usize,
    /// True when the admission bound stopped the pass before it examined
    /// every candidate. Not a remaining-backlog estimate.
    pub bound_reached: bool,
    /// Dry-run only: how many capped eligible selections a real pass would
    /// have been admitted to delete.
    pub would_delete: usize,
    /// Session-stream bytes reclaimed by successful cascades only.
    pub stream_bytes_deleted: u64,
    /// The GC lease was lost (or could not be revalidated) mid-pass; no new
    /// candidate was admitted after that point.
    pub lease_lost: bool,
    pub sessions: Vec<SessionCleanupResult>,
}

/// The pass's terminal result: the report plus the exit-status signal for
/// the CLI. Errors, possible partial effects and lease loss make `failed`
/// true even though the report still describes the completed work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualCleanupOutcome {
    pub report: ManualCleanupReport,
    pub failed: bool,
}

/// The running state the pass threads through its candidate loop.
#[derive(Debug, Default)]
pub(crate) struct CandidateRun {
    pub scanned: usize,
    pub attempted: usize,
    pub completed_cascades: usize,
    pub bound_reached: bool,
    pub lease_lost: bool,
    pub results: Vec<SessionCleanupResult>,
}

impl CandidateRun {
    /// Admissions so far: charged attempts in real runs, capped eligible
    /// selections in dry runs. Read-only skips never consume budget.
    pub(crate) fn admissions(&self, options: &ManualCleanupOptions) -> usize {
        if options.dry_run {
            self.results
                .iter()
                .filter(|result| result.status == SessionCleanupStatus::WouldDelete)
                .count()
        } else {
            self.attempted
        }
    }
}

/// The read-only guards' verdict for one candidate.
pub(crate) enum PassAdmission {
    /// A known live hold or fresh activity: skip, mutate nothing, spend no
    /// budget.
    Active(&'static str),
    /// The canonical record changed or vanished since the snapshot.
    RevisionChanged {
        missing: bool,
    },
    Ready,
}

/// An age-expired candidate, retaining the revision the snapshot listed it
/// under so the pass can reject a record that changed since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupCandidate {
    pub storage_key: String,
    pub metadata_revision: u64,
    /// Effective activity the worker's age semantics use: the activity
    /// record's `last_activity_at`, else the metadata `created_at`.
    pub effective_activity: DateTime<Utc>,
}

/// Select this pass's candidates: the agent scope first (a named filter
/// never selects inline sessions; unfiltered runs include them), then the
/// worker's age rule (strict `<` against the threshold, same fallback to
/// `created_at`), ordered oldest effective activity first with the storage
/// key as the deterministic tie-breaker.
pub(crate) fn select_candidates(
    records: &[ListedSession],
    threshold: u64,
    agent: Option<&str>,
) -> Vec<CleanupCandidate> {
    let mut candidates: Vec<CleanupCandidate> = records
        .iter()
        .filter(|record| {
            agent.is_none_or(|agent| {
                matches!(&record.metadata.agent, SessionAgentSource::Named { name } if name == agent)
            })
        })
        .filter(|record| crate::remote_session_cleanup::is_cleanup_candidate(record, threshold))
        .map(|record| CleanupCandidate {
            storage_key: record.metadata.storage_key(),
            metadata_revision: record.metadata_revision,
            effective_activity: record
                .activity
                .as_ref()
                .map(|activity| activity.last_activity_at)
                .unwrap_or(record.metadata.created_at),
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.effective_activity
            .cmp(&right.effective_activity)
            .then_with(|| left.storage_key.cmp(&right.storage_key))
    });
    candidates
}

/// Summary counters derived from the per-session results, so the report and
/// the loop can never disagree.
pub(crate) struct CleanupCounters {
    pub deleted: usize,
    pub skipped_active: usize,
    pub skipped_revision_changed: usize,
    pub errors: usize,
    pub would_delete: usize,
    pub stream_bytes_deleted: u64,
}

pub(crate) fn summarize(results: &[SessionCleanupResult]) -> CleanupCounters {
    let mut counters = CleanupCounters {
        deleted: 0,
        skipped_active: 0,
        skipped_revision_changed: 0,
        errors: 0,
        would_delete: 0,
        stream_bytes_deleted: 0,
    };
    for result in results {
        match result.status {
            SessionCleanupStatus::Deleted => {
                counters.deleted += 1;
                counters.stream_bytes_deleted += result.stream_bytes;
            }
            SessionCleanupStatus::WouldDelete => counters.would_delete += 1,
            SessionCleanupStatus::SkippedActive => counters.skipped_active += 1,
            SessionCleanupStatus::SkippedRevisionChanged => counters.skipped_revision_changed += 1,
            SessionCleanupStatus::Failed | SessionCleanupStatus::Error => counters.errors += 1,
        }
    }
    counters
}

impl ManualCleanupReport {
    /// Build the final report from the loop state. `failed` (on the outcome)
    /// folds in errors, lease loss and possible partial effects separately.
    pub(crate) fn build(options: &ManualCleanupOptions, run: CandidateRun) -> Self {
        let counters = summarize(&run.results);
        Self {
            mode: if options.dry_run {
                ManualCleanupMode::DryRun
            } else {
                ManualCleanupMode::Delete
            },
            days: options.days,
            agent_filter: options.agent.clone(),
            max: options.max,
            scanned: run.scanned,
            attempted: run.attempted,
            deleted: counters.deleted,
            skipped_active: counters.skipped_active,
            skipped_revision_changed: counters.skipped_revision_changed,
            errors: counters.errors,
            bound_reached: run.bound_reached,
            would_delete: counters.would_delete,
            stream_bytes_deleted: counters.stream_bytes_deleted,
            lease_lost: run.lease_lost,
            sessions: run.results,
        }
    }

    /// A pass with nothing to scan (e.g. no metadata bucket): a success with
    /// zero work, never an error.
    pub(crate) fn empty(options: &ManualCleanupOptions) -> Self {
        Self::build(options, CandidateRun::default())
    }

    /// Whether the CLI must exit nonzero after printing this report:
    /// errors, GC-lease loss, or any session whose cascade may have left
    /// partial effects.
    pub(crate) fn requires_failure_status(&self) -> bool {
        self.errors > 0
            || self.lease_lost
            || self.sessions.iter().any(|session| session.partial_possible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nats_session_metadata::{SessionActivity, SessionInitializer, SessionMetadata};
    use chrono::TimeZone;

    fn named(session_id: &str, agent: &str, activity: Option<i64>) -> ListedSession {
        ListedSession {
            metadata: SessionMetadata::new(
                session_id,
                SessionInitializer::named(agent, Default::default()),
            ),
            metadata_revision: 3,
            activity: activity.map(|seconds| SessionActivity {
                first_activation_at: None,
                last_activity_at: Utc.timestamp_opt(seconds, 0).unwrap(),
            }),
            unread: false,
        }
    }

    fn inline(session_id: &str, activity: Option<i64>) -> ListedSession {
        let mut record = named(session_id, "hephaestus", activity);
        record.metadata.agent = SessionAgentSource::Inline {
            instructions: "test".to_string(),
        };
        record
    }

    fn storage_key(agent: &str, session_id: &str) -> String {
        harnx_core::session_identity::session_key(Some(agent), session_id)
    }

    #[test]
    fn selection_orders_oldest_first_then_storage_key() {
        let records = vec![
            named("b", "hephaestus", Some(90)),
            named("a", "hephaestus", Some(90)),
            named("c", "hephaestus", Some(10)),
        ];
        let selected = select_candidates(&records, 100, None);
        let keys: Vec<String> = selected
            .iter()
            .map(|candidate| candidate.storage_key.clone())
            .collect();
        // Oldest effective activity first.
        assert_eq!(keys[0], storage_key("hephaestus", "c"));
        // Equal ages tie-break by storage key, deterministically.
        let (a, b) = (
            storage_key("hephaestus", "a"),
            storage_key("hephaestus", "b"),
        );
        let expected = if a < b { vec![a, b] } else { vec![b, a] };
        assert_eq!(&keys[1..], expected.as_slice());
    }

    #[test]
    fn named_filter_selects_only_that_agents_sessions() {
        let records = vec![
            named("mine", "athena", Some(50)),
            named("other", "ares", Some(50)),
            inline("inline", Some(50)),
        ];
        let selected = select_candidates(&records, 100, Some("athena"));
        let keys: Vec<String> = selected
            .iter()
            .map(|candidate| candidate.storage_key.clone())
            .collect();
        assert_eq!(keys, vec![storage_key("athena", "mine")]);
    }

    #[test]
    fn unfiltered_selection_includes_inline_sessions() {
        let records = vec![
            named("named", "athena", Some(50)),
            inline("inline", Some(50)),
        ];
        assert_eq!(select_candidates(&records, 100, None).len(), 2);
    }

    #[test]
    fn selection_uses_the_worker_age_rule_with_created_at_fallback() {
        let mut missing_stale = named("missing-stale", "athena", None);
        missing_stale.metadata.created_at = Utc.timestamp_opt(99, 0).unwrap();
        let mut missing_fresh = named("missing-fresh", "athena", None);
        missing_fresh.metadata.created_at = Utc.timestamp_opt(101, 0).unwrap();
        let records = vec![
            named("stale", "athena", Some(99)),
            named("borderline", "athena", Some(100)),
            named("fresh", "athena", Some(101)),
            missing_stale,
            missing_fresh,
        ];
        let selected = select_candidates(&records, 100, None);
        let keys: Vec<String> = selected
            .iter()
            .map(|candidate| candidate.storage_key.clone())
            .collect();
        // Strict `<` against the threshold: borderline is not expired; the
        // created_at fallback selects stale metadata without an activity
        // record, exactly the worker's rule.
        assert!(keys.contains(&storage_key("athena", "stale")));
        assert!(keys.contains(&storage_key("athena", "missing-stale")));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn selection_retains_the_listed_revision() {
        let records = vec![named("kept", "athena", Some(50))];
        let selected = select_candidates(&records, 100, None);
        assert_eq!(selected[0].metadata_revision, 3);
    }

    fn result(status: SessionCleanupStatus, stream_bytes: u64) -> SessionCleanupResult {
        SessionCleanupResult {
            storage_key: storage_key("athena", "s"),
            status,
            stream_bytes,
            late: false,
            partial_possible: false,
            missing_metadata: false,
            detail: None,
        }
    }

    #[test]
    fn counters_match_worker_meaning() {
        let results = vec![
            result(SessionCleanupStatus::Deleted, 10),
            result(SessionCleanupStatus::WouldDelete, 5),
            result(SessionCleanupStatus::SkippedActive, 0),
            result(SessionCleanupStatus::SkippedRevisionChanged, 0),
            result(SessionCleanupStatus::Failed, 7),
            result(SessionCleanupStatus::Error, 0),
        ];
        let counters = summarize(&results);
        assert_eq!(counters.deleted, 1);
        assert_eq!(counters.would_delete, 1);
        assert_eq!(counters.skipped_active, 1);
        assert_eq!(counters.skipped_revision_changed, 1);
        assert_eq!(counters.errors, 2);
        // Only successful cascades count reclaimed bytes.
        assert_eq!(counters.stream_bytes_deleted, 10);
    }

    #[test]
    fn empty_report_is_a_zero_success() {
        let options = ManualCleanupOptions {
            days: 7,
            ..ManualCleanupOptions::default()
        };
        let report = ManualCleanupReport::empty(&options);
        assert_eq!(report.mode, ManualCleanupMode::Delete);
        assert_eq!(report.days, 7);
        assert_eq!(report.max, DEFAULT_MANUAL_CLEANUP_MAX);
        assert_eq!(report.scanned, 0);
        assert_eq!(report.attempted, 0);
        assert!(!report.requires_failure_status());
    }

    #[test]
    fn failure_status_follows_errors_lease_loss_and_partial_effects() {
        let options = ManualCleanupOptions::default();
        let mut run = CandidateRun {
            results: vec![result(SessionCleanupStatus::Failed, 0)],
            ..CandidateRun::default()
        };
        run.results[0].partial_possible = true;
        assert!(ManualCleanupReport::build(&options, run).requires_failure_status());

        let run = CandidateRun {
            lease_lost: true,
            ..CandidateRun::default()
        };
        assert!(ManualCleanupReport::build(&options, run).requires_failure_status());

        // Known pre-mutation skips are not failures.
        let run = CandidateRun {
            results: vec![
                result(SessionCleanupStatus::SkippedActive, 0),
                result(SessionCleanupStatus::SkippedRevisionChanged, 0),
            ],
            ..CandidateRun::default()
        };
        assert!(!ManualCleanupReport::build(&options, run).requires_failure_status());
    }

    #[test]
    fn dry_run_mode_reports_would_delete_and_zero_reclaimed_bytes() {
        let options = ManualCleanupOptions {
            dry_run: true,
            ..ManualCleanupOptions::default()
        };
        let run = CandidateRun {
            results: vec![result(SessionCleanupStatus::WouldDelete, 42)],
            ..CandidateRun::default()
        };
        let report = ManualCleanupReport::build(&options, run);
        assert_eq!(report.mode, ManualCleanupMode::DryRun);
        assert_eq!(report.would_delete, 1);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.stream_bytes_deleted, 0);
    }

    #[test]
    fn options_reject_zero_days_and_max() {
        assert!(ManualCleanupOptions::default().validate().is_err());
        let mut options = ManualCleanupOptions {
            days: 1,
            ..ManualCleanupOptions::default()
        };
        assert!(options.validate().is_ok());
        options.max = 0;
        assert!(options.validate().is_err());
    }
}
