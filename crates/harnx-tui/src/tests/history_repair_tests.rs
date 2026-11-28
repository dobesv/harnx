//! Tests for T3 read-only history repair: initial failure recovery on idle
//! attachment, paced retries without reconnects, empty-history clearing, good
//! transcript preservation, and read-only persistence-sink isolation.

use super::*;
use crate::history_work::RebuildMode;

pub(super) fn attach_target_for_test(tui: &mut Tui) -> (String, String) {
    tui.session_activity_target = tui.session_activity_destination();
    tui.session_attachment_gen = tui.session_attachment_gen.saturating_add(1);
    tui.session_stream_attached = false;
    tui.live_events = tui.live_events.fork();
    tui.session_activity_target
        .clone()
        .expect("session activity destination")
}

pub(super) fn stream_attached_event(
    tui: &Tui,
    target: &(String, String),
    active: bool,
) -> TuiEvent {
    TuiEvent::SessionStreamAttached {
        stamp: crate::event_isolation::EventStamp::live(&tui.live_events),
        session_id: target.0.clone(),
        cluster: target.1.clone(),
        active,
    }
}

pub(super) async fn init_tui_with_failed_initial_history(session_id: &str) -> (GlobalConfig, Tui) {
    let config = super::test_config_with_mock_client_and_agent("test-agent", None);
    {
        let mut guard = config.write();
        guard.remote_agent = Some(("test-agent".to_string(), "remote-test".to_string()));
        guard.session =
            Some(harnx_runtime::config::session::new(&guard, session_id, None).unwrap());
    }
    let tui = Tui::init(&config).await.unwrap();
    assert!(tui.is_history_repair_pending());
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::ErrorText(t) if t.contains("Failed to load session history"))
    }));
    (config, tui)
}

pub(super) async fn wait_for_history_rebuild_slot_idle(tui: &mut Tui) {
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    let started = std::time::Instant::now();
    loop {
        while let Ok(event) = tui.event_rx.try_recv() {
            tui.handle_tui_event(event).await.unwrap();
        }
        if tui.history_rebuild_abort.is_none() {
            return;
        }
        assert!(started.elapsed() < BUDGET, "rebuild slot timed out");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

pub(super) fn has_user_text(tui: &Tui, expected: &str) -> bool {
    tui.app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::UserText { text, .. } if text == expected))
}

pub(super) fn session_agent_event(
    tui: &Tui,
    target: &(String, String),
    event: harnx_core::event::AgentEvent,
) -> TuiEvent {
    TuiEvent::SessionAgent {
        stamp: crate::event_isolation::EventStamp::live(&tui.live_events),
        historical: false,
        session_id: target.0.clone(),
        cluster: target.1.clone(),
        event,
    }
}

#[tokio::test]
async fn initial_history_failure_repairs_on_first_idle_attach_and_paced_retry_without_second_attach(
) {
    let (_config, mut tui) = init_tui_with_failed_initial_history("repair-idle-attach").await;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_millis(15));
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(std::sync::Arc::new(move |_cfg, mode, _abort| {
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            assert!(matches!(mode, RebuildMode::RepairInitial));
            if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                anyhow::bail!("transient repair error");
            }
            Ok(vec![TranscriptItem::UserText {
                text: "repaired history row".to_string(),
                seq: Some(1),
                timestamp: None,
            }])
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    assert!(tui.history_rebuild_abort.is_some());

    wait_for_history_rebuild_slot_idle(&mut tui).await;
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(tui.is_history_repair_pending());
    assert!(tui.history_repair_retry_handle.is_some());
    assert!(tui.app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::ErrorText(t) if t.contains("Failed to load session history"))
    }));

    wait_for_repair_completion(&mut tui).await;
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(!tui.is_history_repair_pending());
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(has_user_text(&tui, "repaired history row"));
}

#[tokio::test]
async fn initial_history_repair_succeeds_with_empty_history_and_skips_redundant_reconnect_refresh()
{
    let (_config, mut tui) = init_tui_with_failed_initial_history("repair-empty").await;
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_hook = attempts.clone();
    tui.history_load_override = Some(std::sync::Arc::new(move |_cfg, _mode, _abort| {
        let attempts = attempts_hook.clone();
        Box::pin(async move {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![])
        })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    tui.flush_history_work_for_test().await;

    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!tui.is_history_repair_pending());
    assert!(!tui
        .app
        .transcript
        .iter()
        .any(|item| matches!(item, TranscriptItem::ErrorText(_))));
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::SystemText(t) if t.contains("Welcome to harnx"))
    ));

    let gen_before = tui.session_attachment_gen;
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    assert_eq!(tui.session_attachment_gen, gen_before + 1);
    assert!(tui.history_rebuild_abort.is_none());
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transient_repair_and_refresh_errors_preserve_good_history_and_pending_state() {
    let (_config, mut tui) = init_tui_with_failed_initial_history("preserve-good").await;
    tui.history_repair_retry_delay_override = Some(std::time::Duration::from_secs(60));
    tui.app.transcript = vec![TranscriptItem::UserText {
        text: "existing good row".to_string(),
        seq: Some(1),
        timestamp: None,
    }];
    tui.history_load_override = Some(std::sync::Arc::new(|_cfg, _mode, _abort| {
        Box::pin(async { anyhow::bail!("still unreachable") })
    }));

    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    wait_for_history_rebuild_slot_idle(&mut tui).await;
    assert!(tui.is_history_repair_pending());
    assert_eq!(tui.app.transcript.len(), 1);
    assert!(has_user_text(&tui, "existing good row"));

    let stamp = crate::event_isolation::EventStamp::live(&tui.live_events);
    tui.schedule_shared_session_refresh(&stamp);
    wait_for_history_rebuild_slot_idle(&mut tui).await;
    assert!(tui.is_history_repair_pending());
    assert_eq!(tui.app.transcript.len(), 1);
    assert!(has_user_text(&tui, "existing good row"));
    tui.abort_history_repair_retry();
}

#[tokio::test]
async fn readonly_remote_history_loader_preserves_persistence_sink_and_respects_abort_nats() {
    if !super::has_nats_server_binary() {
        eprintln!("skipping: nats-server binary not found");
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = super::ENV_LOCK.lock().await;
    let _environment = super::TestEnvironment::set(temp.path());
    let _nats_env = super::NatsEnvReset::clear();

    let entries = vec![(
        1,
        harnx_core::session::SessionLogEntry::Message {
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("hello readonly".to_string()),
            id: None,
            timestamp: None,
            fence_token: None,
        },
    )];
    let config = super::d4_fixture::fixture_config(&entries).await;
    let session_id = config.read().session.as_ref().unwrap().id().to_string();
    assert!(config.read().session.as_ref().unwrap().runtime.is_none());

    let loaded = crate::session_history_loader::load_remote_session_history(
        &config,
        "archive".to_string(),
        harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        session_id.clone(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(loaded.messages.len(), 1);
    assert!(config.read().session.as_ref().unwrap().runtime.is_none());

    let sentinel: std::sync::Arc<dyn std::any::Any + Send + Sync> =
        std::sync::Arc::new(0xDEAD_BEEF_u64);
    config.write().session.as_mut().unwrap().runtime = Some(sentinel.clone());

    let empty = crate::session_history_loader::load_remote_session_history(
        &config,
        "archive".to_string(),
        harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        "brand-new-empty-session".to_string(),
        None,
    )
    .await
    .unwrap();
    assert!(empty.messages.is_empty());
    let current = config
        .read()
        .session
        .as_ref()
        .unwrap()
        .runtime
        .clone()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&sentinel, &current));

    let abort = harnx_runtime::utils::create_abort_signal();
    abort.set_ctrlc();
    let aborted = crate::session_history_loader::load_remote_session_history(
        &config,
        "archive".to_string(),
        harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
        session_id,
        Some(&abort),
    )
    .await;
    assert!(aborted.is_err());
    let after_abort = config
        .read()
        .session
        .as_ref()
        .unwrap()
        .runtime
        .clone()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&sentinel, &after_abort));
}

async fn setup_unreachable_d4_fixture(
    initial_text: &str,
    cluster: &str,
) -> (
    GlobalConfig,
    harnx_runtime::config::NatsServerConfig,
    String,
) {
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;

    let entries = vec![(
        1,
        SessionLogEntry::Message {
            role: MessageRole::User,
            content: MessageContent::Text(initial_text.to_string()),
            id: None,
            timestamp: None,
            fence_token: None,
        },
    )];
    let config = super::d4_fixture::fixture_config(&entries).await;
    let session_id = config.read().session.as_ref().unwrap().id().to_string();
    let mut real_server = config
        .read()
        .nats_servers
        .first()
        .expect("d4 fixture nats server")
        .clone();
    real_server.name = cluster.to_string();
    {
        let mut guard = config.write();
        guard.remote_agent = Some(("archive".to_string(), cluster.to_string()));
        guard.nats_servers.clear();
    }
    (config, real_server, session_id)
}

fn assert_runtime_sink_ptr(
    config: &GlobalConfig,
    sentinel: &std::sync::Arc<dyn std::any::Any + Send + Sync>,
) {
    let current = config
        .read()
        .session
        .as_ref()
        .unwrap()
        .runtime
        .clone()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(sentinel, &current));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_background_repair_and_stale_completion_preserve_d4_fixture_sink_nats() {
    use harnx_core::event::{AgentEvent, TurnEvent};
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;
    use harnx_runtime::nats_session_log::NatsSessionLog;

    if !super::has_nats_server_binary() {
        eprintln!("skipping: nats-server binary not found");
        return;
    }
    let temp = tempfile::TempDir::new().unwrap();
    let _lock = super::ENV_LOCK.lock().await;
    let _environment = super::TestEnvironment::set(temp.path());
    let _nats_env = super::NatsEnvReset::clear();

    let cluster = "d4-remote".to_string();
    let (config, real_server, session_id) =
        setup_unreachable_d4_fixture("hello from d4_fixture", &cluster).await;

    let mut tui = Tui::init(&config).await.unwrap();
    assert!(tui.is_history_repair_pending());
    assert!(config.read().session.as_ref().unwrap().runtime.is_none());

    config.write().nats_servers = vec![real_server];
    let target = attach_target_for_test(&mut tui);
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    tui.flush_history_work_for_test().await;

    assert!(!tui.is_history_repair_pending());
    assert!(has_user_text(&tui, "hello from d4_fixture"));
    assert!(config.read().session.as_ref().unwrap().runtime.is_none());

    let sentinel: std::sync::Arc<dyn std::any::Any + Send + Sync> =
        std::sync::Arc::new(0xCAFE_BABE_u64);
    config.write().session.as_mut().unwrap().runtime = Some(sentinel.clone());

    let cfg = config.read().clone();
    let jetstream = cfg.nats_jetstream(&cluster).await.unwrap();
    let log = NatsSessionLog::for_agent(jetstream, "archive", &session_id).with_replicas(1);
    log.append_event_async(&SessionLogEntry::Message {
        role: MessageRole::User,
        content: MessageContent::Text("second d4_fixture row".to_string()),
        id: None,
        timestamp: None,
        fence_token: None,
    })
    .await
    .unwrap();

    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Started),
    ))
    .await
    .unwrap();
    tui.handle_tui_event(stream_attached_event(&tui, &target, false))
        .await
        .unwrap();
    tui.flush_history_work_for_test().await;
    assert!(!has_user_text(&tui, "second d4_fixture row"));
    assert_runtime_sink_ptr(&config, &sentinel);

    tui.handle_tui_event(session_agent_event(
        &tui,
        &target,
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ))
    .await
    .unwrap();
    tui.flush_history_work_for_test().await;
    assert!(has_user_text(&tui, "second d4_fixture row"));
    assert_runtime_sink_ptr(&config, &sentinel);
}
