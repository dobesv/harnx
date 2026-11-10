//! Commands and report rendering for `harnx list/delete orphan-streams` (GH #2650).
//!
//! Pure grace and batch helpers keep `cleanup_remote_sessions_days: 0` and
//! `--grace-hours 0` from turning "GC disabled" into "delete every empty orphan
//! immediately". Unit tests cover these bounds without connecting to NATS.

use crate::cli::{DeleteOrphanStreamsArgs, ListOrphanStreamsArgs};
use crate::config::WorkingMode;
use crate::init_frontend_config;
use anyhow::{bail, ensure, Result};
use harnx_runtime::session_stream_orphans::{OrphanStreamCleanupStats, OrphanStreamInventory};

fn wall_clock_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn run_list_orphan_streams_command(args: &ListOrphanStreamsArgs) -> Result<()> {
    let config = init_frontend_config(WorkingMode::Cmd, true).await?;
    let inventory =
        harnx_runtime::session_stream_orphans::list_orphan_session_streams(&config, &args.cluster)
            .await?;
    print!(
        "{}",
        render_orphan_inventory(&inventory, &args.cluster, wall_clock_secs())
    );
    Ok(())
}

pub async fn run_orphan_stream_delete_command(args: &DeleteOrphanStreamsArgs) -> Result<()> {
    let config = init_frontend_config(WorkingMode::Cmd, true).await?;
    // Grace and batch bounds are validated (positivity enforced) before any
    // lease is taken or any stream is read; the pass re-validates defensively.
    let grace_secs = resolve_grace_secs(args.grace_hours, config.cleanup_remote_sessions_days)?;
    let max_deletions = resolve_max_deletions(args.max)?;
    let options = harnx_runtime::session_stream_orphans::OrphanStreamCleanupOptions {
        grace_secs,
        max_deletions,
    };

    // Hold the same leader lease as the hourly session-metadata GC, so the
    // CLI cannot run a concurrent orphan batch against the same streams. No
    // epoch-hour dedup marker: the operator asked for this pass explicitly.
    let lease =
        harnx_runtime::remote_session_cleanup::acquire_session_gc_lease(&config, &args.cluster)
            .await?;
    let Some(lease) = lease else {
        anyhow::bail!(
            "the session metadata GC lease on cluster '{}' is held by another pass \
             (worker or CLI); retry when it completes",
            args.cluster
        );
    };

    let cleanup = harnx_runtime::session_stream_orphans::cleanup_orphan_session_streams(
        &config,
        &args.cluster,
        wall_clock_secs(),
        &options,
    )
    .await;
    if let Err(error) = lease.release().await {
        log::warn!(
            "orphan stream cleanup lease release failed: cluster='{}' err={error:#}",
            args.cluster
        );
    }
    let stats = cleanup?;

    print!(
        "{}",
        render_cleanup_report(&stats, &args.cluster, grace_secs, max_deletions)
    );
    Ok(())
}

const SECONDS_PER_HOUR: u64 = 60 * 60;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

/// Resolve the orphan grace period, in seconds.
///
/// An explicit `--grace-hours` wins over the configured retention
/// (`cleanup_remote_sessions_days`). `Some(0)` retention means "GC disabled"
/// everywhere else in the product, so it never silently becomes a zero-hour
/// grace window here: an operator on a disabled-GC cluster must pass
/// `--grace-hours` with a deliberate positive value instead. Zero hours is
/// rejected outright — "delete every empty orphan right now, regardless of
/// age" is exactly the batch the grace period exists to prevent.
pub fn resolve_grace_secs(grace_hours: Option<u64>, retention_days: Option<u64>) -> Result<u64> {
    if let Some(hours) = grace_hours {
        ensure!(
            hours > 0,
            "--grace-hours must be positive; 0 would delete every empty orphan \
             stream immediately, regardless of age"
        );
        return Ok(hours.saturating_mul(SECONDS_PER_HOUR));
    }
    match retention_days {
        Some(days) if days > 0 => Ok(days.saturating_mul(SECONDS_PER_DAY)),
        // Retention absent or explicitly disabled: require an explicit grace.
        _ => bail!(
            "no orphan grace period configured: pass --grace-hours <n> or set \
             cleanup_remote_sessions_days to a positive value (0 disables the session GC \
             entirely and provides no grace window)"
        ),
    }
}

/// The grace period in whole hours, for report headers.
pub fn grace_secs_to_hours(grace_secs: u64) -> u64 {
    grace_secs / SECONDS_PER_HOUR
}

/// Bound the per-run deletion batch. Zero is rejected: it is not "delete
/// everything", but it is also not a bound, and the batch cap is the only
/// thing spreading a bad-inventory blast radius across hourly passes.
/// Deliberately unbounded above: `--max` is the documented incident-recovery
/// dial, and an arbitrary ceiling would just surprise an operator mid-fire.
pub fn resolve_max_deletions(max: usize) -> Result<usize> {
    ensure!(
        max > 0,
        "--max must be positive; 0 is not a valid deletion batch"
    );
    Ok(max)
}

/// `94205` -> `"1d 2h"`. Coarse on purpose: the numbers are diagnostic.
pub fn format_age_secs(age_secs: u64) -> String {
    let days = age_secs / SECONDS_PER_DAY;
    let hours = (age_secs % SECONDS_PER_DAY) / SECONDS_PER_HOUR;
    let minutes = (age_secs % SECONDS_PER_HOUR) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Unix seconds -> RFC 3339 (UTC, second precision), falling back to the raw
/// number for values `chrono` cannot represent.
pub fn format_unix_secs(secs: u64) -> String {
    match chrono::DateTime::from_timestamp(secs as i64, 0) {
        Some(time) => time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        None => secs.to_string(),
    }
}

/// Dry-run report for `harnx list orphan-streams`.
pub fn render_orphan_inventory(
    inventory: &OrphanStreamInventory,
    cluster: &str,
    now_unix_secs: u64,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "SESSION_ streams with no session metadata on cluster '{cluster}' \
         (dry run, nothing is deleted):\n"
    ));
    if inventory.orphans.is_empty() && inventory.unmapped.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for orphan in &inventory.orphans {
        out.push_str(&format!("{}\n", orphan.stream_name));
        out.push_str(&format!("  storage key:    {}\n", orphan.storage_key));
        out.push_str(&format!(
            "  created:        {} (age {})\n",
            format_unix_secs(orphan.created_unix_secs),
            format_age_secs(now_unix_secs.saturating_sub(orphan.created_unix_secs))
        ));
        out.push_str(&format!("  messages:       {}\n", orphan.messages));
        out.push_str(&format!("  bytes:          {}\n", orphan.bytes));
        out.push_str(&format!(
            "  last message:   {}\n",
            orphan
                .last_message_unix_secs
                .map(format_unix_secs)
                .as_deref()
                .unwrap_or("(none)")
        ));
        out.push_str(&format!(
            "  session lease:  {}\n",
            yes_no(orphan.session_lease_present, "present", "no")
        ));
        out.push_str(&format!(
            "  a2a lease:      {}\n",
            yes_no(orphan.a2a_lease_present, "present", "no")
        ));
        out.push_str(&format!(
            "  a2a unresolved: {}\n",
            yes_no(orphan.a2a_unresolved_tasks, "yes", "no")
        ));
    }
    if !inventory.unmapped.is_empty() {
        out.push_str(
            "\nSESSION_ streams that cannot be mapped to a session key \
             (kept for manual review):\n",
        );
        for unmapped in &inventory.unmapped {
            out.push_str(&format!("  {}\n", unmapped.stream_name));
            out.push_str(&format!("    subjects: {}\n", unmapped.subjects.join(", ")));
            out.push_str(&format!("    reason:   {}\n", unmapped.reason));
        }
    }
    out
}

/// Result report for `harnx delete orphan-streams`.
pub fn render_cleanup_report(
    stats: &OrphanStreamCleanupStats,
    cluster: &str,
    grace_secs: u64,
    max_deletions: usize,
) -> String {
    let mut out = format!(
        "Orphan session stream cleanup on cluster '{cluster}' \
         (grace={}h, max={max_deletions}): scanned={} deleted={} quarantined={} \
         skipped_recent={} skipped_active={} skipped_metadata={} deferred={} \
         unmapped={} errors={}\n",
        grace_secs_to_hours(grace_secs),
        stats.scanned,
        stats.deleted,
        stats.quarantined,
        stats.skipped_recent,
        stats.skipped_active,
        stats.skipped_metadata,
        stats.deferred,
        stats.unmapped,
        stats.errors
    );
    if stats.quarantined > 0 {
        out.push_str(&format!(
            "Quarantined streams hold messages and are never deleted automatically; review them \
             with 'harnx list orphan-streams --cluster {cluster}'.\n"
        ));
    }
    if stats.unmapped > 0 {
        out.push_str(&format!(
            "Unmappable streams are reported and never deleted automatically; see \
             'harnx list orphan-streams --cluster {cluster}'.\n"
        ));
    }
    if stats.deferred > 0 {
        out.push_str(&format!(
            "{} candidates were beyond this run's batch limit and are deferred to the next \
             pass; re-run the command until the backlog clears, or pass a larger --max.\n",
            stats.deferred
        ));
    }
    out
}

fn yes_no<'a>(flag: bool, yes: &'a str, no: &'a str) -> &'a str {
    if flag {
        yes
    } else {
        no
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_runtime::session_stream_orphans::{
        OrphanStreamReport, UnmappedStreamReport, DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN,
    };

    fn assert_contains_all(text: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(text.contains(fragment), "missing {fragment:?} in {text:?}");
        }
    }

    #[test]
    fn explicit_grace_hours_win_and_must_be_positive() {
        assert_eq!(resolve_grace_secs(Some(48), Some(1)).unwrap(), 48 * 3_600);
        assert_eq!(resolve_grace_secs(Some(48), None).unwrap(), 48 * 3_600);
        let rejected = resolve_grace_secs(Some(0), None).unwrap_err();
        assert!(
            rejected
                .to_string()
                .contains("--grace-hours must be positive"),
            "zero hours must be rejected, got: {rejected:#}"
        );
    }

    #[test]
    fn zero_retention_is_disabled_not_zero_grace() {
        // The GC-disabled sentinel must not become a zero-hour grace window.
        let rejected = resolve_grace_secs(None, Some(0)).unwrap_err();
        assert!(rejected.to_string().contains("no orphan grace period"));
        assert_eq!(
            resolve_grace_secs(None, Some(3)).unwrap(),
            3 * SECONDS_PER_DAY
        );
        assert_eq!(resolve_grace_secs(None, Some(1)).unwrap(), SECONDS_PER_DAY);
    }

    #[test]
    fn missing_grace_is_an_error() {
        assert!(resolve_grace_secs(None, None).is_err());
    }

    #[test]
    fn explicit_grace_overrides_disabled_retention() {
        // Operator on a disabled-GC cluster can still run a scoped cleanup.
        assert_eq!(resolve_grace_secs(Some(6), Some(0)).unwrap(), 6 * 3_600);
    }

    #[test]
    fn max_deletions_must_be_positive() {
        assert_eq!(resolve_max_deletions(2).unwrap(), 2);
        assert!(resolve_max_deletions(0).is_err());
        assert_eq!(
            resolve_max_deletions(DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN).unwrap(),
            DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN
        );
    }

    #[test]
    fn age_formatting() {
        // Days swallow the minutes.
        for (secs, expected) in [(0, "0m"), (600, "10m"), (7_500, "2h 5m"), (94_200, "1d 2h")] {
            assert_eq!(format_age_secs(secs), expected, "age {secs}");
        }
    }

    #[test]
    fn unix_seconds_formatting() {
        assert_eq!(format_unix_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_unix_secs(1_736_000_000).as_str(),
            "2025-01-04T14:13:20Z"
        );
        // Out-of-range values fall back to the raw number rather than panic.
        // i64::MAX is a valid `as i64` cast that chrono cannot map to a year.
        assert_eq!(format_unix_secs(i64::MAX as u64), i64::MAX.to_string());
    }

    fn report(key: &str) -> OrphanStreamReport {
        OrphanStreamReport {
            stream_name: format!("SESSION_{key}"),
            storage_key: key.to_string(),
            created_unix_secs: 1_736_000_000,
            messages: 0,
            bytes: 0,
            last_message_unix_secs: None,
            session_lease_present: false,
            a2a_lease_present: false,
            a2a_unresolved_tasks: false,
        }
    }

    #[test]
    fn empty_inventory_renders_none() {
        let inventory = OrphanStreamInventory::default();
        let text = render_orphan_inventory(&inventory, "prod", 0);
        assert!(text.contains("dry run, nothing is deleted"));
        assert!(text.contains("  none"));
        assert!(!text.contains("manual review"));
    }

    #[test]
    fn inventory_renders_orphans_and_unmapped() {
        let inventory = OrphanStreamInventory {
            orphans: vec![
                report("alpha"),
                OrphanStreamReport {
                    session_lease_present: true,
                    a2a_unresolved_tasks: true,
                    ..report("beta")
                },
            ],
            unmapped: vec![UnmappedStreamReport {
                stream_name: "SESSION_unmappable".into(),
                subjects: vec!["not.a.session.subject".into()],
                reason: "no subject".into(),
            }],
        };
        let text = render_orphan_inventory(&inventory, "prod", 1_736_086_400);
        assert_contains_all(
            &text,
            &[
                "SESSION_alpha",
                "storage key:    alpha",
                "last message:   (none)",
                "session lease:  present",
                "a2a unresolved: yes",
                "SESSION_unmappable",
                "reason:   no subject",
            ],
        );
    }

    #[test]
    fn cleanup_report_counts_every_field() {
        let stats = OrphanStreamCleanupStats {
            scanned: 15,
            deleted: 5,
            quarantined: 1,
            skipped_recent: 2,
            skipped_active: 3,
            skipped_metadata: 1,
            deferred: 3,
            unmapped: 0,
            errors: 0,
        };
        let text = render_cleanup_report(&stats, "prod", 48 * 3_600, 5);
        assert_contains_all(
            &text,
            &[
                "scanned=15",
                "deleted=5",
                "deferred=3",
                "grace=48h",
                "deferred to the next",
            ],
        );
    }

    #[test]
    fn cleanup_report_without_quarantine_or_unmapped_stays_quiet() {
        let stats = OrphanStreamCleanupStats {
            scanned: 2,
            deleted: 2,
            ..Default::default()
        };
        let text = render_cleanup_report(&stats, "prod", SECONDS_PER_DAY, 5);
        assert_contains_all(&text, &["scanned=2 deleted=2", "deferred=0"]);
        for absent in [
            "Quarantined streams",
            "Unmappable streams",
            "deferred to the next",
        ] {
            assert!(!text.contains(absent), "unexpected {absent:?} in {text:?}");
        }
    }
}
