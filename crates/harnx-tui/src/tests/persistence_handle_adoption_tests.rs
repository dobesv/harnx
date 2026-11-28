//! Regression tests for C5-D4: adopting the persistence handle initialized by
//! isolated bound commands (`.empty session`, broker `.compact session`) onto
//! a sinkless selected existing session under full target/storage/epoch fences,
//! without overwriting newer handles, installing on cancellation/error/stale
//! completions, or installing in read-only history loads.

use super::bound_completion_fencing_tests::install_parked_command_gate;
use super::cross_session_transcript_action_tests::{
    append_completed_turn, archive_session_picker, open_log_for_session, recv_history_event,
    send_key, setup_nats_mutation_test_env, submit_input_line,
};
use super::*;
use crate::types::TuiEvent;
use crossterm::event::{KeyCode, KeyModifiers};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::session::{
    active_session_sink_with_storage_key, install_session_sink_if_missing, SessionAppendSink,
};
use harnx_runtime::config::{load_session_for_render, session_metadata_for_agent};
use std::sync::Arc;

struct SentinelSink;

impl SessionAppendSink for SentinelSink {
    fn append(&self, _entry: &SessionLogEntry) -> anyhow::Result<u64> {
        Ok(1)
    }
}

fn fresh_cluster_readback_config(source: &GlobalConfig) -> Config {
    let mut fresh = Config::default();
    let cfg = source.read();
    fresh.clients = cfg.clients.clone();
    fresh.model = cfg.model.clone();
    fresh.nats_servers = cfg.nats_servers.clone();
    fresh
}

async fn select_archive_session_via_picker(tui: &mut Tui, target: &str, origin: &str) {
    tui.app.modal = Some(archive_session_picker(target, origin));
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
    tui.flush_history_work_for_test().await;
}

async fn switch_archive_session_while_command_parked(tui: &mut Tui, target: &str, origin: &str) {
    tui.app.modal = Some(archive_session_picker(target, origin));
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
    if tui.history_rebuild_abort.is_some() {
        let seq = tui.history_rebuild_seq;
        let done = recv_history_event(
            tui,
            |e| matches!(e, TuiEvent::HistoryRebuildFinished(done) if done.seq == seq),
        )
        .await;
        tui.handle_tui_event(done).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sinkless_selected_session_empty_and_broker_compact_adopt_sink_for_durable_overrides() {
    if !super::has_nats_server_binary() {
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();

    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    config.write().clients.push(ClientConfig::OpenAIConfig(
        harnx_core::provider_config::openai::OpenAIConfig {
            name: "openai".to_string(),
            ..Default::default()
        },
    ));
    append_completed_turn(&log_a, "A turn 1").await;
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B turn 1").await;

    let mut tui = Tui::init(&config).await.unwrap();
    // Navigate B -> A via SessionPicker so A is a freshly selected existing
    // session with no installed persistence sink, even after read-only history loads.
    select_archive_session_via_picker(&mut tui, "session-b", "session-a").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());
    select_archive_session_via_picker(&mut tui, "session-a", "session-b").await;
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_none(),
        "read-only history load must leave selected session sinkless"
    );

    // 1. Successful `.empty session` adopts the persistence handle so subsequent
    // `.model` and `.set temperature` changes persist durably.
    submit_input_line(&mut tui, ".empty session").await;
    let empty_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line == ".empty session"),
    )
    .await;
    tui.handle_tui_event(empty_done).await.unwrap();
    tui.flush_history_work_for_test().await;
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_some(),
        "successful .empty session must preserve its initialized persistence handle"
    );

    submit_input_line(&mut tui, ".model openai:gpt-4o-mini").await;
    submit_input_line(&mut tui, ".set temperature 0.4").await;

    let fresh = fresh_cluster_readback_config(&tui.config);
    let (_, meta_after_empty) =
        session_metadata_for_agent(&fresh, "archive@mutation-cluster", "session-a")
            .await
            .unwrap();
    assert_eq!(
        meta_after_empty.overrides.model.as_deref(),
        Some("openai:gpt-4o-mini")
    );
    assert_eq!(meta_after_empty.overrides.temperature, Some(0.4));
    let rendered_after_empty =
        load_session_for_render(&fresh, Some("mutation-cluster"), "session-a", "archive")
            .await
            .unwrap();
    assert_eq!(rendered_after_empty.model().id(), "openai:gpt-4o-mini");
    assert_eq!(rendered_after_empty.temperature(), Some(0.4));

    // 2. Reselect A as sinkless again and run broker `.compact session`:
    // the broker compaction handle is preserved so subsequent overrides persist.
    append_completed_turn(&log_a, "A turn 2").await;
    select_archive_session_via_picker(&mut tui, "session-b", "session-a").await;
    select_archive_session_via_picker(&mut tui, "session-a", "session-b").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());

    submit_input_line(&mut tui, ".compact session").await;
    let compact_done = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line == ".compact session")
    })
    .await;
    tui.handle_tui_event(compact_done).await.unwrap();
    tui.flush_history_work_for_test().await;
    assert!(
        log_a
            .load_events_async()
            .await
            .unwrap()
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::CompactRequest { .. })),
        "broker .compact session should append CompactRequest"
    );
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_some(),
        "successful broker .compact session must preserve its initialized persistence handle"
    );

    submit_input_line(&mut tui, ".model openai:gpt-4o").await;
    submit_input_line(&mut tui, ".set temperature 0.7").await;

    let (_, meta_after_compact) =
        session_metadata_for_agent(&fresh, "archive@mutation-cluster", "session-a")
            .await
            .unwrap();
    assert_eq!(
        meta_after_compact.overrides.model.as_deref(),
        Some("openai:gpt-4o")
    );
    assert_eq!(meta_after_compact.overrides.temperature, Some(0.7));
    let rendered_after_compact =
        load_session_for_render(&fresh, Some("mutation-cluster"), "session-a", "archive")
            .await
            .unwrap();
    assert_eq!(rendered_after_compact.model().id(), "openai:gpt-4o");
    assert_eq!(rendered_after_compact.temperature(), Some(0.7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cancelled_failed_and_newer_sink_fences_prevent_unwanted_sink_installation() {
    if !super::has_nats_server_binary() {
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();

    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A turn 1").await;
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B turn 1").await;

    let mut tui = Tui::init(&config).await.unwrap();
    let (parked, release) = install_parked_command_gate(&mut tui, ".empty");

    // 1. Stale completion after A -> B must not install A's handle in B.
    select_archive_session_via_picker(&mut tui, "session-b", "session-a").await;
    select_archive_session_via_picker(&mut tui, "session-a", "session-b").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());

    submit_input_line(&mut tui, ".empty session").await;
    parked.notified().await;
    switch_archive_session_while_command_parked(&mut tui, "session-b", "session-a").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());

    release.notify_one();
    let stale_b_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line == ".empty session"),
    )
    .await;
    tui.handle_tui_event(stale_b_done).await.unwrap();
    tui.flush_history_work_for_test().await;
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_none(),
        "stale A completion must not install a persistence handle on B"
    );

    // 2. Stale completion after A -> B -> A (newer epoch on A) must not install
    // the prior epoch's handle on the newer A incarnation.
    select_archive_session_via_picker(&mut tui, "session-a", "session-b").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());
    submit_input_line(&mut tui, ".empty session").await;
    parked.notified().await;
    switch_archive_session_while_command_parked(&mut tui, "session-b", "session-a").await;
    switch_archive_session_while_command_parked(&mut tui, "session-a", "session-b").await;
    assert!(active_session_sink_with_storage_key(&tui.config).is_none());

    release.notify_one();
    let stale_a_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line == ".empty session"),
    )
    .await;
    tui.handle_tui_event(stale_a_done).await.unwrap();
    tui.flush_history_work_for_test().await;
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_none(),
        "stale A completion from an earlier epoch must not install on a newer A epoch"
    );

    // 3. Same-target Ctrl-C cancellation must neither install the handle nor
    // leave rows optimistically actionable before post-settlement read completes.
    submit_input_line(&mut tui, ".empty session").await;
    parked.notified().await;
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    release.notify_one();
    let cancelled_done = recv_history_event(
        &mut tui,
        |e| matches!(e, TuiEvent::HistoryCommandFinished { line, .. } if line == ".empty session"),
    )
    .await;
    tui.handle_tui_event(cancelled_done).await.unwrap();
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_none(),
        "cancelled command must not install a persistence handle"
    );
    assert!(
        tui.actionable_transcript_binding().is_none(),
        "cancelled command must not optimistically rearm transcript rows"
    );
    tui.flush_history_work_for_test().await;

    // 4. Error result on completion must not install an adopted sink, and
    // existing/newer installed handles are never overwritten.
    let key_a = tui.current_session_storage_key().unwrap();
    let sentinel: Arc<dyn SessionAppendSink> = Arc::new(SentinelSink);
    let err_event = TuiEvent::HistoryCommandFinished {
        line: ".empty session".to_string(),
        prev_session: Some("session-a".to_string()),
        prev_agent: Some("archive@mutation-cluster".to_string()),
        output: String::new(),
        result: Err(anyhow::anyhow!("injected failure")),
        adopted_sink: Some((key_a.clone(), sentinel.clone())),
    };
    tui.transcript_view_ownership
        .session_fence
        .running_bound_target = Some(tui.current_session_command_target());
    tui.handle_tui_event(err_event).await.unwrap();
    assert!(
        active_session_sink_with_storage_key(&tui.config).is_none(),
        "failed command must not install an adopted sink"
    );

    assert!(install_session_sink_if_missing(
        &tui.config,
        &key_a,
        sentinel.clone()
    ));
    let replacement: Arc<dyn SessionAppendSink> = Arc::new(SentinelSink);
    assert!(
        !install_session_sink_if_missing(&tui.config, &key_a, replacement),
        "must not overwrite an already installed persistence handle"
    );
    let (_, active) = active_session_sink_with_storage_key(&tui.config).unwrap();
    assert!(Arc::ptr_eq(&active, &sentinel));
}
