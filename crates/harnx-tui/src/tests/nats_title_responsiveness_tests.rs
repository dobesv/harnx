//! Regression for cycle 2 C02 (residual B5): a `.set title` submitted through
//! the real TUI key path must run as cancellable background work, so while
//! its durable write waits for a still-recovering broker the input loop keeps
//! processing keys and raw Ctrl-C, and cancellation cleans up promptly. The
//! gated broker here is the patient, no-deadline acquisition: the write waits
//! as long as the gate stays closed and never stalls the loop.

use super::nats_background_mark_read_tests::{test_nats_server_config, test_nats_tui_config};
use super::*;
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::{HistoryTarget, ModalState, TranscriptItem, Tui, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{AgentEvent, SessionEvent};
use harnx_runtime::config::{load_session_for_render, ConfigLock, SessionMeta};
use harnx_runtime::nats_local_server::ensure_shared_server;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const AGENT_NAME: &str = "hermes";
const TEST_CLUSTER: &str = "c02-title-responsiveness-cluster";
const ACTION_BUDGET: Duration = Duration::from_secs(5);
const SETTLE_BUDGET: Duration = Duration::from_secs(20);

struct TitleFixture {
    _tmp: tempfile::TempDir,
    _env: TestEnvironment,
    _nats_env: NatsEnvReset,
    broker: harnx_runtime::nats_local_server::SharedNatsServer,
    proxy: super::nats_background_mark_read_tests::GatedNatsProxy,
    direct_store: SessionMetadataStore,
    config: Arc<ConfigLock>,
    session_id: String,
    storage_key: String,
}

impl TitleFixture {
    async fn start(session_id: &str) -> Option<Self> {
        Self::start_with_agents(session_id, &[AGENT_NAME]).await
    }

    async fn start_with_agents(session_id: &str, agents: &[&str]) -> Option<Self> {
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
        let proxy =
            super::nats_background_mark_read_tests::GatedNatsProxy::start(&broker.url, true).await;

        // Direct store access, bypassing the gated proxy, to seed the
        // session's canonical metadata and later assert the durable title.
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
        let direct_store = SessionMetadataStore::ensure(&direct_js, 1)
            .await
            .expect("ensure metadata store");
        for agent in agents {
            direct_store
                .create(&SessionMetadata::new(
                    session_id,
                    SessionInitializer::named(*agent, Default::default()),
                ))
                .await
                .expect("create session metadata");
        }
        let primary_agent = agents[0];
        let storage_key =
            harnx_core::session_identity::session_key(Some(primary_agent), session_id);

        let cfg = test_nats_tui_config(
            primary_agent,
            TEST_CLUSTER,
            proxy.url.clone(),
            broker.token.clone(),
            session_id,
        );
        // The pre-turn state: the selected session must not have a sink, so
        // the title write goes through the patient broker acquisition.
        assert!(
            cfg.session
                .as_ref()
                .expect("selected session")
                .runtime
                .is_none(),
            "pre-turn session must not have a persistence sink"
        );

        Some(Self {
            _tmp: tmp,
            _env: env,
            _nats_env: nats_env,
            broker,
            proxy,
            direct_store,
            config: Arc::new(ConfigLock::new(cfg)),
            session_id: session_id.to_string(),
            storage_key,
        })
    }

    fn fresh_direct_config(&self, agent: &str, session_id: &str) -> Config {
        test_nats_tui_config(
            agent,
            TEST_CLUSTER,
            self.broker.url.clone(),
            self.broker.token.clone(),
            session_id,
        )
    }
}

async fn dispatch_key_unblocked(tui: &mut Tui, key: KeyEvent, context: &str) {
    tokio::time::timeout(ACTION_BUDGET, tui.handle_key(key))
        .await
        .unwrap_or_else(|_| panic!("{context}: handle_key stalled on gated broker"))
        .expect("handle_key failed");
}

async fn submit_dot_command_keys(tui: &mut Tui, line: &str, context: &str) {
    tui.set_input_text(line);
    dispatch_key_unblocked(
        tui,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        context,
    )
    .await;
}

fn transcript_texts(tui: &Tui) -> Vec<String> {
    tui.app
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ErrorText(text) | TranscriptItem::SystemText(text) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn footer_notice_text(tui: &Tui) -> String {
    tui.app
        .footer_notice
        .as_ref()
        .map(|notice| notice.text.clone())
        .unwrap_or_default()
}

async fn park_title_rename(fix: &TitleFixture, harness: &mut TuiTestHarness, title: &str) {
    fix.proxy.close();
    let accepts_before = fix.proxy.accepted_count();
    submit_dot_command_keys(
        harness.tui(),
        &format!(".set title {title}"),
        "submit parked .set title",
    )
    .await;
    assert!(
        harness.tui().title_rename_abort.is_some(),
        "title write must run as single-flight background rename"
    );
    fix.proxy.wait_for_accepts(accepts_before + 1).await;
}

async fn wait_for_durable_title(fix: &TitleFixture, expected: &str) {
    let deadline = Instant::now() + SETTLE_BUDGET;
    loop {
        let record = fix
            .direct_store
            .get(&fix.storage_key)
            .await
            .expect("query durable title")
            .expect("session metadata record");
        if record.metadata.title.value.as_deref() == Some(expected) {
            assert!(record.metadata.title.manual);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for durable title {expected:?} on {}",
            fix.storage_key
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_cancelled_title_cleaned_up(fix: &TitleFixture, harness: &mut TuiTestHarness) {
    harness.tui().flush_history_work_for_test().await;
    assert!(harness.tui().title_rename_abort.is_none());
    assert!(!harness.tui().history_command_running);
    assert!(harness.tui().history_command_queue.is_empty());
    assert!(harness.tui().history_command_abort.is_none());
    let texts = transcript_texts(harness.tui());
    assert!(
        texts.iter().any(|text| text.contains("cancelled")),
        "expected a cancellation error in the transcript, got: {texts:?}"
    );
    assert!(
        fix.config
            .read()
            .session
            .as_ref()
            .unwrap()
            .title()
            .is_none(),
        "cancelled write must not set the in-memory title"
    );
    assert!(
        !harness.tui().app.should_quit,
        "Ctrl-C cancels, never quits"
    );
}

async fn assert_retry_title_persists_and_reloads(fix: &TitleFixture, harness: &mut TuiTestHarness) {
    fix.proxy.open();
    submit_dot_command_keys(harness.tui(), ".set title After Cancel", "retry .set title").await;
    harness.tui().flush_history_work_for_test().await;
    assert!(harness.tui().title_rename_abort.is_none());
    assert_eq!(
        fix.config.read().session.as_ref().unwrap().title(),
        Some("After Cancel")
    );
    assert_eq!(
        fix.config
            .read()
            .session
            .as_ref()
            .unwrap()
            .title_last_updated_tokens(),
        usize::MAX
    );
    wait_for_durable_title(fix, "After Cancel").await;
    let fresh_cfg = fix.fresh_direct_config(AGENT_NAME, &fix.session_id);
    let reloaded =
        load_session_for_render(&fresh_cfg, Some(TEST_CLUSTER), &fix.session_id, AGENT_NAME)
            .await
            .expect("reload session after cancel/retry");
    assert_eq!(reloaded.title(), Some("After Cancel"));
    assert_eq!(reloaded.title_last_updated_tokens(), usize::MAX);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_set_title_waits_off_loop_and_ctrl_c_cancels_then_retry_persists() {
    let _lock = ENV_LOCK.lock().await;
    let session_id = format!("c02-title-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start(&session_id).await else {
        return;
    };
    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;

    park_title_rename(&fix, &mut harness, "Waiting Behind Gate").await;
    assert!(!harness.tui().history_command_running);

    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        "type while title write waits",
    )
    .await;
    assert!(harness
        .tui()
        .app
        .input
        .lines()
        .iter()
        .any(|line| line.contains('z')));

    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        "Ctrl-C during gated title write",
    )
    .await;
    assert_cancelled_title_cleaned_up(&fix, &mut harness).await;
    assert_retry_title_persists_and_reloads(&fix, &mut harness).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_second_title_rejected_and_retained_while_first_parked_and_same_id_switch_never_persists(
) {
    let _lock = ENV_LOCK.lock().await;
    let shared_id = format!("c02-shared-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start_with_agents(&shared_id, &[AGENT_NAME, "athena"]).await
    else {
        return;
    };
    let key_a = harnx_core::session_identity::session_key(Some(AGENT_NAME), &shared_id);
    let key_b = harnx_core::session_identity::session_key(Some("athena"), &shared_id);
    assert_ne!(key_a, key_b);

    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;
    fix.proxy.close();
    let accepts_before = fix.proxy.accepted_count();

    // 1. Submit first title on A via actual Enter; parks on closed broker gate.
    submit_dot_command_keys(
        harness.tui(),
        ".set title First On A",
        "submit first .set title on A",
    )
    .await;
    assert!(harness.tui().title_rename_abort.is_some());
    fix.proxy.wait_for_accepts(accepts_before + 1).await;

    // 2. Submit a second title while the first is parked: rejected visibly and
    //    retained in the input draft rather than queued unbound.
    submit_dot_command_keys(
        harness.tui(),
        ".set title Second Never B",
        "submit second .set title while first parked",
    )
    .await;
    let notice = footer_notice_text(harness.tui());
    assert!(
        notice.contains("already in progress"),
        "expected busy rename notice, got: {notice:?}"
    );
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        ".set title Second Never B",
        "second title draft must remain in input on rejection"
    );
    assert!(harness.tui().history_command_queue.is_empty());

    // 3. Navigate A -> B sharing the exact same short session id via inline
    //    `.agent` command.
    let switch_cmd = format!(".agent athena@{TEST_CLUSTER} {shared_id}");
    submit_dot_command_keys(
        harness.tui(),
        &switch_cmd,
        "switch A -> B with same short id",
    )
    .await;
    assert!(
        harness.tui().title_rename_abort.is_none(),
        "inline .agent switch must retire pending title rename"
    );

    // 4. Open broker gate and flush all background work: stale pending A work
    //    must be rejected before persisting, and neither A nor B may receive
    //    A's parked or rejected second title.
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;

    let record_a = fix
        .direct_store
        .get(&key_a)
        .await
        .expect("query A metadata")
        .expect("A metadata record");
    assert_eq!(record_a.metadata.title.value, None);
    assert!(!record_a.metadata.title.manual);

    let record_b = fix
        .direct_store
        .get(&key_b)
        .await
        .expect("query B metadata")
        .expect("B metadata record");
    assert_eq!(record_b.metadata.title.value, None);
    assert!(!record_b.metadata.title.manual);

    {
        let cfg = fix.config.read();
        let session_b = cfg.session.as_ref().expect("active B session");
        assert_eq!(session_b.agent_name(), Some("athena"));
        assert_eq!(session_b.id(), shared_id);
        assert_eq!(session_b.title(), None);
        assert_eq!(session_b.title_last_updated_tokens(), 0);
    }
    let chrome_title = harness.tui().build_input_title().to_string();
    assert!(
        !chrome_title.contains("First On A") && !chrome_title.contains("Second Never B"),
        "B input chrome must not show A's title: {chrome_title:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_preturn_durable_title_survives_fresh_config_load_and_ignores_unbound_rows() {
    let _lock = ENV_LOCK.lock().await;
    let session_id = format!("c02-preturn-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start(&session_id).await else {
        return;
    };
    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;

    // Retire transcript row bindings so no transcript row is actionable;
    // `.set title` must remain completely independent of row bindings.
    harness.tui().retire_prior_target_transcript_actions();
    assert!(harness.tui().actionable_transcript_binding().is_none());

    submit_dot_command_keys(
        harness.tui(),
        ".set title PreTurn Unbound Survives",
        "submit pre-turn .set title with unbound rows",
    )
    .await;
    harness.tui().flush_history_work_for_test().await;

    wait_for_durable_title(&fix, "PreTurn Unbound Survives").await;
    {
        let cfg = fix.config.read();
        let session = cfg.session.as_ref().expect("active session");
        assert_eq!(session.title(), Some("PreTurn Unbound Survives"));
        assert_eq!(session.title_last_updated_tokens(), usize::MAX);
    }
    assert!(
        !transcript_texts(harness.tui())
            .iter()
            .any(|text| text.contains("Cannot durably set the title")),
        "pre-turn title write must succeed without needing an active sink"
    );

    let fresh_cfg = fix.fresh_direct_config(AGENT_NAME, &session_id);
    let reloaded = load_session_for_render(&fresh_cfg, Some(TEST_CLUSTER), &session_id, AGENT_NAME)
        .await
        .expect("reload session from fresh config");
    assert_eq!(reloaded.title(), Some("PreTurn Unbound Survives"));
    assert_eq!(reloaded.title_last_updated_tokens(), usize::MAX);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_busy_history_command_rejects_and_retains_second_title_draft() {
    let _lock = ENV_LOCK.lock().await;
    let session_id = format!("c02-busy-history-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start(&session_id).await else {
        return;
    };
    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;

    fix.proxy.close();
    let accepts_before = fix.proxy.accepted_count();
    submit_dot_command_keys(
        harness.tui(),
        ".empty session",
        "park history command on gated broker",
    )
    .await;
    assert!(harness.tui().history_command_running);
    fix.proxy.wait_for_accepts(accepts_before + 1).await;

    submit_dot_command_keys(
        harness.tui(),
        ".set title Held During History",
        "submit .set title while history command runs",
    )
    .await;
    let notice = footer_notice_text(harness.tui());
    assert!(
        notice.contains("Session command in progress"),
        "expected history-busy notice, got: {notice:?}"
    );
    assert_eq!(
        harness.tui().app.input.lines().join("\n"),
        ".set title Held During History"
    );
    assert!(harness.tui().title_rename_abort.is_none());

    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        "Ctrl-C to cancel parked history command",
    )
    .await;
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;
}

fn stale_title_event(
    seq: u64,
    shared_id: &str,
    title: &str,
    result: anyhow::Result<()>,
) -> TuiEvent {
    TuiEvent::TitleRenameFinished {
        seq,
        target: HistoryTarget {
            session_id: Some(shared_id.to_string()),
            agent: Some(format!("{AGENT_NAME}@{TEST_CLUSTER}")),
            cluster: TEST_CLUSTER.to_string(),
        },
        storage_key: Some(harnx_core::session_identity::session_key(
            Some(AGENT_NAME),
            shared_id,
        )),
        title: title.to_string(),
        result,
    }
}

async fn assert_stale_completion_fences_chrome(
    fix: &TitleFixture,
    harness: &mut TuiTestHarness,
    shared_id: &str,
) {
    let switch_cmd = format!(".agent athena@{TEST_CLUSTER} {shared_id}");
    submit_dot_command_keys(harness.tui(), &switch_cmd, "switch A -> B").await;
    harness.tui().flush_history_work_for_test().await;

    let seq = harness.tui().title_rename_seq;
    harness
        .tui()
        .handle_tui_event(stale_title_event(seq, shared_id, "Stale A Success", Ok(())))
        .await
        .expect("handle stale Ok TitleRenameFinished");
    harness
        .tui()
        .handle_tui_event(stale_title_event(
            seq,
            shared_id,
            "Stale A Chrome",
            Err(anyhow::anyhow!("stale failure must be ignored")),
        ))
        .await
        .expect("handle stale Err TitleRenameFinished");
    assert_eq!(fix.config.read().session.as_ref().unwrap().title(), None);
    assert!(!transcript_texts(harness.tui())
        .iter()
        .any(|t| t.contains("stale failure must be ignored")));
}

async fn assert_picker_paths_retire_rename(
    fix: &TitleFixture,
    harness: &mut TuiTestHarness,
    shared_id: &str,
) {
    park_title_rename(fix, harness, "Picker Parked").await;
    harness.tui().app.modal = Some(ModalState::AgentPicker {
        agents: vec![format!("{AGENT_NAME}@{TEST_CLUSTER}")],
        selected: 0,
        query: String::new(),
    });
    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        "accept AgentPicker",
    )
    .await;
    assert!(harness.tui().title_rename_abort.is_none());
    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        "cancel SessionPicker",
    )
    .await;
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;

    park_title_rename(fix, harness, "SessionPicker Parked").await;
    harness.tui().app.modal = Some(ModalState::SessionPicker {
        sessions: vec![SessionMeta {
            id: shared_id.to_string(),
            session_id: Some(shared_id.to_string()),
            agent_name: Some("athena".to_string()),
            title: None,
            user_id: None,
            modified: None,
            contexts: vec![],
            unread: false,
        }],
        selected: 1,
        origin_agent: Some(format!("athena@{TEST_CLUSTER}")),
        origin_session: Some(shared_id.to_string()),
        error: None,
    });
    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        "accept SessionPicker",
    )
    .await;
    assert!(harness.tui().title_rename_abort.is_none());
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;
}

async fn assert_handoff_paths_retire_rename(
    fix: &TitleFixture,
    harness: &mut TuiTestHarness,
    shared_id: &str,
) {
    park_title_rename(fix, harness, "Handoff Parked").await;
    let task = harnx_runtime::utils::create_abort_signal();
    harness.tui().current_prompt_abort = Some(task.clone());
    let stamp = crate::event_isolation::EventStamp::live(&harness.tui().live_events);
    harness
        .tui()
        .handle_tui_event(TuiEvent::Agent {
            task,
            stamp,
            event: AgentEvent::Session(SessionEvent::HandoffCommitted {
                agent: AGENT_NAME.to_string(),
                session_id: shared_id.to_string(),
                handoff_tool_call_id: None,
                after_seq: None,
            }),
        })
        .await
        .expect("handle HandoffCommitted");
    assert!(harness.tui().title_rename_abort.is_none());
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;
    harness.tui().app.llm_busy = false;

    park_title_rename(fix, harness, "SavedLink Parked").await;
    let link_idx = harness.tui().app.transcript.len();
    harness
        .tui()
        .app
        .transcript
        .push(TranscriptItem::HandoffSession {
            agent: "athena".to_string(),
            session_id: shared_id.to_string(),
        });
    harness.tui().app.transcript_focus = Some(link_idx);
    dispatch_key_unblocked(
        harness.tui(),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        "open focused HandoffSession link",
    )
    .await;
    assert!(harness.tui().title_rename_abort.is_none());
    fix.proxy.open();
    harness.tui().flush_history_work_for_test().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_stale_inflight_completion_fences_chrome_and_all_navigation_paths_retire_rename() {
    let _lock = ENV_LOCK.lock().await;
    let shared_id = format!("c02-nav-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start_with_agents(&shared_id, &[AGENT_NAME, "athena"]).await
    else {
        return;
    };
    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;

    assert_stale_completion_fences_chrome(&fix, &mut harness, &shared_id).await;
    assert_picker_paths_retire_rename(&fix, &mut harness, &shared_id).await;
    assert_handoff_paths_retire_rename(&fix, &mut harness, &shared_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_inline_command_during_background_title_write_does_not_deadlock() {
    let _lock = ENV_LOCK.lock().await;
    let shared_id = format!("c4f2-inline-{}", uuid::Uuid::now_v7());
    let Some(fix) = TitleFixture::start_with_agents(&shared_id, &[AGENT_NAME, "athena"]).await
    else {
        return;
    };
    let mut harness = TuiTestHarness::with_config(fix.config.clone()).await;

    park_title_rename(&fix, &mut harness, "Concurrent Title").await;
    fix.proxy.open();

    // Run inline dot commands while the background title write completes and
    // takes `config.write()` inside `apply_recorded_title`.
    submit_dot_command_keys(
        harness.tui(),
        ".set dry_run true",
        "inline .set dry_run during background title write",
    )
    .await;
    let switch_cmd = format!(
        ".agent {agent}@{TEST_CLUSTER} {shared_id}",
        agent = "athena"
    );
    submit_dot_command_keys(
        harness.tui(),
        &switch_cmd,
        "inline .agent during background title write",
    )
    .await;

    assert!(harness.tui().title_rename_abort.is_none());
    harness.tui().flush_history_work_for_test().await;
    assert_eq!(
        fix.config.read().session.as_ref().unwrap().agent_name(),
        Some("athena")
    );
}
