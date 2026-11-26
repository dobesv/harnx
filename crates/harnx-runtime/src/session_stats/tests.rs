//! Pure aggregation and identity tests. The broker-backed request-shape and
//! no-write tests live with the other real-NATS tests; these exercise the
//! production joins without a broker.
use super::aggregate;
use super::report::{StatsAgentSource, AGENT_TOP_LIST_LIMIT};
use super::retention;
use super::snapshot::{self, SessionInput, StreamInput};
use crate::nats_session_metadata::{SessionMetadata, SessionSnapshot, SnapshotEntry};
use async_nats::jetstream::kv;
use chrono::{DateTime, Duration, Utc};
use harnx_core::agent_config::AgentVariables;
use std::collections::HashSet;

fn input(now: DateTime<Utc>, agent: &str, session: &str, activity: Duration) -> SessionInput {
    SessionInput {
        storage_key: format!("{agent}/{session}"),
        session_id: session.to_string(),
        agent: if agent == "<inline>" {
            StatsAgentSource::Inline
        } else {
            StatsAgentSource::Named {
                name: agent.to_string(),
            }
        },
        effective_activity: now - activity,
        a2a_holds: Default::default(),
    }
}

/// The A2A registry key the fence derives from a session's storage key.
fn a2a_registry_key(storage_key: &str) -> String {
    format!("a2a.registry.{}", harnx_core::crypto::sha256(storage_key))
}

fn stream(key: Option<&str>, bytes: u64, messages: u64) -> StreamInput {
    StreamInput {
        storage_key: key.map(str::to_string),
        bytes,
        messages,
    }
}

#[test]
fn cluster_report_counts_and_classifies_all_streams() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "alpha", "s1", Duration::days(0)),
        input(now, "beta", "s2", Duration::days(2)),
    ];
    let streams = vec![
        stream(Some("alpha/s1"), 100, 2),
        stream(Some("ghost/s9"), 50, 1),
        stream(None, 10, 1),
    ];
    let report = aggregate::build_cluster_report(
        "c",
        &aggregate::ReportScope::new(now, &sessions, &streams, &HashSet::new(), None),
        Vec::new(),
    );
    assert_eq!(report.session_count, 2);
    assert_eq!(report.streams.total.count, 3);
    assert_eq!(report.streams.matched.bytes, 100);
    assert_eq!(report.streams.orphan_mapped.bytes, 50);
    assert_eq!(report.streams.orphan_unmapped.bytes, 10);
    assert!(
        report.streams.total.bytes
            == report.streams.matched.bytes
                + report.streams.orphan_mapped.bytes
                + report.streams.orphan_unmapped.bytes
    );
    assert_eq!(report.streams.metadata_only_sessions, 1);
    assert_eq!(report.activity.age_counts.under_1_day, 1);
    assert_eq!(report.activity.age_counts.one_to_7_days, 1);
    assert_eq!(report.agents.len(), 2);
    // Retention disabled: no eligibility numbers.
    assert!(!report.retention.enabled);
    assert_eq!(report.retention.age_expired, 0);
}

#[test]
fn age_buckets_cover_all_sessions_exactly_once() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "a", "fresh", Duration::hours(1)),
        input(now, "a", "day", Duration::days(3)),
        input(now, "a", "week", Duration::days(10)),
        input(now, "a", "month", Duration::days(40)),
    ];
    let report = aggregate::build_cluster_report(
        "c",
        &aggregate::ReportScope::new(now, &sessions, &[], &HashSet::new(), None),
        Vec::new(),
    );
    let counts = report.activity.age_counts;
    let total =
        counts.under_1_day + counts.one_to_7_days + counts.seven_to_30_days + counts.over_30_days;
    assert_eq!(total, 4);
    assert_eq!(counts.under_1_day, 1);
    assert_eq!(counts.one_to_7_days, 1);
    assert_eq!(counts.seven_to_30_days, 1);
    assert_eq!(counts.over_30_days, 1);
}

#[test]
fn retention_threshold_is_strict_and_holds_are_exact() {
    let now = Utc::now();
    // Ten days minus one second: still fresh.
    let s1 = input(
        now,
        "a",
        "just-new",
        Duration::days(10) - Duration::seconds(1),
    );
    // Exactly the cutoff: not expired, the comparison is strict.
    let mut s2 = input(now, "a", "exactly-old", Duration::days(0));
    s2.effective_activity = now - Duration::seconds(10 * 24 * 60 * 60);
    let s3 = input(now, "a", "leased", Duration::days(11));
    let s4 = input(now, "a", "a2a-leased", Duration::days(11));
    let mut s5 = input(now, "a", "context-hold", Duration::days(11));
    s5.a2a_holds.unresolved_context = true;
    let mut s6 = input(now, "a", "reservation-hold", Duration::days(11));
    s6.a2a_holds.unsettled_first_reservation = true;
    let s7 = input(now, "a", "free", Duration::days(11));
    let sessions = vec![s1, s2, s3, s4, s5, s6, s7];
    let mut leases = HashSet::new();
    leases.insert(crate::nats_lease::NatsLeaseConfig::default().key_for_session("a/leased"));
    leases.insert("sessions/a/a2a-leased/a2a/lock".to_string());
    let report = retention::retention_report(now, &sessions, &leases, Some(10));
    // s2 is exactly at the cutoff: not expired (strict comparison).
    assert_eq!(report.age_expired, 5);
    assert_eq!(report.held, 4);
    assert_eq!(report.eligible, 1);
    assert_eq!(report.held_session_lease, 1);
    assert_eq!(report.held_a2a_lease, 1);
    assert_eq!(report.held_unresolved_context, 1);
    assert_eq!(report.held_unsettled_first_reservation, 1);
    assert_eq!(report.age_expired, report.held + report.eligible);
    // A zero-day configuration is disabled, same as no configuration.
    let disabled = retention::retention_report(now, &sessions, &leases, Some(0));
    assert!(!disabled.enabled);
}

#[test]
fn agent_report_excludes_inline_and_other_agents() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "alpha", "s1", Duration::days(0)),
        input(now, "beta", "s2", Duration::days(0)),
        input(now, "<inline>", "s3", Duration::days(0)),
    ];
    let streams = vec![
        stream(Some("alpha/s1"), 100, 2),
        stream(Some("beta/s2"), 999, 9),
    ];
    let report = aggregate::build_agent_report(
        "c",
        "alpha",
        &aggregate::ReportScope::new(now, &sessions, &streams, &HashSet::new(), None),
    );
    assert_eq!(report.session_count, 1);
    assert_eq!(report.streams.bytes, 100);
    assert_eq!(report.largest_sessions.len(), 1);
    assert_eq!(report.largest_sessions[0].session_id, "s1");
}

#[test]
fn agent_top_lists_are_truncated_and_sorted() {
    let now = Utc::now();
    let sessions: Vec<SessionInput> = (0..25)
        .map(|i| input(now, "alpha", &format!("s{i:02}"), Duration::days(i)))
        .collect();
    let streams: Vec<StreamInput> = (0..25)
        .map(|i| {
            let key = format!("alpha/s{i:02}");
            stream(Some(&key), 1000 - i, 1)
        })
        .collect();
    let report = aggregate::build_agent_report(
        "c",
        "alpha",
        &aggregate::ReportScope::new(now, &sessions, &streams, &HashSet::new(), None),
    );
    assert_eq!(report.largest_sessions.len(), AGENT_TOP_LIST_LIMIT);
    assert_eq!(report.oldest_sessions.len(), AGENT_TOP_LIST_LIMIT);
    assert_eq!(report.largest_sessions[0].session_id, "s00");
    // s00 holds the biggest stream and the freshest activity (0 days old);
    // s24 has been idle longest, so it heads the oldest list.
    assert_eq!(report.oldest_sessions[0].session_id, "s24");
    assert!(report
        .largest_sessions
        .windows(2)
        .all(|w| w[0].bytes >= w[1].bytes));
    assert!(report
        .oldest_sessions
        .windows(2)
        .all(|w| w[0].effective_activity <= w[1].effective_activity));
}

/// 10,000 synthetic sessions — the bulk path's throughput check. Totals must
/// reconcile: every stream classified once, every session counted once, age
/// counts summing to the session count, retention held+eligible balancing.
#[test]
fn synthetic_ten_thousand_sessions_aggregate_consistently() {
    let now = Utc::now();
    let agents = 50;
    let per_agent = 200;
    let mut sessions = Vec::with_capacity(agents * per_agent);
    let mut streams = Vec::new();
    for agent in 0..agents {
        for i in 0..per_agent {
            let name = format!("agent-{agent:02}");
            let session_id = format!("s{agent:02}x{i:03}");
            let mut session = input(now, &name, &session_id, Duration::days((i % 45) as i64));
            session.storage_key = format!("{name}/{session_id}");
            if i % 7 == 0 {
                session.a2a_holds.unresolved_context = true;
            }
            sessions.push(session);
            if i % 4 != 0 {
                streams.push(stream(
                    Some(&format!("{name}/{session_id}")),
                    10 * (i as u64 + 1),
                    i as u64,
                ));
            }
        }
    }
    let orphan = stream(Some("ghost/orphan"), 42, 1);
    streams.push(orphan);
    assert_eq!(sessions.len(), 10_000);
    let mut leases = HashSet::new();
    for session in &sessions {
        if session.effective_activity < now - Duration::days(30) {
            leases.insert(
                crate::nats_lease::NatsLeaseConfig::default().key_for_session(&session.storage_key),
            );
            leases.insert(format!("sessions/{}/a2a/lock", session.storage_key));
        }
    }
    let report = aggregate::build_cluster_report(
        "synthetic",
        &aggregate::ReportScope::new(now, &sessions, &streams, &leases, Some(30)),
        Vec::new(),
    );
    assert_eq!(report.session_count, 10_000);
    assert_eq!(report.agents.len(), agents);
    assert_eq!(
        report.agents.iter().map(|a| a.session_count).sum::<u64>(),
        10_000
    );
    assert!(report
        .agents
        .windows(2)
        .all(|w| w[0].streams.bytes >= w[1].streams.bytes));
    assert_eq!(report.streams.total.count, streams.len() as u64);
    assert_eq!(
        report.streams.total.bytes,
        report.streams.matched.bytes
            + report.streams.orphan_mapped.bytes
            + report.streams.orphan_unmapped.bytes
    );
    assert_eq!(report.streams.orphan_mapped.bytes, 42);
    assert_eq!(
        report.streams.metadata_only_sessions,
        (agents * per_agent / 4) as u64
    );
    let counts = report.activity.age_counts;
    assert_eq!(
        counts.under_1_day + counts.one_to_7_days + counts.seven_to_30_days + counts.over_30_days,
        10_000
    );
    assert_eq!(
        report.activity.oldest_effective_activity,
        Some(now - Duration::days(44))
    );
    assert_eq!(
        report.retention.age_expired,
        report.retention.held + report.retention.eligible
    );
    assert_eq!(
        report.retention.held_session_lease,
        report.retention.held_a2a_lease
    );
    let agent_report = aggregate::build_agent_report(
        "synthetic",
        "agent-07",
        &aggregate::ReportScope::new(now, &sessions, &streams, &leases, Some(30)),
    );
    assert_eq!(agent_report.session_count, per_agent as u64);
    assert!(agent_report.streams.bytes <= report.streams.total.bytes);
}

#[test]
fn identity_is_structured_never_a_display_string() {
    let inline = StatsAgentSource::Inline;
    let json = serde_json::to_string(&inline).expect("serializes");
    assert_eq!(json, r#"{"kind":"inline"}"#);
    let named = StatsAgentSource::Named {
        name: "<inline>".to_string(),
    };
    let named_json = serde_json::to_string(&named).expect("serializes");
    assert_eq!(named_json, r#"{"kind":"named","name":"<inline>"}"#);
    assert_ne!(json, named_json);
    // Round-trips and stays distinct from the inline group.
    let parsed: StatsAgentSource = serde_json::from_str(&named_json).expect("deserializes");
    assert_eq!(parsed, named);
    assert_ne!(parsed, inline);
    assert_ne!(parsed, StatsAgentSource::Inline);
}

#[test]
fn cluster_agent_totals_sort_bytes_desc_with_identity_tiebreak() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "zeta", "s1", Duration::days(0)),
        input(now, "alpha", "s2", Duration::days(0)),
        input(now, "<inline>", "s3", Duration::days(0)),
    ];
    let streams = vec![
        stream(Some("zeta/s1"), 100, 1),
        stream(Some("alpha/s2"), 100, 1),
        stream(Some("<inline>/s3"), 5, 1),
    ];
    let report = aggregate::build_cluster_report(
        "c",
        &aggregate::ReportScope::new(now, &sessions, &streams, &HashSet::new(), None),
        Vec::new(),
    );
    assert_eq!(report.agents.len(), 3);
    // Equal bytes tie-break on identity; inline is its own group, not "<inline>".
    assert_eq!(
        report.agents[0].agent,
        StatsAgentSource::Named {
            name: "alpha".into()
        }
    );
    assert_eq!(
        report.agents[1].agent,
        StatsAgentSource::Named {
            name: "zeta".into()
        }
    );
    assert_eq!(report.agents[2].agent, StatsAgentSource::Inline);
}

fn snapshot_entry(payload: &[u8], operation: kv::Operation) -> SnapshotEntry {
    SnapshotEntry {
        operation,
        payload: payload.to_vec().into(),
        revision: 1,
    }
}

/// Pure coverage for the bulk-snapshot joins: Delete/Purge markers on a
/// session's meta key drop it from the session list (the same way cleanup's
/// per-session reads would), missing activity falls back to the metadata's
/// creation time, and A2A holds come from the same snapshot's marker-aware
/// facts.
#[test]
fn snapshot_joins_skip_marker_sessions_and_keep_marker_aware_holds() {
    let live = SessionMetadata::new(
        "live",
        crate::nats_session_metadata::SessionInitializer::named("alpha", AgentVariables::default()),
    );
    let marked = SessionMetadata::new(
        "marked",
        crate::nats_session_metadata::SessionInitializer::named("alpha", AgentVariables::default()),
    );
    let live_key = live.storage_key();
    let meta_entry =
        |metadata: &SessionMetadata, operation: kv::Operation| -> (String, SnapshotEntry) {
            (
                format!("sessions/{}/meta", metadata.storage_key()),
                SnapshotEntry {
                    operation,
                    payload: serde_json::to_vec(metadata).unwrap().into(),
                    revision: 1,
                },
            )
        };
    let snapshot = SessionSnapshot::from_entries(vec![
        meta_entry(&live, kv::Operation::Put),
        // A Purge marker on the meta key: the session's canonical record is
        // gone even though the key still exists.
        meta_entry(&marked, kv::Operation::Purge),
        // No activity record for `live`: effective activity is metadata's
        // created_at.
    ]);
    let sessions = snapshot::snapshot_sessions(&snapshot).expect("joins the snapshot");
    assert_eq!(sessions.len(), 1, "marker sessions are skipped");
    assert_eq!(sessions[0].session_id, "live");
    assert_eq!(sessions[0].storage_key, live_key);
    assert_eq!(
        sessions[0].effective_activity, live.created_at,
        "missing activity falls back to metadata created_at"
    );
    assert!(!sessions[0].a2a_holds.any(), "no A2A facts: no holds");

    // A live unresolved context in the same snapshot holds the session.
    let snapshot = SessionSnapshot::from_entries(vec![
        meta_entry(&live, kv::Operation::Put),
        (
            format!("sessions/{live_key}/activity"),
            snapshot_entry(
                br#"{"last_activity_at":"2026-01-01T00:00:00Z"}"#,
                kv::Operation::Put,
            ),
        ),
        (
            format!("sessions/{live_key}/a2a/context"),
            snapshot_entry(
                br#"{"state":{"active":{"kind":"busy"}}}"#,
                kv::Operation::Put,
            ),
        ),
    ]);
    let sessions = snapshot::snapshot_sessions(&snapshot).expect("joins the snapshot");
    assert!(sessions[0].a2a_holds.unresolved_context);
    // The activity record wins over the metadata fallback.
    assert_ne!(sessions[0].effective_activity, live.created_at);
    assert_eq!(
        sessions[0].effective_activity,
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    );

    // A Delete marker on the context means no live A2A admission state, so
    // the context is resolved — marker state survives the snapshot read.
    let snapshot = SessionSnapshot::from_entries(vec![
        meta_entry(&live, kv::Operation::Put),
        (
            format!("sessions/{live_key}/a2a/context"),
            snapshot_entry(b"", kv::Operation::Delete),
        ),
    ]);
    let sessions = snapshot::snapshot_sessions(&snapshot).expect("joins the snapshot");
    assert!(!sessions[0].a2a_holds.any(), "marker context is no hold");
}

/// Per-agent stream totals count only streams the agent's sessions own: a
/// metadata-only session is a session, not a stream, or the per-agent counts
/// stop reconciling with the cluster view.
#[test]
fn agent_stream_totals_exclude_metadata_only_sessions() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "alpha", "s1", Duration::days(1)),
        input(now, "alpha", "s2", Duration::days(1)),
    ];
    let streams = vec![stream(Some("alpha/s1"), 100, 2)];
    let live_lease_keys = HashSet::new();
    let scope = aggregate::ReportScope::new(now, &sessions, &streams, &live_lease_keys, None);
    let report = aggregate::build_agent_report("test", "alpha", &scope);
    assert_eq!(report.session_count, 2);
    assert_eq!(report.metadata_only_sessions, 1);
    assert_eq!(
        report.streams.count, 1,
        "a metadata-only session owns no stream and must not count as one"
    );
    assert_eq!(report.streams.bytes, 100);
    assert_eq!(report.streams.messages, 2);

    // The same scope reconciles with the cluster view: summing the per-agent
    // stream counts never double-counts metadata-only sessions.
    let cluster = aggregate::build_cluster_report("test", &scope, Vec::new());
    let agent_streams: u64 = cluster.agents.iter().map(|agent| agent.streams.count).sum();
    assert_eq!(
        agent_streams, cluster.streams.matched.count,
        "per-agent stream counts must reconcile with matched streams"
    );
    assert_eq!(cluster.streams.metadata_only_sessions, 1);
}

/// Exact age boundaries are half-open: one day old is not "under one day",
/// seven is not "one to seven", thirty is not "seven to thirty", and a
/// future timestamp lands in the newest bucket rather than erroring.
#[test]
fn age_bucket_edges_are_half_open_with_future_in_newest() {
    let now = Utc::now();
    let sessions = vec![
        input(now, "a", "future", Duration::days(-1)),
        input(now, "a", "exactly-1d", Duration::days(1)),
        input(now, "a", "exactly-7d", Duration::days(7)),
        input(now, "a", "exactly-30d", Duration::days(30)),
    ];
    let live_lease_keys = HashSet::new();
    let scope = aggregate::ReportScope::new(now, &sessions, &[], &live_lease_keys, None);
    let report = aggregate::build_cluster_report("test", &scope, Vec::new());
    let counts = report.activity.age_counts;
    assert_eq!(
        (
            counts.under_1_day,
            counts.one_to_7_days,
            counts.seven_to_30_days,
            counts.over_30_days
        ),
        (1, 1, 1, 1),
        "exact boundary ages land in the older bucket; a future age is not an error"
    );
    assert_eq!(
        report.activity.newest_effective_activity,
        Some(sessions[0].effective_activity),
        "a future timestamp is the newest activity"
    );
    assert_eq!(
        report.activity.oldest_effective_activity,
        Some(sessions[3].effective_activity)
    );
}

/// The A2A join derives the archive key exactly when the fence would inspect
/// it: a marker context or a non-null active state settles the
/// first-reservation rule before the reservation payload is ever parsed, so a
/// malformed reservation the fence would never read cannot fail the join.
#[test]
fn snapshot_holds_skip_reservation_parsing_where_the_fence_skips() {
    let live = SessionMetadata::new(
        "live",
        crate::nats_session_metadata::SessionInitializer::named("alpha", AgentVariables::default()),
    );
    let live_key = live.storage_key();
    let meta = |operation: kv::Operation| {
        (
            format!("sessions/{live_key}/meta"),
            SnapshotEntry {
                operation,
                payload: serde_json::to_vec(&live).unwrap().into(),
                revision: 1,
            },
        )
    };
    let registry_entry = || {
        (
            a2a_registry_key(&live_key),
            snapshot_entry(
                br#"{"first":{"identity":{"user":"alice"}}}"#,
                kv::Operation::Put,
            ),
        )
    };
    let reservation_entry = |payload: &[u8]| {
        (
            format!(
                "a2a/first-messages/{}",
                harnx_core::crypto::sha256(r#"{"user":"alice"}"#)
            ),
            snapshot_entry(payload, kv::Operation::Put),
        )
    };

    // A Delete marker on the context skips the first-reservation check, so a
    // reservation payload the fence would never read cannot fail the join.
    let snapshot = SessionSnapshot::from_entries(vec![
        meta(kv::Operation::Put),
        (
            format!("sessions/{live_key}/a2a/context"),
            snapshot_entry(b"", kv::Operation::Delete),
        ),
        registry_entry(),
        reservation_entry(b"not json"),
    ]);
    let sessions = snapshot::snapshot_sessions(&snapshot)
        .expect("a marker context never reads the reservation");
    assert!(!sessions[0].a2a_holds.any());

    // A non-null active state settles the reservation rule before the task
    // identity is validated: a malformed task id must not fail the join.
    let snapshot = SessionSnapshot::from_entries(vec![
        meta(kv::Operation::Put),
        (
            format!("sessions/{live_key}/a2a/context"),
            snapshot_entry(
                br#"{"state":{"active":{"stop_confirmed":true}}}"#,
                kv::Operation::Put,
            ),
        ),
        registry_entry(),
        reservation_entry(
            format!(r#"{{"allocation":{{"storage_key":"{live_key}","task_id":"nodot"}}}}"#)
                .as_bytes(),
        ),
    ]);
    let sessions = snapshot::snapshot_sessions(&snapshot)
        .expect("a settled active state never validates the task identity");
    assert!(sessions[0].a2a_holds.unresolved_context);
    assert!(
        !sessions[0].a2a_holds.unsettled_first_reservation,
        "the settled active state satisfies the reservation rule"
    );
}

/// The bulk join follows the fence's short-circuit order with a malformed
/// reservation paired against every context state: unresolved holds without
/// parsing it, malformed context errors before it, the registry record
/// parses first, a marker context skips it, and a retired context still
/// parses it — exactly where the fence's `ensure_first_settled` does.
#[test]
fn snapshot_hold_facts_follow_the_fence_short_circuit_order() {
    let live = SessionMetadata::new(
        "live",
        crate::nats_session_metadata::SessionInitializer::named("alpha", AgentVariables::default()),
    );
    let live_key = live.storage_key();
    let sha256 = harnx_core::crypto::sha256;
    let meta = (
        format!("sessions/{live_key}/meta"),
        SnapshotEntry {
            operation: kv::Operation::Put,
            payload: serde_json::to_vec(&live).unwrap().into(),
            revision: 1,
        },
    );
    let registry = (
        a2a_registry_key(&live_key),
        snapshot_entry(
            br#"{"first":{"identity":{"user":"alice"}}}"#,
            kv::Operation::Put,
        ),
    );
    let malformed_registry = (
        a2a_registry_key(&live_key),
        snapshot_entry(b"not json", kv::Operation::Put),
    );
    let context = |payload: &[u8]| {
        (
            format!("sessions/{live_key}/a2a/context"),
            snapshot_entry(payload, kv::Operation::Put),
        )
    };
    let marker_context = (
        format!("sessions/{live_key}/a2a/context"),
        snapshot_entry(b"", kv::Operation::Delete),
    );
    let malformed_reservation = (
        format!("a2a/first-messages/{}", sha256(r#"{"user":"alice"}"#)),
        snapshot_entry(b"not json", kv::Operation::Put),
    );
    let join = |entries: Vec<(String, SnapshotEntry)>| {
        snapshot::snapshot_sessions(&SessionSnapshot::from_entries(entries))
    };
    let unresolved = br#"{"state":{"active":{"stop_confirmed":false}}}"#;
    let retired = br#"{"state":{"active":{"snapshot":{"task":{"status":{"state":"TASK_STATE_COMPLETED"}}},"stop_confirmed":true,"projections":{"archive":true,"message_mapping":true,"final_event":true},"publication":{"pending":null}}}}"#;
    let malformed = b"not json" as &[u8];

    // An unresolved context short-circuits before the reservation is parsed.
    let sessions = join(vec![
        meta.clone(),
        context(unresolved),
        registry.clone(),
        malformed_reservation.clone(),
    ])
    .expect("an unresolved context never reads the reservation");
    assert!(sessions[0].a2a_holds.unresolved_context);
    assert!(!sessions[0].a2a_holds.unsettled_first_reservation);

    // A malformed context errors before its reservation.
    let error = join(vec![
        meta.clone(),
        context(malformed),
        registry.clone(),
        malformed_reservation.clone(),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("context"),
        "context error first, got: {error:#}"
    );

    // The registry record parses first, even under a marker context.
    let error = join(vec![
        meta.clone(),
        marker_context.clone(),
        malformed_registry.clone(),
        malformed_reservation.clone(),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("registry"),
        "registry error first, got: {error:#}"
    );

    // A retired context still parses the reservation JSON, exactly where the
    // fence does, but never its task identity.
    let error = join(vec![
        meta.clone(),
        context(retired),
        registry.clone(),
        malformed_reservation.clone(),
    ])
    .unwrap_err();
    assert!(
        error.to_string().contains("reservation"),
        "retired context still parses the reservation, got: {error:#}"
    );

    // Control: a marker context skips the reservation entirely.
    let sessions = join(vec![meta, marker_context, registry, malformed_reservation])
        .expect("a marker context skips the reservation");
    assert!(!sessions[0].a2a_holds.any());
}
