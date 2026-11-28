//! Keyboard and rendered-screen coverage using the shared durable transcript.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use harnx_runtime::nats_session_log::NatsSessionLog;
use ratatui::style::Modifier;

use super::d2_fixture::{display_rows, fixture, replay};
use super::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::compaction_history::ArchiveView;
use crate::types::{TranscriptItem, Tui};

pub(super) async fn fixture_config(entries: &[(u64, SessionLogEntry)]) -> GlobalConfig {
    assert!(
        ENV_LOCK.try_lock().is_err(),
        "d4_fixture::fixture_config mutates local NATS state and requires holding ENV_LOCK + TestEnvironment"
    );
    assert!(
        std::env::var_os("HARNX_DATA_DIR").is_some(),
        "d4_fixture::fixture_config requires TestEnvironment isolation"
    );
    let config = super::test_config_with_mock_client_and_agent("archive", None);
    config.write().session = Some(replay(entries));
    let server = harnx_runtime::config::resolve_local_nats_server_config()
        .await
        .unwrap();
    config.write().nats_servers.push(server);
    let log = session_log(&config).await;
    for (_, entry) in entries {
        log.append_event_async(entry).await.unwrap();
    }
    config
}

async fn session_log(config: &GlobalConfig) -> NatsSessionLog {
    let cfg = config.read().clone();
    let jetstream = cfg.nats_jetstream(LOCAL_CLUSTER_KEY).await.unwrap();
    NatsSessionLog::for_agent(jetstream, "archive", cfg.session.as_ref().unwrap().id())
        .with_replicas(1)
}

pub(super) async fn press(tui: &mut Tui, code: KeyCode) {
    tui.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
        .await
        .unwrap();
}

pub(super) async fn select_marker(tui: &mut Tui) {
    let marker = tui
        .app
        .transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::CompactionMarker { .. }))
        .unwrap();
    for _ in 0..tui.app.transcript.len() {
        press(tui, KeyCode::Up).await;
        if tui.app.transcript_focus == Some(marker) {
            return;
        }
    }
    panic!("UP never selected the compaction marker");
}

pub(super) fn archive(tui: &Tui) -> &ArchiveView {
    tui.app
        .archive_browser
        .as_ref()
        .expect("archive browser")
        .views
        .last()
        .unwrap()
}

#[tokio::test]
async fn d4_up_highlights_the_entire_wrapped_marker() {
    let config = super::test_config();
    config.write().session = Some(replay(&fixture()));
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    harness.render();
    let selected = highlighted_text(&harness);
    let summary = harnx_core::transcript_view::default_view(&fixture())
        .unwrap()
        .compaction
        .unwrap()
        .summary;
    assert!(selected.contains("session compacted"));
    assert!(selected.contains(&summary));
    assert!(!selected.contains("Implement the shared transcript view"));
}

fn highlighted_text(harness: &TuiTestHarness) -> String {
    let buffer = harness.terminal.backend().buffer();
    let lines = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .filter_map(|x| {
                    let cell = &buffer[(x, y)];
                    cell.modifier
                        .contains(Modifier::REVERSED)
                        .then(|| cell.symbol())
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    lines.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_fixture_nested_enter_and_escape_restore_each_transcript_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    harness.render();
    let initial = harness.screen_contents();
    press(harness.tui(), KeyCode::Enter).await;
    assert_first_archive(&mut harness);
    press(harness.tui(), KeyCode::Enter).await;
    assert_oldest_archive(&mut harness);
    press(harness.tui(), KeyCode::Esc).await;
    assert_first_archive(&mut harness);
    press(harness.tui(), KeyCode::Esc).await;
    harness.render();
    assert!(harness.tui().app.archive_browser.is_none());
    assert_eq!(harness.screen_contents(), initial);
}

fn assert_first_archive(harness: &mut TuiTestHarness) {
    let view = archive(harness.tui());
    assert_eq!(
        (
            view.before,
            view.marker.as_ref().unwrap().compaction_seq,
            view.focus
        ),
        (15, 5, Some(0))
    );
    insta::assert_snapshot!("d4_before_15", display_rows(&view.items).join("\n"));
    harness.render();
    assert!(harness.screen_contents().contains("read-only"));
}

fn assert_oldest_archive(harness: &mut TuiTestHarness) {
    let view = archive(harness.tui());
    assert_eq!((view.before, view.marker.as_ref()), (5, None));
    assert_eq!(
        view.items
            .iter()
            .filter_map(TranscriptItem::seq)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    insta::assert_snapshot!("d4_before_5", display_rows(&view.items).join("\n"));
    assert_screen_contains(harness, "Inspect the session log format.");
}

fn assert_screen_contains(harness: &mut TuiTestHarness, text: &str) {
    harness.render();
    assert!(harness.screen_contents().contains(text));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_archive_keys_and_paste_cannot_mutate_session_or_input_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let log = session_log(&config).await;
    let original = log.load_events_async().await.unwrap();
    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    select_marker(harness.tui()).await;
    let root_rows = format!("{:?}", harness.tui().app.transcript);
    press(harness.tui(), KeyCode::Enter).await;
    assert_ignored_keys(harness.tui()).await;
    assert_eq!(log.load_events_async().await.unwrap(), original);
    assert_eq!(format!("{:?}", harness.tui().app.transcript), root_rows);
    assert_eq!(
        serde_json::to_value(&config.read().session.as_ref().unwrap().messages).unwrap(),
        serde_json::to_value(&replay(&fixture()).messages).unwrap()
    );
}

async fn assert_ignored_keys(tui: &mut Tui) {
    press(tui, KeyCode::Down).await;
    let before = display_rows(&archive(tui).items);
    for code in [
        KeyCode::Char('e'),
        KeyCode::Char('i'),
        KeyCode::Char('d'),
        KeyCode::Delete,
        KeyCode::Char('r'),
        KeyCode::Char('x'),
        KeyCode::Enter,
    ] {
        press(tui, code).await;
    }
    tui.handle_paste("unexpected input".into()).await;
    assert_eq!(display_rows(&archive(tui).items), before);
    assert_eq!(tui.app.input.lines(), [""]);
    assert!(tui.app.modal.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_empty_archive_opens_and_escape_returns_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let entries = vec![(1, fixture()[4].1.clone())];
    let config = fixture_config(&entries).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert!(archive(harness.tui()).items.is_empty());
    press(harness.tui(), KeyCode::Enter).await;
    press(harness.tui(), KeyCode::Esc).await;
    assert!(harness.tui().app.archive_browser.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_archive_scroll_and_focus_survive_nested_navigation_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    harness.terminal.backend_mut().resize(40, 12);
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    harness.render();
    press(harness.tui(), KeyCode::End).await;
    harness.render();
    assert!(archive(harness.tui()).scroll.position > 0);
    assert_archive_mouse_scroll(harness.tui());
    press(harness.tui(), KeyCode::Up).await;
    harness.render();
    assert_eq!(archive(harness.tui()).scroll.position, 0);
    let expected = highlighted_text(&harness);
    press(harness.tui(), KeyCode::Enter).await;
    press(harness.tui(), KeyCode::Up).await;
    press(harness.tui(), KeyCode::Esc).await;
    harness.render();
    assert_eq!(highlighted_text(&harness), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_open_archive_is_isolated_from_live_transcript_changes_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    let prior = display_rows(&archive(harness.tui()).items);
    let live = "Live reply while browsing archives";
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        text: live.into(),
        seq: None,
        timestamp: None,
    });
    config.write().agent = None;
    press(harness.tui(), KeyCode::Enter).await;
    assert_oldest_archive(&mut harness);
    press(harness.tui(), KeyCode::Esc).await;
    assert_eq!(display_rows(&archive(harness.tui()).items), prior);
    press(harness.tui(), KeyCode::Esc).await;
    assert!(
        matches!(harness.tui().app.transcript.last(), Some(TranscriptItem::UserText { text, .. }) if text == live)
    );
}

#[tokio::test(start_paused = true)]
async fn d4_archive_timeout_shows_error_and_keyboard_still_responds() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = super::test_config();
    config.write().session = Some(replay(&fixture()));
    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    select_marker(harness.tui()).await;

    // Keep the socket open without sending NATS INFO, so connecting cannot finish.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    {
        let mut cfg = config.write();
        cfg.remote_agent = Some(("archive".into(), "stalled".into()));
        cfg.nats_servers.push(
            serde_json::from_value(serde_json::json!({
                "name": "stalled",
                "url": format!("nats://{}", listener.local_addr().unwrap()),
            }))
            .unwrap(),
        );
    }

    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        press(harness.tui(), KeyCode::Enter),
    )
    .await
    .expect("Enter must return control to the event loop after the archive deadline");
    assert!(harness.tui().app.archive_browser.is_none());
    assert!(harness.tui().app.detail_view_open);
    harness.render();
    let screen = harness.screen_contents();
    assert!(screen.contains("Earlier history unavailable"));
    assert!(screen.contains("earlier history read timed out after 10s"));

    press(harness.tui(), KeyCode::Esc).await;
    assert!(!harness.tui().app.detail_view_open);
    press(harness.tui(), KeyCode::Esc).await;
    press(harness.tui(), KeyCode::Char('x')).await;
    assert_eq!(harness.tui().app.input.lines(), ["x"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_stale_marker_reports_reload_instead_of_showing_wrong_archive_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    select_marker(harness.tui()).await;
    let log = session_log(&config).await;
    log.append_event_async(&fixture()[4].1).await.unwrap();
    press(harness.tui(), KeyCode::Enter).await;
    assert!(harness.tui().app.archive_browser.is_none());
    assert!(harness
        .tui()
        .app
        .detail_view_text
        .as_ref()
        .unwrap()
        .contains("session history changed"));
    press(harness.tui(), KeyCode::Esc).await;
    assert!(!harness.tui().app.detail_view_open);
}

fn assert_archive_mouse_scroll(tui: &mut Tui) {
    let archive_position = archive(tui).scroll.position;
    let root_position = tui.app.scroll_state.position;
    tui.handle_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        archive(tui).scroll.position,
        archive_position.saturating_sub(3)
    );
    assert_eq!(tui.app.scroll_state.position, root_position);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_nested_archive_error_returns_to_parent_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    archive_mut(harness.tui())
        .marker
        .as_mut()
        .unwrap()
        .compaction_seq = u64::MAX;
    press(harness.tui(), KeyCode::Enter).await;
    assert_screen_contains(&mut harness, "Failed to load earlier history");
    assert!(harness.tui().app.detail_view_open);
    press(harness.tui(), KeyCode::Esc).await;
    assert!(!harness.tui().app.detail_view_open);
    assert_first_archive_error_parent(harness.tui());
    press(harness.tui(), KeyCode::Esc).await;
    assert!(harness.tui().app.archive_browser.is_none());
}

fn assert_first_archive_error_parent(tui: &Tui) {
    let browser = tui.app.archive_browser.as_ref().unwrap();
    assert_eq!(browser.views.len(), 1);
    assert_eq!((archive(tui).before, archive(tui).focus), (15, Some(0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_archive_enter_checks_item_type_and_footer_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert_screen_contains(&mut harness, "ENTER/earlier history");
    archive_mut(harness.tui()).items.swap(0, 1);
    assert_nonmarker_enter_is_inert(&mut harness).await;
    archive_mut(harness.tui()).focus = Some(1);
    assert_screen_contains(&mut harness, "ENTER/earlier history");
    press(harness.tui(), KeyCode::Enter).await;
    assert_oldest_archive(&mut harness);
    assert_no_earlier_hint(&mut harness);
}

async fn assert_nonmarker_enter_is_inert(harness: &mut TuiTestHarness) {
    press(harness.tui(), KeyCode::Enter).await;
    assert_eq!(
        harness
            .tui()
            .app
            .archive_browser
            .as_ref()
            .unwrap()
            .views
            .len(),
        1
    );
    assert_eq!(archive(harness.tui()).before, 15);
    assert_no_earlier_hint(harness);
}

fn assert_no_earlier_hint(harness: &mut TuiTestHarness) {
    harness.render();
    let screen = harness.screen_contents();
    assert!(!screen.contains("ENTER/earlier history"));
    assert!(screen.contains("ESC/back"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_input_draft_survives_nested_browsing_nats() {
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    let mut harness = TuiTestHarness::with_config(config).await;
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Esc).await;
    type_draft(harness.tui()).await;
    harness.render();
    let original = harness.screen_contents();
    select_marker_with_draft(harness.tui());
    press(harness.tui(), KeyCode::Enter).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert_oldest_archive(&mut harness);
    press(harness.tui(), KeyCode::Esc).await;
    press(harness.tui(), KeyCode::Esc).await;
    press(harness.tui(), KeyCode::Esc).await;
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        "draft before browsing"
    );
    harness.render();
    assert_eq!(harness.screen_contents(), original);
}

async fn type_draft(tui: &mut Tui) {
    for ch in "draft before browsing".chars() {
        press(tui, KeyCode::Char(ch)).await;
    }
}

fn select_marker_with_draft(tui: &mut Tui) {
    // Up edits a nonempty draft, so install transcript focus without changing that binding.
    tui.app.transcript_focus = tui
        .app
        .transcript
        .iter()
        .position(|item| matches!(item, TranscriptItem::CompactionMarker { .. }));
    tui.app.transcript_browsing = true;
    tui.app.scroll_state.follow = false;
    tui.app.scroll_to_focused_item = true;
}

fn archive_mut(tui: &mut Tui) -> &mut ArchiveView {
    tui.app
        .archive_browser
        .as_mut()
        .unwrap()
        .views
        .last_mut()
        .unwrap()
}

fn install_gated_loader(
    tui: &mut Tui,
    rows: Vec<TranscriptItem>,
) -> (
    std::sync::Arc<tokio::sync::Notify>,
    tokio::sync::watch::Sender<bool>,
) {
    let load_started = std::sync::Arc::new(tokio::sync::Notify::new());
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let started_tx = load_started.clone();
    tui.history_load_override = Some(std::sync::Arc::new(move |_cfg, _mode, abort| {
        let started_tx = started_tx.clone();
        let mut release_rx = release_rx.clone();
        let rows = rows.clone();
        Box::pin(async move {
            started_tx.notify_one();
            tokio::select! {
                _ = harnx_core::abort::wait_abort_signal(&abort) => {
                    anyhow::bail!("history load aborted")
                }
                _ = release_rx.wait_for(|released| *released) => Ok(rows),
            }
        })
    }));
    (load_started, release_tx)
}

async fn switch_via_picker_and_cancel(
    tui: &mut Tui,
    load_started: &tokio::sync::Notify,
    target_session: &str,
    origin_session: &str,
) {
    tui.app.modal = Some(crate::types::ModalState::SessionPicker {
        sessions: vec![harnx_runtime::config::SessionMeta {
            id: target_session.to_string(),
            session_id: Some(target_session.to_string()),
            agent_name: Some("archive".to_string()),
            title: None,
            user_id: None,
            modified: None,
            contexts: vec![],
            unread: false,
        }],
        selected: 1,
        origin_agent: Some("archive".to_string()),
        origin_session: Some(origin_session.to_string()),
        error: None,
    });
    press(tui, KeyCode::Enter).await;
    load_started.notified().await;
    tui.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    tui.flush_history_work_for_test().await;
}

fn transcript_snapshot(tui: &Tui) -> Vec<String> {
    tui.app
        .transcript
        .iter()
        .map(|item| format!("{item:?}"))
        .collect()
}

fn assert_unbound_compaction_error(tui: &Tui, expected_rows: &[String]) {
    assert!(tui.app.archive_browser.is_none());
    assert!(tui.app.detail_view_open);
    assert_eq!(
        tui.app.detail_view_title.as_deref(),
        Some("Earlier history unavailable")
    );
    assert_eq!(
        tui.app.detail_view_text.as_deref(),
        Some("Failed to load earlier history: Session changed — waiting for current transcript to load")
    );
    assert!(tui.actionable_transcript_binding().is_none());
    assert_eq!(transcript_snapshot(tui), expected_rows);
}

async fn spawn_counting_nats_listener() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    let task = tokio::spawn(async move {
        while let Ok((_stream, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    (addr, attempts, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_unbound_compaction_marker_enter_rejects_without_broker_access() {
    use std::sync::atomic::Ordering;

    let (addr, connect_attempts, accept_task) = spawn_counting_nats_listener().await;
    let config = super::non_broker_test_config_with_mock_client_and_agent("archive", None);
    {
        let mut cfg = config.write();
        let mut session_a = replay(&fixture());
        session_a.id = "session-a".to_string();
        cfg.session = Some(session_a);
        cfg.nats_servers.push(
            serde_json::from_value(serde_json::json!({
                "name": "counting",
                "url": format!("nats://{addr}"),
            }))
            .unwrap(),
        );
    }

    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    assert!(harness.tui().actionable_transcript_binding().is_some());
    let marker_rows = harness.tui().app.transcript.clone();
    let (load_started, _release_tx) = install_gated_loader(harness.tui(), marker_rows);
    switch_via_picker_and_cancel(harness.tui(), &load_started, "session-b", "session-a").await;

    config.write().remote_agent = Some(("archive".into(), "counting".into()));
    let transcript_before = transcript_snapshot(harness.tui());
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert_eq!(connect_attempts.load(Ordering::SeqCst), 0);
    assert_unbound_compaction_error(harness.tui(), &transcript_before);
    harness.render();
    assert!(harness
        .screen_contents()
        .contains("Earlier history unavailable"));
    press(harness.tui(), KeyCode::Esc).await;
    assert!(!harness.tui().app.detail_view_open);
    assert!(harness.tui().app.archive_browser.is_none());
    assert_eq!(harness.tui().app.transcript.len(), transcript_before.len());
    accept_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d4_unbound_then_fenced_bound_compaction_marker_opens_archive_nats() {
    use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
    use harnx_runtime::SessionInitializer;

    let temp = tempfile::TempDir::new().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _environment = TestEnvironment::set(temp.path());
    let config = fixture_config(&fixture()).await;
    {
        let cfg = config.read().clone();
        let js = cfg.nats_jetstream(LOCAL_CLUSTER_KEY).await.unwrap();
        let store = SessionMetadataStore::ensure(&js, 1).await.unwrap();
        store
            .create(&SessionMetadata::new(
                "session-b",
                SessionInitializer::named("archive", Default::default()),
            ))
            .await
            .unwrap();
        let b_log = NatsSessionLog::for_agent(js, "archive", "session-b").with_replicas(1);
        for (_, entry) in &fixture() {
            b_log.append_event_async(entry).await.unwrap();
        }
    }
    config.write().remote_agent = Some(("archive".into(), LOCAL_CLUSTER_KEY.into()));

    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    let origin_session = config.read().session.as_ref().unwrap().id().to_string();
    let (load_started, _release_tx) = install_gated_loader(harness.tui(), Vec::new());
    switch_via_picker_and_cancel(harness.tui(), &load_started, "session-b", &origin_session).await;

    let transcript_before = transcript_snapshot(harness.tui());
    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert_unbound_compaction_error(harness.tui(), &transcript_before);
    press(harness.tui(), KeyCode::Esc).await;

    harness.tui().history_load_override = None;
    harness.tui().schedule_history_rebuild_for_command(true);
    harness.tui().flush_history_work_for_test().await;
    assert!(harness.tui().actionable_transcript_binding().is_some());

    select_marker(harness.tui()).await;
    press(harness.tui(), KeyCode::Enter).await;
    assert!(!harness.tui().app.detail_view_open);
    assert_first_archive(&mut harness);
}
