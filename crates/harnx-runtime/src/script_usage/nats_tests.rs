//! Real-broker tests for the script usage collectors. These spawn a
//! `nats-server`, so their test names contain `nats` to land in the
//! `broker-e2e` groups the `.config/nextest.toml` filters select.
use super::collect::{collect_from, collect_session_from};
use super::report::{ScriptUsageScope, INLINE_AGENT_LABEL};
use super::session::TimeWindow;
use crate::nats_session_log::{stream_name_for_session, subject_for_session, NatsSessionLog};
use crate::nats_session_metadata::{
    activity_key, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use anyhow::{Context as _, Result};
use async_nats::jetstream;
use chrono::{Duration, Utc};
use harnx_core::agent_config::AgentVariables;
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::tool::ToolCall;
use serde_json::json;
use std::cell::Cell;

/// Create a session's metadata and activity, and optionally a log holding one
/// finished `bash` round. Returns the storage key.
async fn seed_session(
    js: &jetstream::Context,
    store: &SessionMetadataStore,
    initializer: SessionInitializer,
    session_id: &str,
    with_round: bool,
) -> Result<String> {
    let metadata = SessionMetadata::new(session_id, initializer);
    store.create(&metadata).await?;
    let storage_key = metadata.storage_key();
    store.touch_activity(&storage_key).await?;
    if with_round {
        append_bash_round(js, &storage_key, "a").await?;
    }
    Ok(storage_key)
}

async fn seed_named(
    js: &jetstream::Context,
    store: &SessionMetadataStore,
    agent: &str,
    session_id: &str,
    with_round: bool,
) -> Result<String> {
    let initializer = SessionInitializer::named(agent, AgentVariables::default());
    seed_session(js, store, initializer, session_id, with_round).await
}

async fn append_bash_round(
    js: &jetstream::Context,
    storage_key: &str,
    call_id: &str,
) -> Result<()> {
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key.to_string(), 1);
    let call = ToolCall {
        name: "bash".into(),
        arguments: json!({"script": "echo hi"}),
        id: Some(call_id.into()),
        thought_signature: None,
        reasoning_provenance: None,
    };
    let text = "exit 0\n<!-- start stderr -->\n<!-- end stderr -->\n## inner call trace\n- fs_read ok 12B 3ms\n";
    let result = ToolOutput {
        id: Some(call_id.into()),
        name: "bash".into(),
        output: json!({
            "status": "ok", "exit_code": 0,
            "stdout_truncated": false, "stderr_truncated": false, "limit_hit": null,
            "content": [{"type": "text", "text": text}],
        }),
        markdown: None,
        content: vec![],
        user_surfaces: vec![],
        switch_agent: None,
    };
    let entries = [
        SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![call],
            timestamp: Some(Utc::now()),
            fence_token: None,
        },
        SessionLogEntry::ToolResults {
            results: vec![result],
            handoff_selection: None,
            timestamp: None,
        },
        SessionLogEntry::TurnEnd {
            through_seq: 0,
            fence_token: 0,
            timestamp: Some(Utc::now()),
            usage: None,
        },
    ];
    for entry in &entries {
        log.append_event_async(entry).await?;
    }
    Ok(())
}

async fn backdate_activity(
    store: &SessionMetadataStore,
    storage_key: &str,
    days: i64,
) -> Result<()> {
    let activity = json!({"last_activity_at": (Utc::now() - Duration::days(days)).to_rfc3339()});
    store
        .kv_store()
        .put(
            activity_key(storage_key),
            serde_json::to_vec(&activity)?.into(),
        )
        .await
        .with_context(|| format!("backdate activity for '{storage_key}'"))?;
    Ok(())
}

/// A per-test broker and a JetStream context on it; `None` when no
/// `nats-server` binary is available.
async fn broker() -> Result<Option<(NatsServerHandle, jetstream::Context)>> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(None);
    };
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    Ok(Some((server, js)))
}

/// [`broker`] plus the metadata bucket, created.
async fn broker_with_store(
) -> Result<Option<(NatsServerHandle, jetstream::Context, SessionMetadataStore)>> {
    let Some((server, js)) = broker().await? else {
        return Ok(None);
    };
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    Ok(Some((server, js, store)))
}

/// Append a message that is not a log entry, which fails the whole read of
/// that session.
async fn corrupt_log(js: &jetstream::Context, storage_key: &str) -> Result<()> {
    js.publish(
        subject_for_session(storage_key),
        "{{{ not: [an entry".into(),
    )
    .await?
    .await?;
    Ok(())
}

fn pattern(pattern: &str) -> ScriptUsageScope {
    ScriptUsageScope {
        agent_pattern: Some(pattern.into()),
        window: TimeWindow::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_script_usage_collects_matching_agents_only() -> Result<()> {
    let Some((server, js, store)) = broker_with_store().await? else {
        return Ok(());
    };
    seed_named(&js, &store, "pantheon/atlas", "s1", true).await?;
    seed_named(&js, &store, "coding/coder", "s2", true).await?;
    let inline =
        SessionInitializer::inline("do things", AgentVariables::default(), Default::default());
    seed_session(&js, &store, inline, "s3", true).await?;

    let selected = Cell::new(None);
    let report = collect_from(&js, "c", &pattern("pantheon/*"), &|n| selected.set(Some(n))).await?;
    assert_eq!(selected.get(), Some(1));
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.totals.scripts, 1);
    assert_eq!(report.totals.inner_calls, 1);
    assert_eq!(
        report.by_agent.keys().collect::<Vec<_>>(),
        ["pantheon/atlas"]
    );
    assert_eq!(report.agent_pattern.as_deref(), Some("pantheon/*"));

    let all = collect_from(&js, "c", &ScriptUsageScope::default(), &|_| {}).await?;
    assert_eq!(all.sessions_scanned, 3);
    assert_eq!(all.sessions_with_scripts, 3);
    assert_eq!(all.totals.scripts, 3);
    assert!(all.by_agent.contains_key(INLINE_AGENT_LABEL));
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_script_usage_skips_sessions_inactive_before_since() -> Result<()> {
    let Some((server, js, store)) = broker_with_store().await? else {
        return Ok(());
    };
    seed_named(&js, &store, "alpha", "fresh", true).await?;
    let old = seed_named(&js, &store, "alpha", "old", true).await?;
    backdate_activity(&store, &old, 40).await?;

    let scope = ScriptUsageScope {
        agent_pattern: None,
        window: TimeWindow {
            since: Some(Utc::now() - Duration::days(7)),
            until: None,
        },
    };
    let report = collect_from(&js, "c", &scope, &|_| {}).await?;
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.totals.scripts, 1);
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_script_usage_skips_sessions_created_after_until() -> Result<()> {
    let Some((server, js, store)) = broker_with_store().await? else {
        return Ok(());
    };
    seed_named(&js, &store, "alpha", "early", true).await?;
    let mut late = SessionMetadata::new(
        "late",
        SessionInitializer::named("alpha", AgentVariables::default()),
    );
    late.created_at = Utc::now() + Duration::days(2);
    store.create(&late).await?;
    store.touch_activity(&late.storage_key()).await?;
    append_bash_round(&js, &late.storage_key(), "z").await?;

    let scope = ScriptUsageScope {
        agent_pattern: None,
        window: TimeWindow {
            since: None,
            until: Some(Utc::now() + Duration::days(1)),
        },
    };
    let selected = Cell::new(None);
    let report = collect_from(&js, "c", &scope, &|n| selected.set(Some(n))).await?;
    assert_eq!(selected.get(), Some(1));
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.totals.scripts, 1);
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_script_usage_counts_an_unreadable_session() -> Result<()> {
    let Some((server, js, store)) = broker_with_store().await? else {
        return Ok(());
    };
    let bad = seed_named(&js, &store, "alpha", "bad", true).await?;
    corrupt_log(&js, &bad).await?;
    seed_named(&js, &store, "alpha", "good", true).await?;

    let report = collect_from(&js, "c", &ScriptUsageScope::default(), &|_| {}).await?;
    assert_eq!(report.sessions_unreadable, 1);
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.totals.scripts, 1);
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_script_usage_creates_nothing() -> Result<()> {
    let Some((server, js)) = broker().await? else {
        return Ok(());
    };

    let selected = Cell::new(None);
    let report = collect_from(&js, "c", &ScriptUsageScope::default(), &|n| {
        selected.set(Some(n))
    })
    .await?;
    assert_eq!(selected.get(), Some(0));
    assert_eq!(report.sessions_scanned, 0);
    assert!(
        js.get_key_value("harnx_sessions").await.is_err(),
        "an absent metadata bucket stays absent"
    );

    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let key = seed_named(&js, &store, "alpha", "bare", false).await?;
    let report = collect_from(&js, "c", &ScriptUsageScope::default(), &|_| {}).await?;
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.sessions_unreadable, 0);
    assert!(
        js.get_stream(stream_name_for_session(&key)).await.is_err(),
        "a session without a log gets no stream"
    );
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_script_usage_lists_each_script() -> Result<()> {
    let Some((server, js, store)) = broker_with_store().await? else {
        return Ok(());
    };
    let key = seed_named(&js, &store, "alpha", "one", true).await?;
    append_bash_round(&js, &key, "b").await?;

    let detail = collect_session_from(&js, "c", "alpha", "one").await?;
    assert_eq!(
        (detail.agent.as_str(), detail.session_id.as_str()),
        ("alpha", "one")
    );
    assert_eq!(detail.scripts.len(), 2);
    assert_eq!(detail.totals.scripts, 2);
    assert_eq!(detail.tools[0].name, "fs_read");
    assert_eq!(detail.tools[0].inner_calls, 2);
    drop(server);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_script_usage_rejects_an_unknown_session() -> Result<()> {
    let Some((server, js)) = broker().await? else {
        return Ok(());
    };
    let error = collect_session_from(&js, "c", "alpha", "ghost")
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("'ghost'"), "{error:#}");

    let store = SessionMetadataStore::ensure(&js, 1).await?;
    seed_named(&js, &store, "alpha", "real", false).await?;
    let error = collect_session_from(&js, "c", "alpha", "ghost")
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("'ghost'"), "{error:#}");
    drop(server);
    Ok(())
}
