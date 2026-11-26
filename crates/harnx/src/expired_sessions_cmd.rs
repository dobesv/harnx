//! `harnx delete expired-sessions` (#2923 T3): resolves options, validates
//! them before any broker connection, dispatches T2's lease-owning runtime
//! pass, prints the report, and maps the outcome's failure flag onto the
//! process exit code. The runtime owns GC-lease acquisition, renewal,
//! revalidation and release; nothing here duplicates that.

use crate::cli::DeleteExpiredSessionsArgs;
use crate::config::WorkingMode;
use crate::init_frontend_config;
use anyhow::{bail, ensure, Result};
use harnx_runtime::manual_session_cleanup::{
    run_manual_expired_session_cleanup, ManualCleanupMode, ManualCleanupOptions,
    ManualCleanupReport, SessionCleanupResult, SessionCleanupStatus,
};

/// Bound the per-run admission batch, mirroring the orphan-CLI precedent.
/// Zero is not "delete nothing" — it is no bound at all in the other
/// direction, and the cap is the only thing spreading a bad-inventory blast
/// radius across passes.
pub fn resolve_max(max: usize) -> Result<usize> {
    ensure!(
        max > 0,
        "--max must be positive; 0 is not a valid deletion batch"
    );
    Ok(max)
}

/// Resolve retention days from an explicit `--days` or the configured
/// `cleanup_remote_sessions_days`. An explicit value wins and never
/// persists; a disabled (0) or unset configuration fails clearly rather than
/// silently becoming "delete everything older than zero days".
pub fn resolve_days(explicit: Option<u64>, configured: Option<u64>) -> Result<u64> {
    if let Some(days) = explicit {
        ensure!(
            days > 0,
            "--days must be positive; 0 would make every session in scope a deletion \
             candidate. Pass a deliberate positive value, or omit --days to use the \
             configured cleanup_remote_sessions_days"
        );
        return Ok(days);
    }
    match configured {
        None => bail!(
            "no retention days are configured (cleanup_remote_sessions_days is \
             unset); pass --days <n> for an explicit one-off bound, or set \
             cleanup_remote_sessions_days to enable the hourly worker GC as well"
        ),
        Some(0) => bail!(
            "session retention is disabled on this cluster (cleanup_remote_sessions_days \
             = 0); pass --days <n> for an explicit one-off bound. Passing --days does \
             not change the stored configuration or re-enable the hourly worker GC"
        ),
        Some(days) => Ok(days),
    }
}

pub async fn run(args: &DeleteExpiredSessionsArgs) -> Result<u8> {
    // Positivity is checked before the config (let alone the broker) is
    // touched; the runtime re-validates so no caller can bypass it.
    let max = resolve_max(args.max)?;
    let config = init_frontend_config(WorkingMode::Cmd, true).await?;
    let days = resolve_days(args.days, config.cleanup_remote_sessions_days)?;
    let options = ManualCleanupOptions {
        days,
        agent: args.agent.clone(),
        max,
        dry_run: args.dry_run,
    };
    // GC-lease contention, snapshot and connection failures return Err from
    // here and surface as the CLI's error path; the outcome below is for a
    // pass that ran and may still have failed sessions.
    let outcome = run_manual_expired_session_cleanup(&config, &args.cluster, options).await?;
    print!("{}", render_cleanup_report(&outcome.report, &args.cluster));
    Ok(u8::from(outcome.failed))
}

fn mode_label(mode: ManualCleanupMode) -> &'static str {
    match mode {
        ManualCleanupMode::Delete => "delete",
        ManualCleanupMode::DryRun => "dry run, nothing is deleted",
    }
}

/// Compact summary first, then per-session dispositions, then the hints an
/// operator needs to interpret the numbers.
pub fn render_cleanup_report(report: &ManualCleanupReport, cluster: &str) -> String {
    let mut out = format!(
        "Manual expired-session cleanup on cluster '{cluster}' ({}; days={}, agent filter={}, max={}):\n",
        mode_label(report.mode),
        report.days,
        report.agent_filter.as_deref().unwrap_or("none"),
        report.max
    );
    out.push_str(&format!(
        "  scanned: {} age-expired candidates\n",
        report.scanned
    ));
    out.push_str(&format!(
        "  attempted: {}, deleted: {}\n",
        report.attempted, report.deleted
    ));
    out.push_str(&format!(
        "  skipped active: {}, skipped revision changed: {}, errors: {}\n",
        report.skipped_active, report.skipped_revision_changed, report.errors
    ));
    if report.mode == ManualCleanupMode::DryRun {
        out.push_str(&format!(
            "  would delete: {} eligible selections within the cap\n",
            report.would_delete
        ));
    }
    out.push_str(&format!(
        "  session-stream bytes deleted: {}\n",
        report.stream_bytes_deleted
    ));
    out.push_str(&format!(
        "  bound reached: {}, GC lease lost: {}\n",
        if report.bound_reached { "yes" } else { "no" },
        if report.lease_lost { "yes" } else { "no" }
    ));
    for session in &report.sessions {
        out.push_str(&render_session_result(session));
    }
    if report.bound_reached {
        // bound_reached is a stop line, not a backlog estimate: candidates
        // may have gained holds or activity since the snapshot.
        out.push_str("The admission cap stopped this pass; run the command again to continue.\n");
    }
    if report.lease_lost {
        out.push_str(
            "The GC lease was lost mid-pass; no new candidate was admitted after that \
             point. Inspect the sessions listed above before re-running.\n",
        );
    }
    if report.errors > 0 {
        out.push_str(
            "Errors or possible partial effects occurred; inspect the sessions listed \
             above and re-run after fixing the cause.\n",
        );
    }
    out
}

fn render_session_result(session: &SessionCleanupResult) -> String {
    let mut line = format!("  {}: ", session.storage_key);
    line.push_str(match session.status {
        SessionCleanupStatus::Deleted => "deleted",
        SessionCleanupStatus::WouldDelete => "would delete",
        SessionCleanupStatus::SkippedActive => "skipped (active)",
        SessionCleanupStatus::SkippedRevisionChanged => {
            if session.missing_metadata {
                "skipped (metadata missing)"
            } else {
                "skipped (revision changed)"
            }
        }
        SessionCleanupStatus::Failed => "FAILED",
        SessionCleanupStatus::Error => "error",
    });
    if session.stream_bytes > 0 {
        line.push_str(&format!(" ({} B session stream)", session.stream_bytes));
    }
    if session.late {
        line.push_str(" [late: raced a live change after admission]");
    }
    if session.partial_possible {
        line.push_str(" [partial effects possible]");
    }
    if let Some(detail) = &session.detail {
        line.push_str(&format!(": {detail}"));
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_days_explicit_override_wins_and_zero_is_rejected() {
        assert_eq!(resolve_days(Some(7), Some(30)).unwrap(), 7);
        assert_eq!(resolve_days(Some(7), None).unwrap(), 7);
        let error = resolve_days(Some(0), Some(30)).unwrap_err();
        assert!(
            error.to_string().contains("--days must be positive"),
            "{error:#}"
        );
    }

    #[test]
    fn resolve_days_configured_value_is_used_when_positive() {
        assert_eq!(resolve_days(None, Some(30)).unwrap(), 30);
    }

    #[test]
    fn resolve_days_disabled_config_fails_with_retention_hint() {
        let error = resolve_days(None, Some(0)).unwrap_err();
        assert!(
            error.to_string().contains("cleanup_remote_sessions_days"),
            "{error:#}"
        );
        assert!(error.to_string().contains("disabled"), "{error:#}");
    }

    #[test]
    fn resolve_days_missing_config_fails_with_retention_hint() {
        let error = resolve_days(None, None).unwrap_err();
        assert!(
            error.to_string().contains("cleanup_remote_sessions_days"),
            "{error:#}"
        );
        assert!(error.to_string().contains("unset"), "{error:#}");
    }

    #[test]
    fn resolve_max_rejects_zero() {
        assert_eq!(resolve_max(100).unwrap(), 100);
        assert!(resolve_max(0).is_err());
    }

    fn cleanup_report(mode: ManualCleanupMode) -> ManualCleanupReport {
        ManualCleanupReport {
            mode,
            days: 30,
            agent_filter: None,
            max: 100,
            scanned: 3,
            attempted: 1,
            deleted: 1,
            skipped_active: 1,
            skipped_revision_changed: 1,
            errors: 0,
            bound_reached: false,
            would_delete: 0,
            stream_bytes_deleted: 2048,
            lease_lost: false,
            sessions: vec![
                SessionCleanupResult {
                    storage_key: "k-deleted".to_string(),
                    status: SessionCleanupStatus::Deleted,
                    stream_bytes: 2048,
                    late: false,
                    partial_possible: false,
                    missing_metadata: false,
                    detail: None,
                },
                SessionCleanupResult {
                    storage_key: "k-active".to_string(),
                    status: SessionCleanupStatus::SkippedActive,
                    stream_bytes: 0,
                    late: false,
                    partial_possible: false,
                    missing_metadata: false,
                    detail: None,
                },
            ],
        }
    }

    #[test]
    fn cleanup_report_text_prints_summary_and_dispositions() {
        let out = render_cleanup_report(&cleanup_report(ManualCleanupMode::Delete), "stats");
        assert!(
            out.contains("Manual expired-session cleanup on cluster 'stats' (delete; days=30, agent filter=none, max=100)"),
            "{out}"
        );
        assert!(out.contains("scanned: 3 age-expired candidates"), "{out}");
        assert!(out.contains("attempted: 1, deleted: 1"), "{out}");
        assert!(
            out.contains("skipped active: 1, skipped revision changed: 1, errors: 0"),
            "{out}"
        );
        assert!(out.contains("session-stream bytes deleted: 2048"), "{out}");
        assert!(
            out.contains("bound reached: no, GC lease lost: no"),
            "{out}"
        );
        assert!(
            out.contains("k-deleted: deleted (2048 B session stream)"),
            "{out}"
        );
        assert!(out.contains("k-active: skipped (active)"), "{out}");
        // No rerun hint when the pass was not capped.
        assert!(!out.contains("run the command again"), "{out}");
    }

    #[test]
    fn cleanup_report_text_marks_dry_run_bound_and_failure_hints() {
        let mut report = cleanup_report(ManualCleanupMode::DryRun);
        report.attempted = 0;
        report.deleted = 0;
        report.would_delete = 2;
        report.bound_reached = true;
        report.sessions[0].status = SessionCleanupStatus::WouldDelete;
        report.sessions[1].status = SessionCleanupStatus::Error;
        report.sessions[1].detail = Some("malformed a2a state".to_string());
        report.errors = 1;
        let out = render_cleanup_report(&report, "stats");
        assert!(out.contains("dry run, nothing is deleted"), "{out}");
        assert!(out.contains("would delete: 2 eligible selections"), "{out}");
        assert!(out.contains("bound reached: yes"), "{out}");
        assert!(
            out.contains("k-deleted: would delete (2048 B session stream)"),
            "{out}"
        );
        assert!(
            out.contains("k-active: error: malformed a2a state"),
            "{out}"
        );
        assert!(
            out.contains("The admission cap stopped this pass; run the command again to continue."),
            "{out}"
        );
        assert!(
            out.contains("Errors or possible partial effects occurred"),
            "{out}"
        );
    }

    #[test]
    fn cleanup_report_text_flags_late_and_partial_effects() {
        let mut report = cleanup_report(ManualCleanupMode::Delete);
        report.lease_lost = true;
        report.sessions[0].status = SessionCleanupStatus::Failed;
        report.sessions[0].late = true;
        report.sessions[0].partial_possible = true;
        report.sessions[0].detail = Some("cascade failed after stream purge".to_string());
        report.sessions[1].status = SessionCleanupStatus::SkippedRevisionChanged;
        report.sessions[1].missing_metadata = true;
        let out = render_cleanup_report(&report, "stats");
        assert!(
            out.contains("k-deleted: FAILED (2048 B session stream) [late: raced a live change after admission] [partial effects possible]: cascade failed after stream purge"),
            "{out}"
        );
        assert!(
            out.contains("k-active: skipped (metadata missing)"),
            "{out}"
        );
        assert!(out.contains("GC lease lost: yes"), "{out}");
        assert!(
            out.contains("no new candidate was admitted after that"),
            "{out}"
        );
    }
}
