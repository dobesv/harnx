//! Dedicated coverage for `history_work.rs` (design 9): the background
//! transcript rebuild's supersession/abort lifecycle, the serialized history
//! command queue, Ctrl-C cancellation of every background job, and the
//! picker/new-session/info-overlay completion events. All deterministic:
//! in-flight jobs are observed through their abort slots and the event queue,
//! never through wall-clock timing.

use super::*;
use crate::history_work::RebuildMode;
use crate::types::ModalState;

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

fn session_id(tui: &Tui) -> Option<String> {
    tui.config
        .read()
        .session
        .as_ref()
        .map(|s| s.id().to_string())
}

/// Picker modal with no Esc-restore origins: the state a session-flow
/// picker reaches in tests. One construction site so the shape cannot drift
/// between the picker tests here.
fn session_picker_modal(
    sessions: Vec<harnx_runtime::config::SessionMeta>,
    selected: usize,
) -> ModalState {
    ModalState::SessionPicker {
        sessions,
        selected,
        origin_agent: None,
        origin_session: None,
        error: None,
    }
}

/// One construction site for reservation success events; the apply and
/// stale cases differ only in `seq`/`session_id`/origin.
fn reserved_event(seq: u64, session_id: &str, origin_agent: Option<&str>) -> TuiEvent {
    TuiEvent::NewSessionReserved {
        seq,
        session_id: session_id.to_string(),
        origin_agent: origin_agent.map(str::to_string),
        origin_session: None,
    }
}

/// The picker snapshot a failed reservation restores: the session list,
/// highlighted row and Esc-restore origins, bundled so the event
/// constructor below stays under the parameter cap.
struct ReservationFailureRestore {
    sessions: Vec<harnx_runtime::config::SessionMeta>,
    selected: usize,
    origin_agent: Option<&'static str>,
    origin_session: Option<&'static str>,
}

/// Restore snapshot with row 0 highlighted and no Esc-origins: the shape
/// a stale failure carries.
fn stale_restore(sessions: Vec<harnx_runtime::config::SessionMeta>) -> ReservationFailureRestore {
    ReservationFailureRestore {
        sessions,
        selected: 0,
        origin_agent: None,
        origin_session: None,
    }
}

/// One construction site for reservation failure events; the apply and
/// stale cases differ in `seq`, error text and the restored picker snapshot.
fn failed_reservation_event(seq: u64, error: &str, restore: ReservationFailureRestore) -> TuiEvent {
    let ReservationFailureRestore {
        sessions,
        selected,
        origin_agent,
        origin_session,
    } = restore;
    TuiEvent::NewSessionReservationFailed {
        seq,
        error: error.to_string(),
        sessions,
        selected,
        origin_agent: origin_agent.map(str::to_string),
        origin_session: origin_session.map(str::to_string),
    }
}

fn transcript_texts(tui: &Tui) -> Vec<String> {
    tui.app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::SystemText(text) => Some(text.clone()),
            TranscriptItem::ErrorText(text) => Some(text.clone()),
            TranscriptItem::StatusLine(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A superseded rebuild's completion (older seq) is discarded without
/// touching the displayed transcript, and the newer rebuild's abort slot
/// survives: it belongs to the newer, still-cancellable load.
#[tokio::test]
async fn history_rebuild_supersession_discards_stale_seq_completion() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.schedule_history_rebuild();
    tui.schedule_history_rebuild();
    let newer_abort = tui
        .history_rebuild_abort
        .clone()
        .expect("newer rebuild owns the abort slot");

    let before = tui.app.transcript.len();
    // Direct apply of the superseded (seq 1) completion.
    let target = tui.current_history_target();
    tui.apply_history_rebuild(crate::types::HistoryRebuildCompletion {
        seq: 1,
        target,
        attachment_gen: tui.session_attachment_gen,
        refresh_rev: tui.history_refresh_rev,
        mutation_fence: tui.transcript_mutation_seq,
        live_stamp: None,
        preserved_from: 0,
        mode: RebuildMode::PreserveTail,
        failure_policy: crate::history_work::RebuildFailurePolicy::KeepRepairOrPreserveSilently,
        result: Ok(vec![]),
    });
    assert_eq!(
        tui.app.transcript.len(),
        before,
        "stale completion must not touch the transcript"
    );
    assert!(
        tui.history_rebuild_abort.is_some(),
        "superseded completion must not clear the newer rebuild's abort slot"
    );

    // The newest rebuild still applies.
    tui.flush_history_work_for_test().await;
    assert!(tui.history_rebuild_abort.is_none());
    assert!(
        !transcript_texts(&tui)
            .iter()
            .any(|text| text.contains("Loading session history…")),
        "applied rebuild replaces the loading status line"
    );
    let _ = newer_abort;
}

/// When the frontend moved to another session/agent, the newest rebuild's
/// completion is discarded AND its abort slot is cleared: nothing is in
/// flight anymore, so nothing may look cancellable (the lingering-slot bug
/// Argus flagged).
#[tokio::test]
async fn history_rebuild_target_mismatch_discards_and_clears_abort() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", Some("session-a"));
    let mut tui = Tui::init(&config).await.unwrap();

    tui.schedule_history_rebuild();
    assert!(tui.history_rebuild_abort.is_some());
    let seq = tui.history_rebuild_seq;

    // The frontend switches to another session while the rebuild loads.
    let stale_target = crate::types::HistoryTarget {
        session_id: Some("session-a".to_string()),
        agent: Some("test-agent".to_string()),
        cluster: super::NON_BROKER_TEST_CLUSTER.to_string(),
    };
    let replacement = {
        let guard = config.read();
        harnx_runtime::config::session::new(&guard, "session-b", None).unwrap()
    };
    config.write().session = Some(replacement);

    let before = tui.app.transcript.len();
    tui.apply_history_rebuild(crate::types::HistoryRebuildCompletion {
        seq,
        target: stale_target,
        attachment_gen: tui.session_attachment_gen,
        refresh_rev: tui.history_refresh_rev,
        mutation_fence: tui.transcript_mutation_seq,
        live_stamp: None,
        preserved_from: 0,
        mode: RebuildMode::PreserveTail,
        failure_policy: crate::history_work::RebuildFailurePolicy::KeepRepairOrPreserveSilently,
        result: Ok(vec![]),
    });
    assert_eq!(
        tui.app.transcript.len(),
        before,
        "target-mismatched completion must not touch the transcript"
    );
    assert!(
        tui.history_rebuild_abort.is_none(),
        "discarded newest rebuild must clear its abort slot: nothing is in flight"
    );

    // The still-arriving real completion for the stale target is discarded
    // the same way, and leaves no pending work behind.
    tui.flush_history_work_for_test().await;
    assert!(tui.history_rebuild_abort.is_none());
    assert_eq!(
        session_id(&tui).as_deref(),
        Some("session-b"),
        "stale rebuild must not switch the session back"
    );
}

/// The applied rebuild clears its abort slot and replaces the loading status
/// line with the loaded view.
#[tokio::test]
async fn history_rebuild_applies_and_clears_abort() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.schedule_history_rebuild();
    assert!(tui.history_rebuild_abort.is_some());

    tui.flush_history_work_for_test().await;
    assert!(
        tui.history_rebuild_abort.is_none(),
        "applied rebuild clears its abort slot"
    );
    assert!(
        !transcript_texts(&tui)
            .iter()
            .any(|text| text.contains("Loading session history…")),
        "loading status line must be gone after the rebuild applies"
    );
}

/// Ctrl-C cancels an in-flight rebuild: the abort slot is taken and set
/// (not left dangling), no completion event ever arrives, and the displayed
/// transcript is preserved as-is.
#[tokio::test]
async fn ctrl_c_cancels_inflight_rebuild_without_lingering_abort() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.schedule_history_rebuild();
    assert!(tui.history_rebuild_abort.is_some());

    let before = tui.app.transcript.clone();
    tui.handle_ctrl_c().await;
    assert!(
        tui.history_rebuild_abort.is_none(),
        "Ctrl-C must take and fire the rebuild's abort, leaving no slot"
    );

    // Flush returns promptly: no completion event exists to wait for, and a
    // lingering Some(abort) here would make it panic on the 10s budget.
    tui.flush_history_work_for_test().await;
    assert_eq!(
        tui.app.transcript.len(),
        before.len(),
        "cancelled rebuild must leave the displayed transcript untouched"
    );
}

/// Ctrl-C while a history command runs: the running command's per-command
/// signal fires, the queued command is dropped (never started), and later
/// work is not poisoned — a fresh command gets a fresh, unset signal.
#[tokio::test]
async fn ctrl_c_cancels_running_history_command_and_clears_queue() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.enqueue_history_command(".session first".to_string());
    assert!(
        tui.history_command_running,
        "first command starts immediately"
    );
    assert!(
        tui.history_command_abort.is_some(),
        "running command owns its abort slot"
    );
    tui.enqueue_history_command(".session second".to_string());
    assert_eq!(
        tui.history_command_queue.len(),
        1,
        "second command must queue behind the running one"
    );

    tui.handle_ctrl_c().await;
    assert!(
        tui.history_command_abort.is_none(),
        "Ctrl-C clears the slot after firing it"
    );
    assert!(
        tui.history_command_queue.is_empty(),
        "Ctrl-C drops queued history commands"
    );

    // The aborted command's completion lands (`.session first` is not a
    // valid explicit target, so it fails) and must not open the session
    // picker — Ctrl-C cannot leave a half-finished command applied.
    tui.flush_history_work_for_test().await;
    assert!(!tui.history_command_running);
    assert!(
        !matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })),
        "cancelled command must not open the session picker"
    );

    // Later work is not poisoned: a fresh command gets a fresh, unset
    // signal and completes — bare `.session` succeeds without a broker and
    // opens the picker on completion.
    tui.enqueue_history_command(".session".to_string());
    assert!(
        tui.history_command_abort.is_some(),
        "fresh command gets a fresh abort slot"
    );
    tui.flush_history_work_for_test().await;
    assert!(
        matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })),
        "post-Ctrl-C history command must run to completion"
    );
    assert!(tui.history_command_abort.is_none());
    assert!(!tui.history_command_running);
}

/// Enter's shared-signal reset must not un-abort an in-flight history
/// command. This is the original T2 bug Argus flagged: the history command
/// ran on a clone of the shared prompt signal, and the next Enter's
/// `abort_signal.reset()` revived the cancelled command. Commands now run on
/// their own signal, asserted three ways: signal identity (not the shared
/// Arc), flag persistence (the command's abort survives the shared reset),
/// and slot lifecycle (its completion still clears the slot and the next
/// command starts fresh). Broker-waiting commands observe the same signal
/// inside `run_command` because it is passed into the spawned run.
#[tokio::test]
async fn enter_shared_abort_reset_does_not_unabort_inflight_history_command() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.enqueue_history_command(".session alpha".to_string());
    let command_abort = tui
        .history_command_abort
        .clone()
        .expect("running command owns its abort slot");
    assert!(
        !std::sync::Arc::ptr_eq(&command_abort, &tui.abort_signal),
        "history commands must not run on the shared prompt signal"
    );

    // Ctrl-C fires the command's own signal; what handle_enter_key does
    // before the next submission only resets the shared prompt signal. The
    // in-flight command must stay aborted — under the old shared signal this
    // reset cleared the command's abort too.
    command_abort.set_ctrlc();
    tui.abort_signal.reset();
    assert!(
        command_abort.aborted_ctrlc(),
        "the command's abort must survive the shared-signal reset"
    );
    assert!(
        !tui.abort_signal.aborted_ctrlc(),
        "the shared signal itself is reset, as Enter does"
    );

    // The command's completion still lands (here as an error: `.session
    // alpha` is not a valid explicit target), clears the slot, and does not
    // poison the queue for later work.
    tui.flush_history_work_for_test().await;
    assert!(tui.history_command_abort.is_none());
    assert!(!tui.history_command_running);

    tui.enqueue_history_command(".session beta".to_string());
    let fresh = tui
        .history_command_abort
        .as_ref()
        .expect("fresh command gets a fresh abort slot");
    assert!(
        !fresh.aborted_ctrlc(),
        "the next command must start fresh, not pre-aborted"
    );
    assert!(
        !std::sync::Arc::ptr_eq(fresh, &command_abort),
        "the next command must not reuse the cancelled signal"
    );
    tui.flush_history_work_for_test().await;
    assert!(tui.history_command_abort.is_none());
}

/// The queue serializes history commands in submission order: while one
/// runs, the next is queued; finishing the first immediately starts the
/// second with its own fresh abort signal; finishing the last drains the
/// queue and clears the slot.
#[tokio::test]
async fn history_command_queue_serializes_submission_order() {
    use harnx_runtime::commands::CommandOutcome;

    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.enqueue_history_command(".session alpha".to_string());
    tui.enqueue_history_command(".session beta".to_string());
    assert!(tui.history_command_running);
    assert_eq!(tui.history_command_queue.len(), 1);

    let first_abort = tui
        .history_command_abort
        .clone()
        .expect("running command owns its abort slot");

    // First completion: the queue immediately starts the second command.
    tui.finish_history_command(
        ".session alpha".to_string(),
        (None, None),
        String::new(),
        Ok(CommandOutcome::Continue),
    )
    .await;
    assert!(
        tui.history_command_running,
        "finishing the first command must start the queued one"
    );
    assert!(tui.history_command_queue.is_empty());
    assert!(
        tui.history_command_abort.is_some(),
        "the second command gets its own abort slot"
    );
    let second_abort = tui.history_command_abort.clone().unwrap();
    assert!(
        !std::sync::Arc::ptr_eq(&first_abort, &second_abort),
        "each command must carry its own signal, not a shared one"
    );

    // Last completion: queue drains, slot clears.
    tui.finish_history_command(
        ".session beta".to_string(),
        (None, None),
        String::new(),
        Ok(CommandOutcome::Continue),
    )
    .await;
    assert!(!tui.history_command_running);
    assert!(tui.history_command_queue.is_empty());
    assert!(tui.history_command_abort.is_none());
}

/// Ctrl-C cancels pending picker-refresh, info-overlay and new-session
/// reservation jobs: their slots are taken and fired, and no completion
/// events arrive.
#[tokio::test]
async fn ctrl_c_cancels_pending_picker_info_and_reservation_jobs() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.spawn_picker_sessions_refresh();
    assert!(tui.picker_refresh_abort.is_some());
    tui.spawn_info_overlay_load(
        vec![".dump session".to_string(), "x".to_string()],
        crate::input_commands::InfoOverlayType::SessionDump,
    );
    assert!(tui.info_overlay_abort.is_some());
    tui.spawn_new_session_reservation(crate::lifecycle::NewSessionSelection {
        sessions: vec![],
        selected: 0,
        origin_agent: None,
        origin_session: None,
    });
    assert!(tui.new_session_reservation_abort.is_some());

    tui.handle_ctrl_c().await;
    assert!(tui.picker_refresh_abort.is_none());
    assert!(tui.info_overlay_abort.is_none());
    assert!(tui.new_session_reservation_abort.is_none());

    // Flush returns promptly: cancelled jobs send no completion events — a
    // cancelled reservation must not reopen the picker with a cancellation
    // error either (retry-2 Argus finding).
    tui.flush_history_work_for_test().await;
    assert!(
        !tui.app.detail_view_open,
        "cancelled info load must not open the overlay"
    );
    assert!(
        tui.app.modal.is_none(),
        "cancelled reservation must not reopen the picker"
    );
}

/// A finished picker refresh only applies while a session picker is open,
/// and the highlighted session keeps its highlight in the new list.
#[tokio::test]
async fn picker_sessions_completion_updates_open_picker_and_tracks_selection() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    // No picker open: the completion is discarded, and its abort slot still
    // clears (the event dispatch owns the clearing).
    tui.picker_refresh_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(picker_sessions_loaded_event(
        &tui,
        vec![picker_meta("new-a")],
    ))
    .await
    .unwrap();
    assert!(tui.app.modal.is_none());
    assert!(tui.picker_refresh_abort.is_none());

    // Open picker with session-b highlighted (index 2 = second session row).
    tui.app.modal = Some(ModalState::SessionPicker {
        sessions: vec![picker_meta("old-a"), picker_meta("old-b")],
        selected: 2,
        origin_agent: None,
        origin_session: None,
        error: None,
    });
    tui.picker_refresh_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(crate::types::TuiEvent::PickerSessionsLoaded {
        seq: tui.picker_refresh_seq,
        picker_incarnation: tui.picker_incarnation,
        target: tui.current_picker_target(),
        sessions: vec![picker_meta("new-c"), picker_meta("old-b")],
        fetch_error: Some("fetch error".to_string()),
    })
    .await
    .unwrap();
    let Some(ModalState::SessionPicker {
        sessions,
        selected,
        error,
        ..
    }) = &tui.app.modal
    else {
        panic!("expected session picker after refresh");
    };
    assert_eq!(
        sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["new-c", "old-b"]
    );
    assert_eq!(
        *selected, 2,
        "highlighted session keeps its position in the new list"
    );
    assert_eq!(error.as_deref(), Some("fetch error"));
}

/// A finished "New session" reservation switches the session and reconciles
/// through the background rebuild; a failed one restores the picker it came
/// from with the error visible.
#[tokio::test]
async fn new_session_reservation_events_switch_or_restore() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", None);
    let mut tui = Tui::init(&config).await.unwrap();

    // Success path: the completion event switches the session and clears
    // the reservation's abort slot (the event dispatch owns the clearing).
    tui.new_session_reservation_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(reserved_event(
        tui.new_session_reservation_seq,
        "fresh-session",
        Some("origin-agent"),
    ))
    .await
    .unwrap();
    assert!(tui.new_session_reservation_abort.is_none());
    assert_eq!(
        session_id(&tui).as_deref(),
        Some("fresh-session"),
        "reserved session becomes active"
    );
    tui.flush_history_work_for_test().await;

    // Failure path: the picker the reservation started from is restored
    // with the error.
    tui.new_session_reservation_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(failed_reservation_event(
        tui.new_session_reservation_seq,
        "reservation boom",
        ReservationFailureRestore {
            sessions: vec![picker_meta("kept-a")],
            selected: 1,
            origin_agent: Some("origin-agent"),
            origin_session: Some("origin-session"),
        },
    ))
    .await
    .unwrap();
    assert!(tui.new_session_reservation_abort.is_none());
    let Some(ModalState::SessionPicker {
        sessions,
        selected,
        origin_agent,
        origin_session,
        error,
    }) = &tui.app.modal
    else {
        panic!("failed reservation must restore the session picker");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(*selected, 1);
    assert_eq!(origin_agent.as_deref(), Some("origin-agent"));
    assert_eq!(origin_session.as_deref(), Some("origin-session"));
    assert!(error.as_deref().unwrap().contains("reservation boom"));

    // Stale (superseded or cancelled) completions are fenced by seq: the
    // slot survives, the session is not switched, and the restored picker
    // is left alone.
    tui.new_session_reservation_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(reserved_event(
        tui.new_session_reservation_seq + 1,
        "stale-session",
        None,
    ))
    .await
    .unwrap();
    assert!(
        tui.new_session_reservation_abort.is_some(),
        "stale success must not clear the newer slot"
    );
    assert_eq!(
        session_id(&tui).as_deref(),
        Some("fresh-session"),
        "stale success must not switch the session"
    );
    tui.handle_tui_event(failed_reservation_event(
        tui.new_session_reservation_seq + 1,
        "stale boom",
        stale_restore(vec![picker_meta("stale-a")]),
    ))
    .await
    .unwrap();
    assert!(
        tui.new_session_reservation_abort.is_some(),
        "stale failure must not clear the newer slot"
    );
    let Some(ModalState::SessionPicker {
        sessions, error, ..
    }) = &tui.app.modal
    else {
        panic!("stale failure must not clobber the restored picker");
    };
    assert_eq!(
        sessions.len(),
        1,
        "stale failure must not replace the picker list"
    );
    assert!(error.as_deref().unwrap().contains("reservation boom"));
}

/// Argus retry-1 finding, real reachable flow: Enter on the picker's
/// "New session" row starts reservation A and closes the picker; bare
/// `.session` (the history command queue) reopens the picker while A is
/// pending, and a second Enter starts reservation B. The two reservations
/// own independent abort signals and carry seq fences: A's signal fires on
/// supersede, A's stale success and failure completions are discarded, and
/// Ctrl-C on B emits nothing — no reopen, no session switch.
#[tokio::test]
async fn new_session_reservation_supersession_via_real_picker_actions_is_fenced() {
    let config = non_broker_test_config_with_mock_client_and_agent("test-agent", None);
    let mut tui = Tui::init(&config).await.unwrap();

    // Real UI action: picker row 0 is "New session"; Enter starts
    // reservation A and closes the picker.
    tui.app.modal = Some(session_picker_modal(
        vec![picker_meta("a-1"), picker_meta("a-2")],
        0,
    ));
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        tui.app.modal.is_none(),
        "Enter closes the picker while the reservation runs"
    );
    let reservation_a = tui
        .new_session_reservation_abort
        .clone()
        .expect("reservation A owns the abort slot");
    assert_eq!(tui.new_session_reservation_seq, 1);

    // A fails fast on the unconfigured non-local test cluster (no broker),
    // and its completion event stays unhandled — from the UI's point of view
    // the reservation is still pending when the user reopens the picker, which
    // is exactly the state the seq fence must survive.
    let mut stale_completions = vec![recv_reservation_completion(&mut tui).await];

    // Real UI action: bare `.session` reopens the picker, and the reopened
    // picker's Enter starts reservation B, superseding A with an
    // independent signal.
    reopen_picker_with_bare_session(&mut tui, &mut stale_completions).await;
    assert!(
        matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })),
        "bare .session reopens the picker while the reservation is pending"
    );
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.new_session_reservation_seq, 2);
    let reservation_b = tui
        .new_session_reservation_abort
        .clone()
        .expect("reservation B owns a fresh abort slot");
    assert!(
        !std::sync::Arc::ptr_eq(&reservation_a, &reservation_b),
        "A and B own independent signals"
    );

    // Stale success and failure from A, and A's real queued completion,
    // must not clear B's slot, switch the session, or restore the picker.
    assert_stale_completions_discarded(&mut tui, &reservation_b, stale_completions).await;

    // Ctrl-C cancels B without emitting a completion, and the fence bump
    // discards anything B queued before the cancel: no reopen, no switch.
    tui.handle_ctrl_c().await;
    tui.flush_history_work_for_test().await;
    assert!(tui.new_session_reservation_abort.is_none());
    assert!(
        tui.app.modal.is_none(),
        "cancelled reservation must not reopen the picker"
    );
    assert_eq!(
        session_id(&tui),
        None,
        "cancelled reservation must not switch the session"
    );
}

/// Receive events, handling the non-reservation ones (they cannot touch the
/// closed picker), until a background reservation completion arrives.
async fn recv_reservation_completion(tui: &mut Tui) -> TuiEvent {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), tui.event_rx.recv())
            .await
            .expect("reservation completion within budget")
            .expect("event channel stays open");
        if matches!(
            event,
            TuiEvent::NewSessionReserved { .. } | TuiEvent::NewSessionReservationFailed { .. }
        ) {
            return event;
        }
        tui.handle_tui_event(event).await.unwrap();
    }
}

/// Bare `.session` through the history command queue reopens the picker;
/// reservation completions that surface while it runs are stashed unhandled.
async fn reopen_picker_with_bare_session(tui: &mut Tui, stash: &mut Vec<TuiEvent>) {
    tui.enqueue_history_command(".session".to_string());
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), tui.event_rx.recv())
            .await
            .expect("picker reopen event within budget")
            .expect("event channel stays open");
        if matches!(
            event,
            TuiEvent::NewSessionReserved { .. } | TuiEvent::NewSessionReservationFailed { .. }
        ) {
            stash.push(event);
            continue;
        }
        tui.handle_tui_event(event).await.unwrap();
        if matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })) {
            return;
        }
    }
}

/// Every stale reservation completion (A's real queued one plus injected
/// stale success and failure) is discarded: B's slot survives, the session
/// is not switched, and the closed picker stays closed.
async fn assert_stale_completions_discarded(
    tui: &mut Tui,
    reservation_b: &harnx_runtime::utils::AbortSignal,
    mut stale: Vec<TuiEvent>,
) {
    stale.push(reserved_event(1, "stale-session", None));
    stale.push(failed_reservation_event(
        1,
        "stale failure",
        stale_restore(vec![picker_meta("stale-a")]),
    ));
    for event in stale {
        tui.handle_tui_event(event).await.unwrap();
        assert!(
            std::sync::Arc::ptr_eq(
                tui.new_session_reservation_abort.as_ref().unwrap(),
                reservation_b
            ),
            "stale completion must not clear B's slot"
        );
        assert_eq!(
            session_id(tui),
            None,
            "stale completion must not switch the session"
        );
        assert!(
            tui.app.modal.is_none(),
            "stale completion must not reopen the picker"
        );
    }
}

/// Argus retry-1 finding: `u`/`U` in the session picker used to await the
/// metadata-store write inline, freezing the input loop while the broker
/// recovered. The toggle now leaves the input loop immediately as
/// cancellable background work: input keys keep flowing while it is
/// pending, a newer toggle supersedes it with an independent signal and a
/// seq fence discards stale completions, and the current completion
/// re-lists only the still-open picker.
#[tokio::test]
async fn picker_unread_toggle_leaves_input_loop_and_fences_stale_completions() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    // Real UI action: `u` on a session row starts the toggle in the
    // background and returns to the input loop immediately.
    tui.app.modal = Some(session_picker_modal(
        vec![picker_meta("u-1"), picker_meta("u-2")],
        2,
    ));
    tui.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        tui.picker_unread_toggle_abort.is_some(),
        "toggle runs as pending background work, not inline"
    );

    // Input keeps flowing while the toggle is pending: navigation is
    // processed on the same (unblocked) input loop.
    tui.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await
        .unwrap();
    let Some(ModalState::SessionPicker { selected, .. }) = &tui.app.modal else {
        panic!("picker stays open while the toggle is pending");
    };
    assert_eq!(*selected, 1, "input keys flow while the toggle is pending");

    // A second `u` supersedes the first toggle with an independent signal.
    let first_abort = tui.picker_unread_toggle_abort.clone().unwrap();
    tui.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(tui.picker_unread_toggle_seq, 2);
    assert!(
        first_abort.aborted_ctrlc(),
        "supersede fires the first toggle's signal"
    );
    let second_abort = tui.picker_unread_toggle_abort.clone().unwrap();
    assert!(
        !std::sync::Arc::ptr_eq(&first_abort, &second_abort),
        "toggles own independent signals"
    );

    // Stale completion (first toggle's seq) is discarded: the current slot
    // survives and the open picker is not touched.
    tui.handle_tui_event(crate::types::TuiEvent::PickerUnreadToggleFinished { seq: 1 })
        .await
        .unwrap();
    assert!(
        tui.picker_unread_toggle_abort.is_some(),
        "stale toggle completion must not clear the newer slot"
    );
    assert!(
        matches!(tui.app.modal, Some(ModalState::SessionPicker { .. })),
        "stale toggle completion must not clobber the modal"
    );

    // Current completion clears the slot and re-lists the still-open
    // picker as (fenced, cancellable) background work.
    tui.handle_tui_event(crate::types::TuiEvent::PickerUnreadToggleFinished { seq: 2 })
        .await
        .unwrap();
    assert!(tui.picker_unread_toggle_abort.is_none());
    assert!(
        tui.picker_refresh_abort.is_some(),
        "current completion re-lists the still-open picker"
    );

    // The re-list applies to the still-open picker only: it stays a
    // session picker (with the no-broker fetch error), never clobbered.
    tui.flush_history_work_for_test().await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::SessionPicker { .. })
    ));
}

/// Ctrl-C cancels a pending picker unread toggle without a completion
/// event: the slot clears, no refresh is spawned, and the open picker is
/// left exactly as it was.
#[tokio::test]
async fn ctrl_c_cancels_pending_picker_unread_toggle_without_refresh() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.app.modal = Some(session_picker_modal(
        vec![picker_meta("u-1"), picker_meta("u-2")],
        2,
    ));
    tui.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.picker_unread_toggle_abort.is_some());

    tui.handle_ctrl_c().await;
    assert!(tui.picker_unread_toggle_abort.is_none());

    // No completion event arrives, so nothing re-lists or clobbers the
    // picker; the flush proves the cancelled job terminates promptly.
    tui.flush_history_work_for_test().await;
    assert!(
        tui.picker_refresh_abort.is_none(),
        "cancelled toggle must not spawn a picker re-list"
    );
    let Some(ModalState::SessionPicker {
        sessions, selected, ..
    }) = &tui.app.modal
    else {
        panic!("cancelled toggle leaves the picker as it was");
    };
    assert_eq!(sessions.len(), 2);
    assert_eq!(*selected, 2);
}

/// The toggle's completion re-lists only the picker that is still the
/// current view: when the user has moved on (Esc back to the agent picker),
/// a finished toggle must not spawn a session-list refresh.
#[tokio::test]
async fn picker_unread_toggle_completion_refreshes_only_current_picker_view() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.app.modal = Some(session_picker_modal(vec![picker_meta("u-1")], 1));
    tui.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.picker_unread_toggle_abort.is_some());

    // Real UI action: Esc on an agent-flow picker goes back to the agent
    // picker; the session picker is no longer the current view.
    tui.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        matches!(tui.app.modal, Some(ModalState::AgentPicker { .. })),
        "Esc returns to the agent picker"
    );

    // Current-seq completion still clears the slot but does not spawn a
    // refresh for the moved-on view.
    tui.handle_tui_event(crate::types::TuiEvent::PickerUnreadToggleFinished {
        seq: tui.picker_unread_toggle_seq,
    })
    .await
    .unwrap();
    assert!(tui.picker_unread_toggle_abort.is_none());
    assert!(
        tui.picker_refresh_abort.is_none(),
        "finished toggle must not re-list a view that moved on"
    );

    tui.flush_history_work_for_test().await;
    assert!(matches!(
        tui.app.modal,
        Some(ModalState::AgentPicker { .. })
    ));
}

fn spawn_writer_contention(
    config: harnx_runtime::config::GlobalConfig,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let session_a = harnx_runtime::config::session::new(
        &Config {
            remote_agent: Some(("agent-a".to_string(), "local".to_string())),
            ..Config::default()
        },
        "sess-a",
        None,
    )
    .unwrap();
    let session_b = harnx_runtime::config::session::new(
        &Config {
            remote_agent: Some(("agent-b".to_string(), "local".to_string())),
            ..Config::default()
        },
        "sess-b",
        None,
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut toggle = false;
        while !stop.load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < deadline
        {
            let mut guard = config.write();
            if toggle {
                guard.remote_agent = Some(("agent-a".to_string(), "local".to_string()));
                guard.session = Some(session_a.clone());
            } else {
                guard.remote_agent = Some(("agent-b".to_string(), "local".to_string()));
                guard.session = Some(session_b.clone());
            }
            drop(guard);
            toggle = !toggle;
            std::thread::yield_now();
        }
    })
}

/// C4-F2 regression: `current_history_target_and_storage_key` and
/// `run_config_command_inline` must take one brief read guard (never nested
/// `config.read()`), so writer contention on `YieldingRwLock` cannot deadlock
/// and every captured `(HistoryTarget, storage_key)` pair stays coherent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_command_target_snapshot_uses_single_read_guard_under_writer_contention() {
    let config = non_broker_test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = spawn_writer_contention(config.clone(), stop.clone());

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for i in 0..200 {
            let (target, storage_key) = tui.current_history_target_and_storage_key();
            if let (Some(agent), Some(sid)) = (
                target
                    .agent
                    .as_deref()
                    .map(|a| a.split('@').next().unwrap_or(a)),
                target.session_id.as_deref(),
            ) {
                let expected = harnx_core::session_identity::session_key(Some(agent), sid);
                assert_eq!(storage_key.as_deref(), Some(expected.as_str()));
            }
            if i < 50 {
                tui.run_config_command_inline(".set dry_run true")
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .expect("inline snapshot/command must not deadlock under writer contention");

    stop.store(true, std::sync::atomic::Ordering::Release);
    writer.await.unwrap();
}
