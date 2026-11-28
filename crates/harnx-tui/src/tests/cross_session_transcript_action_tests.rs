//! Regression tests for B1: cross-session durable transcript action safety.
//!
//! Exercises actual UI key and event dispatch (`handle_key`, `handle_tui_event`)
//! across gated A->B history loads, cancelled reads leaving prior rows visible,
//! confirmations opened before navigation and resolved after, queued durable
//! mutations, rapid same-session incarnation changes, and same-target repair.

use super::*;
use crate::history_work::RebuildMode;
use crate::types::{ModalState, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::{watch, Notify};

fn picker_meta(id: &str) -> harnx_runtime::config::SessionMeta {
    harnx_runtime::config::SessionMeta {
        id: id.to_string(),
        session_id: Some(id.to_string()),
        agent_name: Some("test-agent".to_string()),
        title: None,
        user_id: None,
        modified: None,
        contexts: vec![],
        unread: false,
    }
}

pub(super) fn active_session_id(tui: &Tui) -> Option<String> {
    tui.config
        .read()
        .session
        .as_ref()
        .map(|s| s.id().to_string())
}

fn user_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::UserText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
    }
}

fn assistant_row(seq: usize, text: &str) -> TranscriptItem {
    TranscriptItem::AssistantText {
        text: text.to_string(),
        seq: Some(seq),
        timestamp: None,
        rendered_cache: None,
    }
}

fn record_dispatched_commands(tui: &mut Tui) -> Arc<Mutex<Vec<String>>> {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tui.command_override = Some(Arc::new(move |line| {
        sink.lock().unwrap().push(line.to_string());
    }));
    recorded
}

fn install_gated_session_loader(
    tui: &mut Tui,
    session_b_rows: Vec<TranscriptItem>,
    session_a_reload_rows: Vec<TranscriptItem>,
) -> (Arc<Notify>, watch::Sender<bool>) {
    let load_started = Arc::new(Notify::new());
    let (release_tx, release_rx) = watch::channel(false);
    let started_tx = load_started.clone();
    let a_call_count = Arc::new(AtomicUsize::new(0));
    tui.history_load_override = Some(Arc::new(move |cfg, _mode, abort| {
        let started_tx = started_tx.clone();
        let mut release_rx = release_rx.clone();
        let session_b_rows = session_b_rows.clone();
        let session_a_reload_rows = session_a_reload_rows.clone();
        let a_call_count = a_call_count.clone();
        Box::pin(async move {
            let sid = cfg
                .read()
                .session
                .as_ref()
                .map(|s| s.id().to_string())
                .unwrap_or_default();
            started_tx.notify_one();
            tokio::select! {
                _ = harnx_core::abort::wait_abort_signal(&abort) => {
                    anyhow::bail!("history load aborted")
                }
                _ = release_rx.wait_for(|released| *released) => {}
            }
            if sid == "session-b" {
                Ok(session_b_rows)
            } else {
                a_call_count.fetch_add(1, Ordering::SeqCst);
                Ok(session_a_reload_rows)
            }
        })
    }));
    (load_started, release_tx)
}

pub(super) async fn send_key(tui: &mut Tui, code: KeyCode, modifiers: KeyModifiers) {
    Box::pin(tui.handle_key(KeyEvent::new(code, modifiers)))
        .await
        .unwrap();
}

pub(super) async fn submit_input_line(tui: &mut Tui, text: &str) {
    tui.set_input_text(text);
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
}

pub(super) async fn open_session_picker_and_select(tui: &mut Tui, target_session: &str) {
    tui.app.modal = Some(ModalState::SessionPicker {
        sessions: vec![picker_meta(target_session)],
        selected: 1,
        origin_agent: Some("test-agent".to_string()),
        origin_session: active_session_id(tui),
        error: None,
    });
    send_key(tui, KeyCode::Enter, KeyModifiers::NONE).await;
}

#[tokio::test]
async fn gated_session_switch_retires_old_rows_and_blocks_actions_through_cancel_until_b_commits() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![
        user_row(1, "Session A prompt"),
        assistant_row(2, "Session A reply"),
    ];
    let recorded = record_dispatched_commands(&mut tui);
    let (load_started, release_load) = install_gated_session_loader(
        &mut tui,
        vec![user_row(1, "Session B prompt")],
        vec![user_row(1, "Session A reloaded")],
    );

    // Focus row on A and open its detail view before switching to B.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    assert_eq!(tui.app.transcript_focus, Some(1));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert!(tui.app.detail_view_open);

    // Switch A -> B via SessionPicker Enter; B's history load is gated open.
    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert!(tui.history_rebuild_abort.is_some());
    assert!(
        !tui.app.detail_view_open
            && tui.app.transcript_focus.is_none()
            && !tui.app.transcript_browsing,
        "switching to B must immediately retire prior focus, browse mode, and detail overlay"
    );

    // Even if user browses onto A's visible rows during the gated read,
    // delete / rewind / edit and typed durable mutations must all be rejected.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    for key in ['d', 'r', 'e'] {
        send_key(&mut tui, KeyCode::Char(key), KeyModifiers::NONE).await;
        assert!(
            tui.app.modal.is_none(),
            "old row key '{key}' must not open a confirmation while B is loading"
        );
        assert!(tui.app.footer_notice_text().is_some());
    }

    // Exit browsing mode via Esc and cancel B's gated read via Ctrl-C, which
    // can leave A's old rows on screen; they must stay retired/non-actionable against B.
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    tui.flush_history_work_for_test().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    for key in ['d', 'r', 'e'] {
        send_key(&mut tui, KeyCode::Char(key), KeyModifiers::NONE).await;
        assert!(
            tui.app.modal.is_none(),
            "old row key '{key}' must stay retired after Ctrl-C cancelled B's load"
        );
    }

    // Typed dot-command mutations must also be refused before B's view commits
    // (both through the test command seam and through real run_config_command).
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    for cmd in [".delete message 1", ".rewind 1", ".edit message 1"] {
        submit_input_line(&mut tui, cmd).await;
    }
    assert!(
        recorded.lock().unwrap().is_empty(),
        "no mutation may reach B before B's own view commits: {:?}",
        *recorded.lock().unwrap()
    );
    tui.command_override = None;
    for cmd in [".delete message 1", ".rewind 1", ".edit message 1"] {
        submit_input_line(&mut tui, cmd).await;
    }
    assert!(tui.history_command_queue.is_empty());
    assert!(!tui.history_command_running);
    tui.command_override = Some({
        let sink = recorded.clone();
        Arc::new(move |line| {
            sink.lock().unwrap().push(line.to_string());
        })
    });

    // Now complete B's load cleanly and verify actions on B's committed view succeed.
    tui.schedule_history_rebuild_for_command(true);
    load_started.notified().await;
    let _ = release_load.send(true);
    tui.flush_history_work_for_test().await;

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { from: 1, to: 1, .. })
    ));
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        [".delete message 1"],
        "action explicitly chosen from B's committed view must succeed"
    );
}

#[tokio::test]
async fn confirmation_opened_before_navigation_is_retired_and_stale_resolution_is_rejected() {
    // Path 1: Delete confirmation opened on A, retired by NewSessionReserved to B.
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Session A prompt")];
    let recorded = record_dispatched_commands(&mut tui);
    let (_load_started, release_load) = install_gated_session_loader(
        &mut tui,
        vec![user_row(1, "Session B prompt")],
        vec![user_row(1, "Session A prompt")],
    );

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { from: 1, to: 1, .. })
    ));

    tui.new_session_reservation_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(TuiEvent::NewSessionReserved {
        seq: tui.new_session_reservation_seq,
        session_id: "session-b".to_string(),
        origin_agent: Some("test-agent".to_string()),
        origin_session: Some("session-a".to_string()),
    })
    .await
    .unwrap();
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert!(
        tui.app.modal.is_none(),
        "navigation via NewSessionReserved must retire A's open delete confirmation"
    );
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert!(recorded.lock().unwrap().is_empty());
    let _ = release_load.send(true);
    tui.flush_history_work_for_test().await;

    // Path 2: Rewind confirmation opened on A, retired by HandoffCommitted to B.
    let _env_lock = crate::test_utils::ENV_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::test_utils::TestEnvironment::set(temp.path());
    struct NonBrokerNatsEnvGuard {
        prior_url: Option<std::ffi::OsString>,
        prior_token: Option<std::ffi::OsString>,
    }
    impl Drop for NonBrokerNatsEnvGuard {
        fn drop(&mut self) {
            match &self.prior_url {
                Some(v) => std::env::set_var("HARNX_NATS_URL", v),
                None => std::env::remove_var("HARNX_NATS_URL"),
            }
            match &self.prior_token {
                Some(v) => std::env::set_var("HARNX_NATS_TOKEN", v),
                None => std::env::remove_var("HARNX_NATS_TOKEN"),
            }
        }
    }
    let _non_broker_env = NonBrokerNatsEnvGuard {
        prior_url: std::env::var_os("HARNX_NATS_URL"),
        prior_token: std::env::var_os("HARNX_NATS_TOKEN"),
    };
    std::env::set_var("HARNX_NATS_URL", "nats://127.0.0.1:1");
    std::env::set_var("HARNX_NATS_TOKEN", "non-broker-test");
    create_agent_stubs(&temp.path().join("agents"), &["test-agent"]);
    let config2 = test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui2 = Tui::init(&config2).await.unwrap();
    tui2.app.transcript = vec![user_row(1, "A secret prompt")];
    let recorded2 = record_dispatched_commands(&mut tui2);
    let (_load_started2, release_load2) = install_gated_session_loader(
        &mut tui2,
        vec![user_row(1, "Session B prompt")],
        vec![user_row(1, "A secret prompt")],
    );

    send_key(&mut tui2, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui2, KeyCode::Char('r'), KeyModifiers::NONE).await;
    let captured_rewind_modal = tui2.app.modal.clone().expect("rewind modal opened");
    assert!(matches!(
        captured_rewind_modal,
        ModalState::ConfirmRewind { seq: 1, .. }
    ));

    let handoff_event = handoff_committed_event(&mut tui2, "session-b");
    tui2.handle_tui_event(handoff_event).await.unwrap();
    assert_eq!(active_session_id(&tui2).as_deref(), Some("session-b"));
    assert!(
        tui2.app.modal.is_none(),
        "HandoffCommitted must retire A's open rewind confirmation"
    );

    // Even if that pre-navigation confirmation is resolved after B loads,
    // binding validation must reject it and leave B's input draft intact.
    let _ = release_load2.send(true);
    tui2.flush_history_work_for_test().await;
    tui2.app.modal = Some(captured_rewind_modal);
    send_key(&mut tui2, KeyCode::Enter, KeyModifiers::NONE).await;
    assert!(tui2.app.modal.is_none());
    assert!(
        recorded2.lock().unwrap().is_empty(),
        "stale pre-navigation rewind confirmation must not mutate B"
    );
    assert!(
        tui2.app.input.lines().join("").is_empty(),
        "rejected stale rewind must not paste A's user text into B's input"
    );
    assert!(tui2.app.footer_notice_text().is_some());
}

#[tokio::test]
async fn queued_durable_mutations_and_rapid_same_session_incarnations_reject_stale_writes() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Incarnation 1 prompt")];
    let (_load_started, release_load) = install_gated_session_loader(
        &mut tui,
        vec![user_row(1, "Session B prompt")],
        vec![user_row(1, "Incarnation 2 prompt")],
    );

    // 1. Capture a bound edit/delete target from A (rev 1) and queue a mutation
    // through real UI key dispatch (without command_override) while a history
    // command is already running, then verify switching A -> B drops it.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    let rev1_delete_modal = tui.app.modal.clone().expect("delete modal on A rev 1");
    let rev1_target = tui
        .actionable_transcript_binding()
        .expect("initial A rev 1 binding");

    // Start a real history command (.session) and press 'y' via handle_key to
    // queue `.delete message 1` bound to A (rev 1) without command_override.
    tui.enqueue_history_command(".session".to_string());
    assert!(tui.history_command_running);
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(tui.history_command_queue.len(), 1);
    assert!(tui
        .history_command_queue
        .front()
        .is_some_and(|cmd| cmd.line == ".delete message 1"
            && cmd.expected_target.as_ref() == Some(&rev1_target)));

    // Switch A -> B before the running history command finishes; retirement drops
    // the queued A-bound mutation immediately so it never runs against B.
    open_session_picker_and_select(&mut tui, "session-b").await;
    assert!(
        tui.history_command_queue.is_empty(),
        "target switch must drop stale queued mutation before it can run on B"
    );
    let _ = release_load.send(true);
    tui.flush_history_work_for_test().await;
    assert!(
        !tui.app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::MutationNotice(text) if text.contains("Deleted entries")
        )),
        "dropped queued mutation must never emit a deletion notice for B"
    );

    // 2. Rapid same-session incarnation A -> B -> A: even though config ends up
    // back on `session-a`, view_revision has advanced so rev-1 confirmations and
    // queued mutations from incarnation 1 are rejected.
    open_session_picker_and_select(&mut tui, "session-a").await;
    let _ = release_load.send(true);
    tui.flush_history_work_for_test().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-a"));

    let rev_after_reenter = tui
        .actionable_transcript_binding()
        .expect("re-entered A has a fresh binding");
    assert!(
        rev_after_reenter.view_revision > rev1_target.view_revision,
        "re-entering same session ID must advance view_revision ({} > {})",
        rev_after_reenter.view_revision,
        rev1_target.view_revision
    );

    // Queueing or resolving an action bound to incarnation 1 must be rejected.
    let recorded = record_dispatched_commands(&mut tui);
    tui.enqueue_bound_history_command(".edit message 1".to_string(), Some(rev1_target.clone()));
    assert!(!tui.history_command_running);
    assert!(tui.history_command_queue.is_empty());

    tui.app.modal = Some(rev1_delete_modal);
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert!(
        recorded.lock().unwrap().is_empty(),
        "incarnation-1 confirmation must not mutate incarnation-2 of session-a"
    );
}

#[tokio::test]
async fn same_session_refresh_retire_old_confirmations_and_preserve_actions_on_failure() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.history_load_override = Some(Arc::new(|_cfg, _mode, _abort| {
        Box::pin(async { Ok(vec![user_row(1, "Current session row")]) })
    }));
    tui.schedule_history_rebuild_for_command(false);
    tui.flush_history_work_for_test().await;
    let initial_revision = tui
        .actionable_transcript_binding()
        .expect("initial session view must be actionable")
        .view_revision;

    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { .. })
    ));
    tui.handle_tui_event(TuiEvent::HistoryRebuildFinished(Box::new(
        new_history_rebuild_completion(
            &tui,
            RebuildMode::SharedRefresh,
            crate::history_work::RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            Ok(vec![user_row(1, "Shared refresh updated row")]),
        ),
    )))
    .await
    .unwrap();
    assert!(
        tui.app.modal.is_none(),
        "refresh retires old row confirmation"
    );
    let refreshed_revision = tui
        .actionable_transcript_binding()
        .expect("binding after SharedRefresh")
        .view_revision;
    assert!(refreshed_revision > initial_revision);

    tui.schedule_history_rebuild_with_mode(RebuildMode::SharedRefresh);
    tui.handle_tui_event(TuiEvent::HistoryRebuildFinished(Box::new(
        new_history_rebuild_completion(
            &tui,
            RebuildMode::SharedRefresh,
            crate::history_work::RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            Err("transient read error".to_string()),
        ),
    )))
    .await
    .unwrap();
    assert!(tui.actionable_transcript_binding().is_some());

    // A tokenless/forged HistoryCommandFinished error cannot manufacture a binding
    // even after manual retirement on the same target.
    tui.retire_transcript_actions_for_pending_mutation();
    tui.handle_tui_event(history_command_finished_event(
        ".delete message 1",
        "",
        Err(anyhow::anyhow!("transient broker timeout")),
    ))
    .await
    .unwrap();
    assert!(
        tui.actionable_transcript_binding().is_none(),
        "tokenless command failure must not manufacture a binding"
    );

    // Restore a clean bound view, then dispatch a real same-target destructive
    // command (`d` -> `y`) that fails cleanly before any durable write; its
    // command-owned retirement token proves provenance and restores actionability.
    tui.schedule_history_rebuild_for_command(false);
    tui.flush_history_work_for_test().await;
    assert!(tui.actionable_transcript_binding().is_some());

    tui.command_override = None;
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::ConfirmDelete { .. })
    ));
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    tui.flush_history_work_for_test().await;
    assert!(
        tui.actionable_transcript_binding().is_some(),
        "failed same-target command with intact history and valid provenance restores actionability; ownership={:?}, transcript_tail={:?}",
        tui.transcript_view_ownership,
        tui.app.transcript.last()
    );

    let recorded = record_dispatched_commands(&mut tui);
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('e'), KeyModifiers::NONE).await;
    assert_eq!(recorded.lock().unwrap().as_slice(), [".edit message 1"]);
}

async fn assert_retained_rows_non_actionable(
    tui: &mut Tui,
    recorded: &Arc<Mutex<Vec<String>>>,
    context: &str,
) {
    assert!(
        tui.actionable_transcript_binding().is_none(),
        "{context}: actionable binding must remain None"
    );
    assert!(
        tui.transcript_view_ownership.active_binding.is_none(),
        "{context}: active_binding must remain None"
    );
    send_key(tui, KeyCode::Up, KeyModifiers::NONE).await;
    for key in ['d', 'r', 'e'] {
        send_key(tui, KeyCode::Char(key), KeyModifiers::NONE).await;
        assert!(
            tui.app.modal.is_none(),
            "{context}: key '{key}' must not open a confirmation modal"
        );
    }
    assert!(
        recorded.lock().unwrap().is_empty(),
        "{context}: key dispatch must not fire a mutation"
    );
    send_key(tui, KeyCode::Esc, KeyModifiers::NONE).await;
    for cmd in [".delete message 1", ".rewind 1", ".edit message 1"] {
        submit_input_line(tui, cmd).await;
    }
    assert!(
        recorded.lock().unwrap().is_empty(),
        "{context}: typed destructive commands must be refused"
    );
    assert!(
        tui.history_command_queue.is_empty(),
        "{context}: queue must stay empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_target_switch_remains_unbound_across_intervening_command_errors() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![
        user_row(1, "Session A prompt"),
        assistant_row(2, "Session A reply"),
    ];

    // Capture a pre-navigation confirmation modal on A to also test stale resolution.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    let stale_a_modal = tui.app.modal.clone().expect("delete modal on A");
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;

    let (load_started, release_load) = install_gated_session_loader(
        &mut tui,
        vec![user_row(1, "Session B prompt")],
        vec![user_row(1, "Session A prompt")],
    );

    // A -> B, then Ctrl-C cancels B's history load leaving A's rows visible.
    open_session_picker_and_select(&mut tui, "session-b").await;
    load_started.notified().await;
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    tui.flush_history_work_for_test().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert!(tui.history_rebuild_abort.is_none());
    assert!(!tui.is_history_repair_pending());

    // Unrelated failures must never manufacture a B binding over A's rows.
    tui.command_override = None;
    submit_input_line(&mut tui, ".set unknown foo").await;
    submit_input_line(&mut tui, ".bogus_cmd").await;
    let recorded = record_dispatched_commands(&mut tui);
    assert_retained_rows_non_actionable(&mut tui, &recorded, "after inline errors on B").await;

    tui.command_override = None;
    submit_input_line(&mut tui, ".session nonexistent").await;
    tui.flush_history_work_for_test().await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    let recorded = record_dispatched_commands(&mut tui);
    assert_retained_rows_non_actionable(&mut tui, &recorded, "after failing history cmds on B")
        .await;

    // Even resolving a previously captured A confirmation modal is rejected.
    tui.app.modal = Some(stale_a_modal);
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert!(tui.app.modal.is_none());
    assert!(recorded.lock().unwrap().is_empty());

    // Only a fenced history commit for B makes B actionable.
    tui.schedule_history_rebuild_for_command(true);
    load_started.notified().await;
    let _ = release_load.send(true);
    tui.flush_history_work_for_test().await;
    assert!(tui.actionable_transcript_binding().is_some());
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('d'), KeyModifiers::NONE).await;
    send_key(&mut tui, KeyCode::Char('y'), KeyModifiers::NONE).await;
    assert_eq!(recorded.lock().unwrap().as_slice(), [".delete message 1"]);
}

#[tokio::test]
async fn failed_mutation_discards_earlier_refresh_and_waits_for_post_settlement_read() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.transcript = vec![user_row(1, "Old row")];
    let (started, release) =
        install_gated_session_loader(&mut tui, vec![], vec![user_row(1, "Fresh row")]);
    let stamp = crate::event_isolation::EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    release.send(true).unwrap();
    let prior_read = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryRebuildFinished(_))
    })
    .await;
    started.notified().await;
    release.send(false).unwrap();

    submit_input_line(&mut tui, ".rewind  invalid").await;
    assert!(tui.history_command_running);
    tui.handle_tui_event(prior_read).await.unwrap();
    tui.schedule_shared_session_refresh(&stamp);
    assert!(tui.history_rebuild_abort.is_none());
    assert!(tui.actionable_transcript_binding().is_none());

    let failed = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { result: Err(_), .. })
    })
    .await;
    tui.handle_tui_event(failed).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    assert!(tui.actionable_transcript_binding().is_none());
    assert!(
        matches!(&tui.app.transcript[0], TranscriptItem::UserText { text, .. } if text == "Old row")
    );
    release.send(true).unwrap();
    tui.flush_history_work_for_test().await;
    assert!(tui.actionable_transcript_binding().is_some());
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|r| matches!(r, TranscriptItem::UserText { text, .. } if text == "Fresh row")));
    assert!(tui.app.transcript.iter().any(
        |r| matches!(r, TranscriptItem::ErrorText(text) if text.contains("Invalid sequence number"))
    ));
}

pub(super) async fn setup_nats_mutation_test_env(
    temp: &tempfile::TempDir,
    agent_name: &str,
    session_id: &str,
) -> (
    harnx_runtime::config::GlobalConfig,
    harnx_runtime::NatsSession,
    harnx_runtime::nats_session_log::NatsSessionLog,
) {
    let config = test_config_with_mock_client_and_agent(agent_name, Some(session_id));
    let mut server = harnx_runtime::config::resolve_local_nats_server_config()
        .await
        .unwrap();
    server.name = "mutation-cluster".to_string();
    {
        let mut cfg = config.write();
        cfg.remote_agent = Some((agent_name.to_string(), server.name.clone()));
        cfg.nats_servers.push(server);
    }
    let session = harnx_runtime::NatsSession::from_global_config(
        harnx_runtime::NatsSessionConfig {
            cluster: "mutation-cluster".to_string(),
            initializer: harnx_runtime::SessionInitializer::named(agent_name, Default::default()),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        &config,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await
    .unwrap();
    let snapshot = config.read().clone();
    let jetstream = snapshot.nats_jetstream("mutation-cluster").await.unwrap();
    let log =
        harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, session.storage_key())
            .with_replicas(1);
    let _ = temp;
    (config, session, log)
}

pub(super) async fn open_log_for_session(
    config: &harnx_runtime::config::GlobalConfig,
    agent_name: &str,
    session_id: &str,
) -> harnx_runtime::nats_session_log::NatsSessionLog {
    let session = harnx_runtime::NatsSession::from_global_config(
        harnx_runtime::NatsSessionConfig {
            cluster: "mutation-cluster".to_string(),
            initializer: harnx_runtime::SessionInitializer::named(agent_name, Default::default()),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        config,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await
    .unwrap();
    let snapshot = config.read().clone();
    let jetstream = snapshot.nats_jetstream("mutation-cluster").await.unwrap();
    harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, session.storage_key())
        .with_replicas(1)
}

fn seed_live_session_message(config: &harnx_runtime::config::GlobalConfig, text: &str) {
    use harnx_core::message::{Message, MessageContent, MessageRole};

    let mut cfg = config.write();
    if let Some(session) = cfg.session.as_mut() {
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(text.to_string()),
        ));
    }
}

async fn append_user_log_entry(log: &harnx_runtime::nats_session_log::NatsSessionLog, text: &str) {
    use harnx_core::{
        message::{MessageContent, MessageRole},
        session::SessionLogEntry,
    };
    log.append_event_async(&SessionLogEntry::Message {
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        id: None,
        timestamp: None,
        fence_token: None,
    })
    .await
    .unwrap();
}

pub(super) async fn append_completed_turn(
    log: &harnx_runtime::nats_session_log::NatsSessionLog,
    text: &str,
) {
    use harnx_core::{
        message::{MessageContent, MessageRole},
        session::SessionLogEntry,
    };
    append_user_log_entry(log, text).await;
    log.append_event_async(&SessionLogEntry::Message {
        role: MessageRole::Assistant,
        content: MessageContent::Text(format!("Reply to {text}")),
        id: None,
        timestamp: None,
        fence_token: None,
    })
    .await
    .unwrap();
    let through_seq = log
        .load_events_async()
        .await
        .unwrap()
        .last()
        .map(|(seq, _)| *seq)
        .unwrap_or(0);
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 0,
        timestamp: None,
        usage: None,
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_after_durable_delete_keeps_rows_unbound_until_read_retry() {
    use std::time::Duration;

    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session, log) =
        setup_nats_mutation_test_env(&temp, "archive", "mutation-test").await;
    append_user_log_entry(&log, "Deleted row").await;
    let mut tui = Tui::init(&config).await.unwrap();
    let seq = tui
        .app
        .transcript
        .iter()
        .find_map(|row| match row {
            TranscriptItem::UserText { text, seq, .. } if text == "Deleted row" => *seq,
            _ => None,
        })
        .expect("initial durable row with a logical coordinate");
    let started = Arc::new(Notify::new());
    let (release, read_gate) = watch::channel(false);
    let notify_read = started.clone();
    tui.history_load_override = Some(Arc::new(move |cfg, _, abort| {
        let mut read_gate = read_gate.clone();
        notify_read.notify_one();
        Box::pin(async move {
            read_gate.wait_for(|ready| *ready).await.unwrap();
            crate::lifecycle::load_session_history_transcript_items(&cfg, Some(&abort)).await
        })
    }));
    tui.history_repair_retry_delay_override = Some(Duration::from_millis(20));

    submit_input_line(&mut tui, &format!(".delete message {seq}")).await;
    let mut completion = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryCommandFinished { .. })
    })
    .await;
    let TuiEvent::HistoryCommandFinished { result, .. } = &mut completion else {
        unreachable!()
    };
    assert!(result.is_ok(), "durable delete at {seq} failed: {result:?}");
    let rows = crate::lifecycle::load_session_history_transcript_items(&config, None)
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "durable history changed before completion delivery"
    );

    // Model a lost acknowledgement at the TUI boundary, with the durable
    // effect already proven above. Ctrl-C must not be interpreted as rollback.
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    *result = Err(anyhow::anyhow!("cancelled after durable progress"));
    tui.handle_tui_event(completion).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    assert!(tui.actionable_transcript_binding().is_none());
    assert!(tui
        .app
        .transcript
        .iter()
        .any(|r| matches!(r, TranscriptItem::UserText { text, .. } if text == "Deleted row")));

    let retry = recv_history_event(&mut tui, |e| {
        matches!(e, TuiEvent::HistoryRepairRetryDue { .. })
    })
    .await;
    tui.handle_tui_event(retry).await.unwrap();
    release.send(true).unwrap();
    tui.flush_history_work_for_test().await;
    assert!(tui.actionable_transcript_binding().is_some());
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|r| matches!(r, TranscriptItem::UserText { text, .. } if text == "Deleted row")));
    let durable_entries = log.load_events_async().await.unwrap().len();
    assert_eq!(durable_entries, 2, "retry must not append another write");
}

pub(super) async fn recv_history_event(
    tui: &mut Tui,
    matches: impl Fn(&TuiEvent) -> bool,
) -> TuiEvent {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = tui.event_rx.recv().await.unwrap();
            if matches(&event) {
                return event;
            }
        }
    })
    .await
    .expect("missing history event")
}

pub(super) fn archive_session_picker(target_session: &str, origin_session: &str) -> ModalState {
    ModalState::SessionPicker {
        sessions: vec![picker_meta(target_session)],
        selected: 1,
        origin_agent: Some("archive".to_string()),
        origin_session: Some(origin_session.to_string()),
        error: None,
    }
}

/// C4-F3 regression: queued session-wide destructive commands (including
/// newly offloaded `.empty  session` and `.reset\trepl` spellings) bind to the
/// submitting session target without requiring transcript row bindings, are
/// dropped on independent picker navigation to B leaving both B and A intact,
/// and still execute on the same target when A stays active.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_session_wide_destructive_spellings_reject_on_navigation_and_run_on_same_target() {
    use harnx_core::session::SessionLogEntry;

    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A secret prompt").await;
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B protected prompt").await;

    let mut tui = Tui::init(&config).await.unwrap();
    let a_initial_entries = log_a.load_events_async().await.unwrap().len();
    let b_initial_entries = log_b.load_events_async().await.unwrap().len();

    // Clear row bindings to prove session-wide destructive commands do NOT
    // require transcript row bindings at submission.
    tui.transcript_view_ownership.active_binding = None;

    // Park a history command on A, then press real Enter on newly offloaded
    // whitespace spellings and ordinary `.empty session`.
    tui.enqueue_history_command(".session".to_string());
    assert!(tui.history_command_running);
    for cmd in [".empty  session", ".reset\trepl", ".empty session"] {
        submit_input_line(&mut tui, cmd).await;
    }
    assert_eq!(tui.history_command_queue.len(), 3);
    assert!(tui
        .history_command_queue
        .iter()
        .all(|cmd| cmd.expected_session.is_some() && cmd.expected_target.is_none()));

    // Independent SessionPicker switch to B drops the queued A-bound commands
    // before A's parked job settles.
    tui.app.modal = Some(archive_session_picker("session-b", "session-a"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    assert!(tui.history_command_queue.is_empty());

    // Seed live in-memory message on B and settle A's parked job.
    seed_live_session_message(&config, "B live state");
    tui.flush_history_work_for_test().await;

    // Neither B nor A received an unintended queued clear; B's live state is intact.
    let b_entries = log_b.load_events_async().await.unwrap();
    assert_eq!(b_entries.len(), b_initial_entries);
    assert!(!b_entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Clear)));
    assert_eq!(config.read().session.as_ref().unwrap().messages.len(), 1);
    assert_eq!(
        log_a.load_events_async().await.unwrap().len(),
        a_initial_entries
    );

    // Same-target positive test: switch back to A, confirm both `.empty  session`
    // (even without row bindings) and `.reset\trepl` work on A.
    tui.app.modal = Some(archive_session_picker("session-a", "session-b"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    tui.flush_history_work_for_test().await;

    seed_live_session_message(&config, "A live state");
    tui.transcript_view_ownership.active_binding = None;
    submit_input_line(&mut tui, ".empty  session").await;
    tui.flush_history_work_for_test().await;
    assert!(matches!(
        log_a
            .load_events_async()
            .await
            .unwrap()
            .last()
            .map(|(_, entry)| entry),
        Some(SessionLogEntry::Clear)
    ));
    assert!(config.read().session.as_ref().unwrap().messages.is_empty());

    append_completed_turn(&log_a, "A second message").await;
    seed_live_session_message(&config, "A live state 2");
    submit_input_line(&mut tui, ".reset\trepl").await;
    tui.flush_history_work_for_test().await;
    let a_after_reset = log_a.load_events_async().await.unwrap();
    assert!(matches!(
        a_after_reset.last().map(|(_, entry)| entry),
        Some(SessionLogEntry::Clear)
    ));
    assert!(config.read().session.as_ref().unwrap().messages.is_empty());
}

/// C4-F3 mid-flight navigation fence: navigating A -> B while `.empty  session`
/// is already executing in the background must keep B's canonical history,
/// live session state, and transcript untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inflight_session_clear_fences_live_cleanup_and_b_history_on_navigation() {
    use harnx_core::session::SessionLogEntry;

    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let _nats_env = NatsEnvReset::clear();
    let (config, _session_a, log_a) =
        setup_nats_mutation_test_env(&temp, "archive", "session-a").await;
    append_completed_turn(&log_a, "A inflight prompt").await;
    let log_b = open_log_for_session(&config, "archive", "session-b").await;
    append_completed_turn(&log_b, "B protected prompt").await;
    let b_initial_entries = log_b.load_events_async().await.unwrap().len();

    let mut tui = Tui::init(&config).await.unwrap();
    submit_input_line(&mut tui, ".empty  session").await;
    assert!(tui.history_command_running);

    // Navigate to B while A's `.empty  session` task is in flight, and give B
    // live in-memory state before A's completion event lands.
    tui.app.modal = Some(archive_session_picker("session-b", "session-a"));
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert_eq!(active_session_id(&tui).as_deref(), Some("session-b"));
    seed_live_session_message(&config, "B live in-memory message");

    tui.flush_history_work_for_test().await;

    let b_entries = log_b.load_events_async().await.unwrap();
    assert_eq!(b_entries.len(), b_initial_entries);
    assert!(!b_entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Clear)));
    assert_eq!(config.read().session.as_ref().unwrap().messages.len(), 1);
    assert!(tui.app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::UserText { text, .. } if text == "B protected prompt"
    )));
}
