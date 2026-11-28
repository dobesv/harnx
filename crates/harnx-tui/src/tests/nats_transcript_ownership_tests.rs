//! Real gated-NATS regression test for C01 / residual B1:
//! A -> B -> cancel B's history load -> unrelated failing command leaves A's
//! retained rows unbound so destructive key/command actions cannot mutate B's
//! durable JetStream log.

use super::nats_background_mark_read_tests::{
    test_nats_server_config, test_nats_tui_config, GatedNatsProxy,
};
use super::*;
use crate::test_utils::{TestEnvironment, ENV_LOCK};
use crate::types::{ModalState, TranscriptItem};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::{Config, ConfigLock, SessionMeta};
use harnx_runtime::nats_local_server::{ensure_shared_server, SharedNatsServer};
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::SessionInitializer;
use std::sync::Arc;

const AGENT_NAME: &str = "hermes";
const TEST_CLUSTER: &str = "c01-transcript-ownership-cluster";

fn log_message(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        timestamp: None,
        fence_token: None,
        role,
        content: MessageContent::Text(text.to_string()),
    }
}

struct NatsC01Fixture {
    _tmp: tempfile::TempDir,
    _env: TestEnvironment,
    _nats_env: NatsEnvReset,
    _broker: SharedNatsServer,
    proxy: GatedNatsProxy,
    direct_js: async_nats::jetstream::Context,
    config: GlobalConfig,
}

impl NatsC01Fixture {
    async fn start() -> Option<Self> {
        if !has_nats_server_binary() {
            eprintln!("skipping: nats-server binary not found");
            return None;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = TestEnvironment::set(tmp.path());
        let nats_env = NatsEnvReset::clear();
        let broker = ensure_shared_server()
            .await
            .expect("start isolated local nats-server");
        let proxy = GatedNatsProxy::start(&broker.url, true).await;

        let direct_cfg = Config {
            nats_servers: vec![test_nats_server_config(
                TEST_CLUSTER,
                broker.url.clone(),
                broker.token.clone(),
            )],
            ..Config::default()
        };
        let direct_js = direct_cfg
            .nats_jetstream(TEST_CLUSTER)
            .await
            .expect("connect direct jetstream");
        let store = SessionMetadataStore::ensure(&direct_js, 1)
            .await
            .expect("ensure metadata store");
        for sid in ["session-a", "session-b"] {
            store
                .create(&SessionMetadata::new(
                    sid,
                    SessionInitializer::named(AGENT_NAME, Default::default()),
                ))
                .await
                .expect("create session metadata");
        }

        let cfg = test_nats_tui_config(
            AGENT_NAME,
            TEST_CLUSTER,
            proxy.url.clone(),
            broker.token.clone(),
            "session-a",
        );

        Some(Self {
            _tmp: tmp,
            _env: env,
            _nats_env: nats_env,
            _broker: broker,
            proxy,
            direct_js,
            config: Arc::new(ConfigLock::new(cfg)),
        })
    }

    async fn seed_session(&self, session_id: &str, messages: &[(MessageRole, &str)]) {
        let log = NatsSessionLog::for_agent(self.direct_js.clone(), AGENT_NAME, session_id)
            .with_replicas(1);
        for (role, text) in messages {
            log.append_event_async(&log_message(*role, text))
                .await
                .expect("append seed message");
        }
    }

    async fn load_entries(&self, session_id: &str) -> Vec<(u64, SessionLogEntry)> {
        NatsSessionLog::for_agent(self.direct_js.clone(), AGENT_NAME, session_id)
            .load_events_async()
            .await
            .expect("load session log entries")
    }
}

async fn send_key(tui: &mut Tui, code: KeyCode, modifiers: KeyModifiers) {
    Box::pin(tui.handle_key(KeyEvent::new(code, modifiers)))
        .await
        .unwrap();
}

#[tokio::test]
async fn nats_cancelled_switch_and_unrelated_error_never_mutate_target_log() {
    let _env_lock = ENV_LOCK.lock().await;
    let Some(fixture) = NatsC01Fixture::start().await else {
        return;
    };
    fixture
        .seed_session("session-a", &[(MessageRole::User, "Session A prompt")])
        .await;
    fixture
        .seed_session(
            "session-b",
            &[
                (MessageRole::User, "Session B prompt"),
                (MessageRole::Assistant, "Session B reply"),
            ],
        )
        .await;
    let b_initial_entries = fixture.load_entries("session-b").await;
    assert_eq!(b_initial_entries.len(), 2);

    let mut tui = Tui::init(&fixture.config).await.unwrap();
    assert!(tui.actionable_transcript_binding().is_some());
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Session A prompt")
    ));

    // Gate proxy closed, switch A -> B via SessionPicker, then Ctrl-C cancel B's load.
    fixture.proxy.close();
    let accepts_before = fixture.proxy.accepted_count();
    tui.app.modal = Some(ModalState::SessionPicker {
        sessions: vec![SessionMeta {
            id: "session-b".to_string(),
            session_id: Some("session-b".to_string()),
            agent_name: Some(AGENT_NAME.to_string()),
            title: None,
            user_id: None,
            modified: None,
            contexts: vec![],
            unread: false,
        }],
        selected: 1,
        origin_agent: Some(AGENT_NAME.to_string()),
        origin_session: Some("session-a".to_string()),
        error: None,
    });
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    fixture.proxy.wait_for_accepts(accepts_before + 1).await;
    send_key(&mut tui, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    tui.flush_history_work_for_test().await;

    // Reopen NATS so any wrongly armed destructive command would reach B's real log.
    fixture.proxy.open();
    tui.set_input_text(".set unknown foo");
    send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    assert!(
        tui.actionable_transcript_binding().is_none(),
        "unrelated command error after cancelled switch must leave view unbound"
    );

    // Attempt destructive keys and typed mutations against A's retained rows.
    send_key(&mut tui, KeyCode::Up, KeyModifiers::NONE).await;
    for key in ['d', 'r', 'e'] {
        send_key(&mut tui, KeyCode::Char(key), KeyModifiers::NONE).await;
        assert!(tui.app.modal.is_none());
    }
    send_key(&mut tui, KeyCode::Esc, KeyModifiers::NONE).await;
    for cmd in [".delete message 1", ".rewind 1", ".edit message 1"] {
        tui.set_input_text(cmd);
        send_key(&mut tui, KeyCode::Enter, KeyModifiers::NONE).await;
    }
    tui.flush_history_work_for_test().await;

    let b_after_entries = fixture.load_entries("session-b").await;
    assert_eq!(
        b_after_entries, b_initial_entries,
        "session B's durable NATS log must remain untouched"
    );

    // Fenced commit for B loads B's real rows and restores legitimate actionability.
    tui.schedule_history_rebuild_for_command(true);
    tui.flush_history_work_for_test().await;
    assert!(tui.actionable_transcript_binding().is_some());
    assert!(tui.app.transcript.iter().any(
        |item| matches!(item, TranscriptItem::UserText { text, .. } if text == "Session B prompt")
    ));
}
