//! Text rendering for `harnx info sessions` (#2923 T3): turns T1's typed
//! storage reports into compact, greppable operator output. Pure functions —
//! the JSON path prints the typed report itself and never comes through here.

use chrono::{DateTime, Utc};
use harnx_runtime::session_stats::{
    AgeCounts, AgentStatsReport, ClusterStatsReport, RetentionReport, SessionStatsDetail,
    StatsAgentSource,
};

const KIB: f64 = 1024.0;

pub fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn optional_rfc3339(time: Option<DateTime<Utc>>) -> String {
    time.map(rfc3339).unwrap_or_else(|| "(none)".to_string())
}

/// Byte count -> compact human size, binary units, one decimal above 1024.
pub fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes < KIB as u64 {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value < KIB * KIB * KIB * KIB {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    } else {
        format!("{:.1} TiB", value / (KIB * KIB * KIB * KIB))
    }
}

fn agent_label(agent: &StatsAgentSource) -> String {
    match agent {
        StatsAgentSource::Named { name } => name.clone(),
        // Display-only convention from `list sessions`; the JSON report keeps
        // structured identity.
        StatsAgentSource::Inline => "<inline>".to_string(),
    }
}

fn activity_line(
    ages: &AgeCounts,
    oldest: Option<DateTime<Utc>>,
    newest: Option<DateTime<Utc>>,
) -> String {
    format!(
        "activity: oldest {}, newest {}\nages: <1d {}, 1-7d {}, 7-30d {}, 30d+ {}\n",
        optional_rfc3339(oldest),
        optional_rfc3339(newest),
        ages.under_1_day,
        ages.one_to_7_days,
        ages.seven_to_30_days,
        ages.over_30_days
    )
}

fn retention_text(retention: &RetentionReport) -> String {
    let configured = retention
        .configured_days
        .map(|days| format!("{days} days"))
        .unwrap_or_else(|| "unset".to_string());
    format!(
        "retention: {configured} ({})\n  age-expired: {}, held: {}, eligible: {} (snapshot estimate, not a guarantee)\n",
        if retention.enabled { "enabled" } else { "disabled" },
        retention.age_expired,
        retention.held,
        retention.eligible
    )
}

fn session_stream_lines(streams: &harnx_runtime::session_stats::SessionStreamTotals) -> String {
    let mut out = format!(
        "session streams: {} total, {}, {} msgs\n",
        streams.total.count,
        format_bytes(streams.total.bytes),
        streams.total.messages
    );
    out.push_str(&format!(
        "  matched to metadata: {} streams, {}, {} msgs\n",
        streams.matched.count,
        format_bytes(streams.matched.bytes),
        streams.matched.messages
    ));
    out.push_str(&format!(
        "  orphan (session key recovered): {} streams, {}, {} msgs\n",
        streams.orphan_mapped.count,
        format_bytes(streams.orphan_mapped.bytes),
        streams.orphan_mapped.messages
    ));
    out.push_str(&format!(
        "  orphan (unmappable): {} streams, {}, {} msgs\n",
        streams.orphan_unmapped.count,
        format_bytes(streams.orphan_unmapped.bytes),
        streams.orphan_unmapped.messages
    ));
    out
}

fn shared_bucket_lines(buckets: &[harnx_runtime::session_stats::SharedBucketReport]) -> String {
    let mut out = String::from("shared buckets (cluster-wide backing streams):\n");
    for bucket in buckets {
        out.push_str(&format!(
            "  {}: {}, {}, {} msgs\n",
            bucket.bucket,
            if bucket.present {
                "present"
            } else {
                "absent (normal for retired or unused buckets)"
            },
            format_bytes(bucket.bytes),
            bucket.messages
        ));
    }
    out
}

fn agent_lines(agents: &[harnx_runtime::session_stats::AgentTotals]) -> String {
    let mut out = String::from("agents by session-stream bytes:\n");
    if agents.is_empty() {
        out.push_str("  none\n");
    }
    for agent in agents {
        out.push_str(&format!(
            "  {}: {} sessions, {}, {} msgs\n",
            agent_label(&agent.agent),
            agent.session_count,
            format_bytes(agent.streams.bytes),
            agent.streams.messages
        ));
    }
    out
}

pub fn render_cluster_text(report: &ClusterStatsReport) -> String {
    let mut out = format!(
        "Session storage on cluster '{}' (collected {})\n",
        report.cluster,
        rfc3339(report.collected_at)
    );
    out.push_str(&format!(
        "sessions: {} metadata-backed ({} metadata-only, no stream)\n",
        report.session_count, report.streams.metadata_only_sessions
    ));
    out.push_str(&session_stream_lines(&report.streams));
    out.push_str(&activity_line(
        &report.activity.age_counts,
        report.activity.oldest_effective_activity,
        report.activity.newest_effective_activity,
    ));
    out.push_str(&shared_bucket_lines(&report.shared_buckets));
    out.push_str(&agent_lines(&report.agents));
    out.push_str(&retention_text(&report.retention));
    out
}

pub fn render_agent_text(report: &AgentStatsReport) -> String {
    let mut out = format!(
        "Session storage for agent '{}' on cluster '{}' (collected {})\n",
        agent_label(&report.agent),
        report.cluster,
        rfc3339(report.collected_at)
    );
    out.push_str(&format!(
        "sessions: {} metadata-backed ({} metadata-only, no stream)\n",
        report.session_count, report.metadata_only_sessions
    ));
    out.push_str(&format!(
        "session streams: {}, {}, {} msgs\n",
        report.streams.count,
        format_bytes(report.streams.bytes),
        report.streams.messages
    ));
    out.push_str(&activity_line(
        &report.activity.age_counts,
        report.activity.oldest_effective_activity,
        report.activity.newest_effective_activity,
    ));
    out.push_str(&retention_text(&report.retention));
    out.push_str(&format!(
        "largest sessions (top {} of {}):\n",
        report.largest_sessions.len(),
        report.session_count
    ));
    for entry in &report.largest_sessions {
        out.push_str(&format!(
            "  {}: {}, {} msgs\n",
            entry.session_id,
            format_bytes(entry.bytes),
            entry.messages
        ));
    }
    out.push_str(&format!(
        "oldest sessions (top {} of {}):\n",
        report.oldest_sessions.len(),
        report.session_count
    ));
    for entry in &report.oldest_sessions {
        out.push_str(&format!(
            "  {}: effective activity {}\n",
            entry.session_id,
            rfc3339(entry.effective_activity)
        ));
    }
    out
}

pub fn render_session_text(report: &SessionStatsDetail) -> String {
    let mut out = format!(
        "Session storage for '{}' (agent '{}') on cluster '{}' (collected {})\n",
        report.session_id,
        agent_label(&report.agent),
        report.cluster,
        rfc3339(report.collected_at)
    );
    out.push_str(&format!(
        "metadata: revision {}, created {}\n",
        report.metadata_revision,
        rfc3339(report.metadata_created_at)
    ));
    match &report.activity {
        Some(activity) => out.push_str(&format!(
            "activity: last {}{}\n",
            rfc3339(activity.last_activity_at),
            activity
                .first_activation_at
                .map(|first| format!(", first activation {}", rfc3339(first)))
                .unwrap_or_default()
        )),
        None => out.push_str("activity: (no activity record)\n"),
    }
    match &report.stream {
        Some(stream) => {
            out.push_str(&format!(
                "stream: {} present, {}, {} msgs\n",
                stream.name,
                format_bytes(stream.bytes),
                stream.messages
            ));
            out.push_str(&format!(
                "  created {}, last message {}\n",
                rfc3339(stream.created_at),
                optional_rfc3339(stream.last_message_time)
            ));
            out.push_str(&format!(
                "  first seq {}, last seq {}\n",
                optional_seq(stream.first_sequence),
                optional_seq(stream.last_sequence)
            ));
        }
        None => out.push_str("stream: (no session stream)\n"),
    }
    let holds = &report.holds;
    out.push_str(&format!(
        "holds: session lease {}, a2a lease {}, a2a unresolved context {}, a2a unsettled first reservation {}\n",
        yes_no(holds.session_lease),
        yes_no(holds.a2a_lease),
        yes_no(holds.a2a_unresolved_context),
        yes_no(holds.a2a_unsettled_first_reservation)
    ));
    out.push_str(&format!(
        "journal rows: {} ({})\n",
        report.journal_rows.count,
        bucket_presence(report.journal_rows.present)
    ));
    out.push_str(&format!(
        "checkpoints: {} ({})\n",
        report.checkpoints.count,
        bucket_presence(report.checkpoints.present)
    ));
    out.push_str(&format!(
        "plans: {} plans, {} documents ({})\n",
        report.plans.plans,
        report.plans.documents,
        bucket_presence(report.plans.present)
    ));
    out.push_str(&format!(
        "attachments: {} objects, {} logical ({})\n",
        report.attachments.count,
        format_bytes(report.attachments.logical_bytes),
        bucket_presence(report.attachments.present)
    ));
    out
}

fn optional_seq(seq: Option<u64>) -> String {
    seq.map(|value| value.to_string())
        .unwrap_or_else(|| "(none)".to_string())
}

fn yes_no(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

fn bucket_presence(present: bool) -> &'static str {
    if present {
        "bucket present"
    } else {
        "bucket absent"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn time(days_before: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 - days_before * 86_400, 0)
            .single()
            .expect("time")
    }

    fn cluster_report() -> ClusterStatsReport {
        ClusterStatsReport {
            view: "cluster",
            cluster: "prod".to_string(),
            collected_at: time(0),
            session_count: 4,
            streams: harnx_runtime::session_stats::SessionStreamTotals {
                total: breakdown(3, 6144, 3),
                matched: breakdown(2, 4096, 2),
                orphan_mapped: breakdown(1, 2048, 1),
                orphan_unmapped: breakdown(0, 0, 0),
                metadata_only_sessions: 1,
            },
            activity: harnx_runtime::session_stats::report::ActivitySummary {
                age_counts: AgeCounts {
                    under_1_day: 2,
                    one_to_7_days: 0,
                    seven_to_30_days: 0,
                    over_30_days: 2,
                },
                oldest_effective_activity: Some(time(40)),
                newest_effective_activity: Some(time(0)),
            },
            shared_buckets: vec![harnx_runtime::session_stats::SharedBucketReport {
                bucket: "harnx_sessions",
                stream: "KV_harnx_sessions",
                present: true,
                bytes: 2432,
                messages: 8,
            }],
            agents: vec![harnx_runtime::session_stats::AgentTotals {
                agent: StatsAgentSource::Named {
                    name: "alpha".to_string(),
                },
                session_count: 2,
                streams: breakdown(2, 4096, 2),
                age_counts: AgeCounts::default(),
            }],
            retention: RetentionReport {
                configured_days: Some(30),
                enabled: true,
                age_expired: 2,
                held: 1,
                eligible: 1,
                held_session_lease: 1,
                held_a2a_lease: 0,
                held_unresolved_context: 0,
                held_unsettled_first_reservation: 0,
            },
        }
    }

    fn breakdown(
        count: u64,
        bytes: u64,
        messages: u64,
    ) -> harnx_runtime::session_stats::StreamBreakdown {
        harnx_runtime::session_stats::StreamBreakdown {
            count,
            bytes,
            messages,
        }
    }

    #[test]
    fn cluster_text_covers_every_section_and_reconciles() {
        let out = render_cluster_text(&cluster_report());
        assert!(out.contains("Session storage on cluster 'prod'"), "{out}");
        assert!(
            out.contains("sessions: 4 metadata-backed (1 metadata-only, no stream)"),
            "{out}"
        );
        assert!(
            out.contains("session streams: 3 total, 6.0 KiB, 3 msgs"),
            "{out}"
        );
        assert!(
            out.contains("matched to metadata: 2 streams, 4.0 KiB, 2 msgs"),
            "{out}"
        );
        assert!(
            out.contains("ages: <1d 2, 1-7d 0, 7-30d 0, 30d+ 2"),
            "{out}"
        );
        assert!(
            out.contains("harnx_sessions: present, 2.4 KiB, 8 msgs"),
            "{out}"
        );
        assert!(out.contains("alpha: 2 sessions, 4.0 KiB, 2 msgs"), "{out}");
        assert!(out.contains("retention: 30 days (enabled)"), "{out}");
        assert!(
            out.contains("age-expired: 2, held: 1, eligible: 1 (snapshot estimate"),
            "{out}"
        );
    }

    #[test]
    fn agent_text_lists_largest_and_oldest_sessions() {
        let report = AgentStatsReport {
            view: "agent",
            cluster: "prod".to_string(),
            collected_at: time(0),
            agent: StatsAgentSource::Named {
                name: "alpha".to_string(),
            },
            session_count: 2,
            streams: breakdown(2, 4096, 2),
            metadata_only_sessions: 0,
            activity: harnx_runtime::session_stats::report::ActivitySummary {
                age_counts: AgeCounts::default(),
                oldest_effective_activity: None,
                newest_effective_activity: None,
            },
            retention: RetentionReport::default(),
            largest_sessions: vec![harnx_runtime::session_stats::SessionSizeEntry {
                session_id: "s-big".to_string(),
                storage_key: "k".to_string(),
                bytes: 2048,
                messages: 1,
            }],
            oldest_sessions: vec![harnx_runtime::session_stats::SessionAgeEntry {
                session_id: "s-old".to_string(),
                storage_key: "k".to_string(),
                effective_activity: time(40),
            }],
        };
        let out = render_agent_text(&report);
        assert!(
            out.contains("Session storage for agent 'alpha' on cluster 'prod'"),
            "{out}"
        );
        assert!(out.contains("largest sessions (top 1 of 2)"), "{out}");
        assert!(out.contains("  s-big: 2.0 KiB, 1 msgs"), "{out}");
        assert!(out.contains("  s-old: effective activity "), "{out}");
        // Empty scope: no fabricated timestamps.
        assert!(
            out.contains("activity: oldest (none), newest (none)"),
            "{out}"
        );
        assert!(out.contains("retention: unset (disabled)"), "{out}");
    }

    #[test]
    fn session_text_labels_holds_and_owned_resources() {
        let report = SessionStatsDetail {
            view: "session",
            cluster: "prod".to_string(),
            collected_at: time(0),
            agent: StatsAgentSource::Named {
                name: "alpha".to_string(),
            },
            session_id: "s-1".to_string(),
            storage_key: "k".to_string(),
            metadata_revision: 3,
            metadata_created_at: time(2),
            activity: Some(harnx_runtime::nats_session_metadata::SessionActivity {
                first_activation_at: Some(time(2)),
                last_activity_at: time(1),
            }),
            stream_present: true,
            stream: Some(harnx_runtime::session_stats::SessionStreamSummary {
                name: "SESSION_k".to_string(),
                bytes: 1024,
                messages: 1,
                first_sequence: Some(1),
                last_sequence: Some(1),
                created_at: time(2),
                last_message_time: Some(time(1)),
            }),
            holds: harnx_runtime::session_stats::SessionHolds {
                session_lease: true,
                a2a_lease: false,
                a2a_unresolved_context: false,
                a2a_unsettled_first_reservation: false,
            },
            journal_rows: harnx_runtime::session_stats::OwnedCount {
                present: true,
                count: 5,
            },
            checkpoints: harnx_runtime::session_stats::OwnedCount {
                present: false,
                count: 0,
            },
            plans: harnx_runtime::session_stats::OwnedPlans {
                present: true,
                plans: 2,
                documents: 7,
            },
            attachments: harnx_runtime::session_stats::OwnedAttachments {
                present: true,
                count: 3,
                logical_bytes: 2048,
            },
        };
        let out = render_session_text(&report);
        assert!(
            out.contains("Session storage for 's-1' (agent 'alpha')"),
            "{out}"
        );
        assert!(out.contains("metadata: revision 3, created "), "{out}");
        assert!(
            out.contains("stream: SESSION_k present, 1.0 KiB, 1 msgs"),
            "{out}"
        );
        assert!(out.contains("first seq 1, last seq 1"), "{out}");
        assert!(
            out.contains("holds: session lease yes, a2a lease no"),
            "{out}"
        );
        assert!(out.contains("journal rows: 5 (bucket present)"), "{out}");
        assert!(out.contains("checkpoints: 0 (bucket absent)"), "{out}");
        assert!(
            out.contains("plans: 2 plans, 7 documents (bucket present)"),
            "{out}"
        );
        assert!(
            out.contains("attachments: 3 objects, 2.0 KiB logical"),
            "{out}"
        );
    }

    #[test]
    fn session_text_handles_missing_stream_activity_and_sequences() {
        let report = SessionStatsDetail {
            view: "session",
            cluster: "prod".to_string(),
            collected_at: time(0),
            agent: StatsAgentSource::Inline,
            session_id: "s-2".to_string(),
            storage_key: "k".to_string(),
            metadata_revision: 1,
            metadata_created_at: time(0),
            activity: None,
            stream_present: false,
            stream: None,
            holds: harnx_runtime::session_stats::SessionHolds::default(),
            journal_rows: harnx_runtime::session_stats::OwnedCount {
                present: false,
                count: 0,
            },
            checkpoints: harnx_runtime::session_stats::OwnedCount {
                present: false,
                count: 0,
            },
            plans: harnx_runtime::session_stats::OwnedPlans::default(),
            attachments: harnx_runtime::session_stats::OwnedAttachments::default(),
        };
        let out = render_session_text(&report);
        assert!(out.contains("(agent '<inline>')"), "{out}");
        assert!(out.contains("activity: (no activity record)"), "{out}");
        assert!(out.contains("stream: (no session stream)"), "{out}");
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
