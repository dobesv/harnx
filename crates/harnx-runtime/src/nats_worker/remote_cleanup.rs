//! Hourly session GC lifecycle and outcome logging for the worker daemon.

use crate::config::GlobalConfig;
use crate::remote_session_cleanup::{
    run_periodic_remote_cleanup, CleanupOutcome, RemoteCleanupStats,
};
use std::time::Duration;
use tokio::task::JoinHandle;

const REMOTE_SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(super) fn spawn_remote_session_cleanup(
    config: GlobalConfig,
    cluster: String,
    retention: Option<u64>,
) -> Option<JoinHandle<()>> {
    let days = match retention {
        None => {
            log::warn!(
                "session GC DISABLED for cluster='{cluster}'; NATS session state grows unbounded; \
                 set HARNX_CLEANUP_REMOTE_SESSIONS_DAYS or config key \
                 cleanup_remote_sessions_days to enable it"
            );
            return None;
        }
        Some(0) => {
            log::info!("session GC explicitly disabled for cluster='{cluster}'");
            return None;
        }
        Some(days) => days,
    };

    log::info!("session GC enabled for cluster='{cluster}': retention_days={days} interval=1h");
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(REMOTE_SESSION_CLEANUP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let snapshot = config.read().clone();
            let outcome = run_periodic_remote_cleanup(&snapshot, days, &cluster).await;
            log_cleanup_outcome(outcome, &cluster);
        }
    }))
}

fn log_cleanup_outcome(outcome: CleanupOutcome, cluster: &str) {
    match outcome {
        CleanupOutcome::Ran(stats) => log_cleanup_stats(&stats, cluster),
        CleanupOutcome::NotLeader => {
            log::debug!(
                "session GC did not run for cluster='{cluster}': lease is held by another worker or CLI"
            );
        }
        CleanupOutcome::Failed => {
            log::warn!(
                "session GC did not complete for cluster='{cluster}' this cycle; will retry \
                 next tick"
            );
        }
        CleanupOutcome::Skipped => {
            log::debug!(
                "session GC skipped for cluster='{cluster}': current hour already collected"
            );
        }
    }
}

fn log_cleanup_stats(stats: &RemoteCleanupStats, cluster: &str) {
    if cleanup_has_activity(stats) {
        log::info!(
            "session GC completed for cluster='{cluster}': scanned={} deleted={} \
             skipped_active={} skipped_metadata_changed={} orphan_streams={} \
             orphan_deleted={} orphan_kept={} orphan_unmapped={} errors={}",
            stats.scanned,
            stats.deleted,
            stats.skipped_active,
            stats.skipped_metadata_changed,
            stats.orphan_streams,
            stats.orphan_deleted,
            stats.orphan_kept,
            stats.orphan_unmapped,
            stats.errors
        );
    } else {
        log::debug!(
            "session GC completed for cluster='{cluster}': scanned={} deleted=0 \
             skipped_active={} skipped_metadata_changed=0 errors=0",
            stats.scanned,
            stats.skipped_active
        );
    }
}

/// A pass that deleted something, hit a revision conflict or error, or kept
/// an idle orphan (including those deferred by the batch limit) logs at info.
fn cleanup_has_activity(stats: &RemoteCleanupStats) -> bool {
    [
        stats.deleted,
        stats.skipped_metadata_changed,
        stats.orphan_deleted,
        stats.orphan_kept,
        stats.errors,
    ]
    .into_iter()
    .any(|count| count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletions_revision_conflicts_kept_orphans_and_errors_raise_gc_log_level() {
        for stats in [
            RemoteCleanupStats {
                deleted: 1,
                ..Default::default()
            },
            RemoteCleanupStats {
                skipped_metadata_changed: 1,
                ..Default::default()
            },
            RemoteCleanupStats {
                orphan_deleted: 1,
                ..Default::default()
            },
            RemoteCleanupStats {
                orphan_kept: 1,
                ..Default::default()
            },
            RemoteCleanupStats {
                errors: 1,
                ..Default::default()
            },
        ] {
            assert!(cleanup_has_activity(&stats), "expected info log: {stats:?}");
        }
    }

    #[test]
    fn scanning_without_changes_keeps_gc_at_debug_level() {
        let stats = RemoteCleanupStats {
            scanned: 3,
            skipped_active: 2,
            orphan_unmapped: 1,
            ..Default::default()
        };
        assert!(!cleanup_has_activity(&stats));
        assert!(!cleanup_has_activity(&RemoteCleanupStats::default()));
    }
}
