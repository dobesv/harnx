//! Regression tests for B6: `mark_current_session_read_background` when
//! `current_session_unread == true` under gated broker access.
//!
//! Uses isolated `TestEnvironment` (`ENV_LOCK`) and an isolated local NATS
//! broker fronted by a gated TCP proxy, with `nats_` test naming so nextest
//! places every test in `broker-e2e` / `broker-e2e-local`.

use super::*;
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::{ModalState, PendingMessage, TranscriptItem, TuiEvent};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::config::{Config, ConfigLock, NatsServerConfig};
use harnx_runtime::nats_local_server::{ensure_shared_server, SharedNatsServer};
use harnx_runtime::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use harnx_runtime::SessionInitializer;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

const AGENT_NAME: &str = "hermes";
const TEST_CLUSTER: &str = "b6-mark-read-cluster";
const ACTION_BUDGET: Duration = Duration::from_secs(5);
const SETTLE_BUDGET: Duration = Duration::from_secs(20);

/// Deterministic TCP gate in front of an isolated local NATS server.
///
/// While closed, accepted client connections park before reaching upstream
/// NATS so tests can verify immediate UI responsiveness and optimistic chrome
/// updates, confirm the durable store is still unread, switch targets, and
/// then open the gate to prove the captured write lands on the intended
/// session.
pub(super) struct GatedNatsProxy {
    pub(super) url: String,
    gate_tx: watch::Sender<bool>,
    accepted: Arc<AtomicUsize>,
    accepted_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    accept_task: JoinHandle<()>,
}

impl GatedNatsProxy {
    pub(super) async fn start(upstream_url: &str, initially_open: bool) -> Self {
        let upstream_addr: SocketAddr = upstream_url
            .strip_prefix("nats://")
            .expect("nats:// URL")
            .parse()
            .expect("upstream socket address");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gated proxy");
        let local_addr = listener.local_addr().expect("proxy local addr");
        let (gate_tx, gate_rx) = watch::channel(initially_open);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_notify = Arc::new(Notify::new());

        let accepted_bg = Arc::clone(&accepted);
        let notify_bg = Arc::clone(&accepted_notify);
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    res = listener.accept() => {
                        let Ok((mut client, _)) = res else { break; };
                        accepted_bg.fetch_add(1, Ordering::SeqCst);
                        notify_bg.notify_waiters();
                        let mut gate_rx = gate_rx.clone();
                        let mut conn_shutdown = shutdown_rx.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = conn_shutdown.changed() => return,
                                res = gate_rx.wait_for(|open| *open) => {
                                    if res.is_err() {
                                        return;
                                    }
                                }
                            }
                            let Ok(mut upstream) = tokio::net::TcpStream::connect(upstream_addr).await else {
                                return;
                            };
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                        });
                    }
                }
            }
        });

        Self {
            url: format!("nats://127.0.0.1:{}", local_addr.port()),
            gate_tx,
            accepted,
            accepted_notify,
            shutdown_tx,
            accept_task,
        }
    }

    pub(super) fn close(&self) {
        let _ = self.gate_tx.send(false);
    }

    pub(super) fn open(&self) {
        let _ = self.gate_tx.send(true);
    }

    pub(super) fn accepted_count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    pub(super) async fn wait_for_accepts(&self, target: usize) {
        tokio::time::timeout(SETTLE_BUDGET, async {
            loop {
                let notified = self.accepted_notify.notified();
                if self.accepted.load(Ordering::SeqCst) >= target {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("timed out waiting for gated proxy to accept background broker connection");
    }
}

impl Drop for GatedNatsProxy {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.accept_task.abort();
    }
}

pub(super) fn test_nats_server_config(
    cluster: &str,
    url: String,
    token: String,
) -> NatsServerConfig {
    NatsServerConfig {
        user_id: None,
        name: cluster.to_string(),
        url,
        token: Some(token),
        replicas: Some(1),
        tls: None,
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
        ignore_discovered_servers: Some(true),
        agents: vec![],
    }
}

pub(super) fn test_nats_tui_config(
    agent_name: &str,
    cluster: &str,
    nats_url: String,
    token: String,
    initial_session_id: &str,
) -> Config {
    let model = MockClient::builder().build().model().clone();
    let mut agent =
        harnx_runtime::config::Agent::new(harnx_runtime::config::AgentConfig::from_prompt(""));
    agent.set_name(agent_name);
    agent.set_model(model.clone());
    let mut cfg = Config {
        clients: vec![ClientConfig::Unknown],
        model,
        agent: Some(agent),
        nats_servers: vec![test_nats_server_config(cluster, nats_url, token)],
        remote_agent: Some((agent_name.to_string(), cluster.to_string())),
        ..Config::default()
    };
    cfg.session = Some(
        harnx_runtime::config::session::new(&cfg, initial_session_id, None)
            .expect("create initial session"),
    );
    cfg
}

struct IsolatedNatsFixture {
    _tmp: tempfile::TempDir,
    _env: TestEnvironment,
    _nats_env: NatsEnvReset,
    broker: SharedNatsServer,
    proxy: GatedNatsProxy,
    direct_store: SessionMetadataStore,
    config: GlobalConfig,
}

impl IsolatedNatsFixture {
    async fn start(initial_session_id: &str) -> Option<Self> {
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
        let direct_store = SessionMetadataStore::ensure(&direct_js, 1)
            .await
            .expect("ensure metadata store");

        let cfg = test_nats_tui_config(
            AGENT_NAME,
            TEST_CLUSTER,
            proxy.url.clone(),
            broker.token.clone(),
            initial_session_id,
        );

        Some(Self {
            _tmp: tmp,
            _env: env,
            _nats_env: nats_env,
            broker,
            proxy,
            direct_store,
            config: Arc::new(ConfigLock::new(cfg)),
        })
    }

    async fn seed_unread_session(&self, session_id: &str) -> String {
        self.direct_store
            .create(&SessionMetadata::new(
                session_id,
                SessionInitializer::named(AGENT_NAME, Default::default()),
            ))
            .await
            .expect("create session metadata");
        let storage_key = harnx_core::session_identity::session_key(Some(AGENT_NAME), session_id);
        self.direct_store
            .mark_unread(&storage_key)
            .await
            .expect("mark session unread");
        assert!(
            self.direct_store
                .get_read_state(&storage_key)
                .await
                .expect("read seeded unread state")
                .is_unread(),
            "seeded session {session_id} must start unread"
        );
        storage_key
    }

    async fn wait_until_durable_read(&self, storage_key: &str) {
        let deadline = tokio::time::Instant::now() + SETTLE_BUDGET;
        loop {
            if !self
                .direct_store
                .get_read_state(storage_key)
                .await
                .expect("query read state")
                .is_unread()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for durable mark-read on {storage_key}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn picker_meta(id: &str, unread: bool) -> harnx_runtime::config::SessionMeta {
    harnx_runtime::config::SessionMeta {
        id: id.to_string(),
        session_id: Some(id.to_string()),
        agent_name: Some(AGENT_NAME.to_string()),
        title: None,
        user_id: None,
        modified: None,
        contexts: vec![],
        unread,
    }
}

async fn sync_unread_chrome_from_store(harness: &mut TuiTestHarness, storage_key: &str) {
    harness
        .tui()
        .handle_tui_event(TuiEvent::SessionReadInvalidation {
            session_id: storage_key.to_string(),
        })
        .await
        .expect("apply session read invalidation");
    assert!(
        harness.tui().app.current_session_unread,
        "expected current_session_unread=true for {storage_key}"
    );
    assert!(
        line_to_plain(&harness.tui().build_input_title()).contains("● "),
        "input title should display unread indicator before key dispatch"
    );
    harness.render();
    if !harness.tui().app.transcript_browsing {
        assert!(
            harness.screen_contents().contains("● "),
            "rendered screen should display unread indicator before key dispatch"
        );
    }
}

fn assert_optimistic_read_chrome(harness: &mut TuiTestHarness, context: &str) {
    assert!(
        !harness.tui().app.current_session_unread,
        "{context}: current_session_unread must clear optimistically before broker responds"
    );
    assert!(
        !line_to_plain(&harness.tui().build_input_title()).contains("● "),
        "{context}: input title must drop unread indicator optimistically"
    );
    harness.render();
    assert!(
        !harness.screen_contents().contains("● "),
        "{context}: rendered screen must drop unread indicator optimistically"
    );
}

async fn dispatch_key_unblocked(harness: &mut TuiTestHarness, key: KeyEvent, context: &str) {
    tokio::time::timeout(ACTION_BUDGET, harness.tui().handle_key(key))
        .await
        .unwrap_or_else(|_| panic!("{context}: handle_key stalled on gated broker"))
        .expect("handle_key failed");
}

fn ensure_navigable_transcript_row(tui: &mut Tui) {
    if tui.app.transcript.is_empty() {
        tui.app.transcript.push(TranscriptItem::UserText {
            text: "hello".to_string(),
            seq: Some(1),
            timestamp: None,
        });
    }
}

#[derive(Clone, Copy)]
struct MarkReadInputPath {
    label: &'static str,
    key: KeyEvent,
    prepare: fn(&mut Tui),
    assert_side_effect: fn(&Tui, &str),
}

impl MarkReadInputPath {
    fn new(
        label: &'static str,
        key: KeyEvent,
        prepare: fn(&mut Tui),
        assert_side_effect: fn(&Tui, &str),
    ) -> Self {
        Self {
            label,
            key,
            prepare,
            assert_side_effect,
        }
    }

    fn all() -> [Self; 7] {
        [
            Self::new(
                "draft-char",
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                |_| {},
                |tui, label| assert_eq!(tui.app.input.lines().join("\n"), "x", "{label}"),
            ),
            Self::new(
                "enter-nonempty",
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                |tui| {
                    tui.app.llm_busy = true;
                    tui.set_input_text("queued follow-up");
                },
                |tui, label| {
                    assert!(
                        tui.app.pending_message.is_some(),
                        "{label}: busy Enter queues follow-up"
                    );
                },
            ),
            Self::new(
                "esc-idle",
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                |_| {},
                |_, _| {},
            ),
            Self::new(
                "esc-transcript-focus",
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                |tui| {
                    ensure_navigable_transcript_row(tui);
                    tui.app.transcript_focus = Some(0);
                    tui.app.transcript_selection_anchor = Some(0);
                },
                |tui, label| {
                    assert_eq!(tui.app.transcript_focus, None, "{label}");
                    assert!(tui.app.scroll_state.follow, "{label}");
                },
            ),
            Self::new(
                "esc-transcript-browsing",
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                |tui| {
                    ensure_navigable_transcript_row(tui);
                    tui.app.transcript_browsing = true;
                    tui.app.transcript_focus = Some(0);
                    tui.app.transcript_selection_anchor = Some(0);
                },
                |tui, label| {
                    assert!(!tui.app.transcript_browsing, "{label}");
                    assert_eq!(tui.app.transcript_focus, None, "{label}");
                    assert!(tui.app.scroll_state.follow, "{label}");
                },
            ),
            Self::new(
                "ctrl-c",
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                |_| {},
                |tui, label| assert!(tui.abort_signal.aborted_ctrlc(), "{label}"),
            ),
            Self::new(
                "busy-ctrl-d",
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                |tui| tui.app.llm_busy = true,
                |tui, label| {
                    assert!(
                        matches!(tui.app.modal, Some(ModalState::ConfirmExit { .. })),
                        "{label}: busy Ctrl+D opens ConfirmExit"
                    );
                    assert!(!tui.app.should_quit, "{label}");
                    assert!(!tui.abort_signal.aborted_ctrld(), "{label}");
                },
            ),
        ]
    }

    fn prepare(self, tui: &mut Tui) {
        tui.app.modal = None;
        tui.app.detail_view_open = false;
        tui.app.transcript_browsing = false;
        tui.app.transcript_focus = None;
        tui.app.transcript_selection_anchor = None;
        tui.app.completions.clear();
        tui.app.pending_message = None;
        tui.app.llm_busy = false;
        tui.app.should_quit = false;
        tui.set_input_text("");
        tui.abort_signal.reset();
        (self.prepare)(tui);
    }

    fn assert_immediate_side_effect(self, tui: &Tui) {
        (self.assert_side_effect)(tui, self.label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_background_mark_read_all_input_paths_update_optimistically_and_persist_after_gate() {
    let _lock = ENV_LOCK.lock().await;
    let initial_id = format!("b6-init-{}", uuid::Uuid::now_v7());
    let Some(fixture) = IsolatedNatsFixture::start(&initial_id).await else {
        return;
    };
    fixture.seed_unread_session(&initial_id).await;

    let mut harness = TuiTestHarness::with_config(fixture.config.clone()).await;

    for (idx, path) in MarkReadInputPath::all().into_iter().enumerate() {
        let session_id = if idx == 0 {
            initial_id.clone()
        } else {
            let next_id = format!("b6-{}-{}", path.label, uuid::Uuid::now_v7());
            fixture.seed_unread_session(&next_id).await;
            fixture
                .config
                .write()
                .use_session(Some(&next_id))
                .expect("switch config to path session");
            next_id
        };
        let storage_key = harnx_core::session_identity::session_key(Some(AGENT_NAME), &session_id);

        fixture.proxy.open();
        path.prepare(harness.tui());
        sync_unread_chrome_from_store(&mut harness, &storage_key).await;

        fixture.proxy.close();
        let accepts_before = fixture.proxy.accepted_count();
        dispatch_key_unblocked(&mut harness, path.key, path.label).await;

        assert_optimistic_read_chrome(&mut harness, path.label);
        path.assert_immediate_side_effect(harness.tui());

        fixture.proxy.wait_for_accepts(accepts_before + 1).await;
        assert!(
            fixture
                .direct_store
                .get_read_state(&storage_key)
                .await
                .expect("read gated state")
                .is_unread(),
            "{}: broker must remain unread while proxy gate is closed",
            path.label
        );

        fixture.proxy.open();
        fixture.wait_until_durable_read(&storage_key).await;
    }

    let _ = &fixture.broker;
    harness.drain_and_settle().await.unwrap();
}

async fn assert_switch_keeps_session_b_unread(
    fixture: &IsolatedNatsFixture,
    harness: &mut TuiTestHarness,
    key_a: &str,
    key_b: &str,
) {
    fixture.proxy.open();
    fixture.wait_until_durable_read(key_a).await;

    let rebuild_deadline = tokio::time::Instant::now() + SETTLE_BUDGET;
    loop {
        while let Ok(event) = harness.tui().event_rx.try_recv() {
            harness.tui().handle_tui_event(event).await.unwrap();
        }
        let active_binding_matches = harness
            .tui()
            .transcript_view_ownership
            .active_binding
            .as_ref()
            .is_some_and(|binding| binding.storage_key.as_deref() == Some(key_b));
        if harness.tui().history_rebuild_abort.is_none() && active_binding_matches {
            break;
        }
        assert!(
            tokio::time::Instant::now() < rebuild_deadline,
            "timed out settling session B history rebuild after gate opened"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(fixture
        .direct_store
        .get_read_state(key_b)
        .await
        .unwrap()
        .is_unread());
    assert!(harness.tui().app.current_session_unread);
    harness.render();
    assert!(harness.screen_contents().contains("● "));
    assert_eq!(
        harness
            .tui()
            .transcript_view_ownership
            .active_binding
            .as_ref()
            .and_then(|binding| binding.storage_key.as_deref()),
        Some(key_b)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_background_mark_read_target_switch_while_pending_marks_captured_session_without_stall(
) {
    let _lock = ENV_LOCK.lock().await;
    let session_a = format!("b6-switch-a-{}", uuid::Uuid::now_v7());
    let session_b = format!("b6-switch-b-{}", uuid::Uuid::now_v7());
    let Some(fixture) = IsolatedNatsFixture::start(&session_a).await else {
        return;
    };
    let key_a = fixture.seed_unread_session(&session_a).await;
    let key_b = fixture.seed_unread_session(&session_b).await;

    let mut harness = TuiTestHarness::with_config(fixture.config.clone()).await;
    sync_unread_chrome_from_store(&mut harness, &key_a).await;

    // Gate broker access, then dispatch a real draft key on session A.
    fixture.proxy.close();
    let accepts_before = fixture.proxy.accepted_count();
    dispatch_key_unblocked(
        &mut harness,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        "draft key on session A",
    )
    .await;

    assert_optimistic_read_chrome(&mut harness, "session A optimistic update");
    fixture.proxy.wait_for_accepts(accepts_before + 1).await;
    assert!(
        fixture
            .direct_store
            .get_read_state(&key_a)
            .await
            .unwrap()
            .is_unread(),
        "session A must still be unread on broker while gate is closed"
    );

    // Switch target to unread session B via real picker Enter while session A's
    // background mark-read write is still parked on the closed broker gate.
    harness.tui().app.modal = Some(ModalState::SessionPicker {
        sessions: vec![
            picker_meta(&session_a, false),
            picker_meta(&session_b, true),
        ],
        selected: 2,
        origin_agent: Some(AGENT_NAME.to_string()),
        origin_session: Some(session_a.clone()),
        error: None,
    });
    dispatch_key_unblocked(
        &mut harness,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        "switch to session B while mark-read pending",
    )
    .await;

    assert!(harness.tui().app.modal.is_none());
    assert_eq!(
        harness
            .tui()
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.id().to_string())
            .as_deref(),
        Some(session_b.as_str())
    );
    assert!(
        harness.tui().app.current_session_unread,
        "switching to unread session B must restore B's unread flag"
    );
    harness.render();
    assert!(
        harness.screen_contents().contains("● "),
        "rendered screen must show session B's unread indicator while A's write is pending"
    );
    assert!(
        fixture
            .direct_store
            .get_read_state(&key_b)
            .await
            .unwrap()
            .is_unread(),
        "session B must remain unread on broker before gate opens"
    );

    assert_switch_keeps_session_b_unread(&fixture, &mut harness, &key_a, &key_b).await;

    harness.drain_and_settle().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_background_mark_read_ctrl_c_preserves_prompt_cancel_and_ownership_while_gated() {
    let _lock = ENV_LOCK.lock().await;
    let session_a = format!("b6-ctrlc-{}", uuid::Uuid::now_v7());
    let Some(fixture) = IsolatedNatsFixture::start(&session_a).await else {
        return;
    };
    let key_a = fixture.seed_unread_session(&session_a).await;

    let mut harness = TuiTestHarness::with_config(fixture.config.clone()).await;
    sync_unread_chrome_from_store(&mut harness, &key_a).await;

    let prompt_abort = harnx_runtime::utils::create_abort_signal();
    harness.tui().app.llm_busy = true;
    harness.tui().current_prompt_abort = Some(prompt_abort.clone());
    harness.tui().app.pending_message = Some(PendingMessage {
        text: "queued message".to_string(),
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
    });
    let expected_binding = harness
        .tui()
        .transcript_view_ownership
        .active_binding
        .clone();

    fixture.proxy.close();
    let accepts_before = fixture.proxy.accepted_count();
    dispatch_key_unblocked(
        &mut harness,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        "Ctrl+C during active prompt while unread",
    )
    .await;

    assert!(harness.tui().abort_signal.aborted_ctrlc());
    assert!(prompt_abort.aborted_ctrlc());
    assert!(harness.tui().app.pending_message.is_none());
    assert_eq!(
        harness.tui().transcript_view_ownership.active_binding,
        expected_binding
    );
    assert_optimistic_read_chrome(&mut harness, "Ctrl+C optimistic update");

    fixture.proxy.wait_for_accepts(accepts_before + 1).await;
    assert!(
        fixture
            .direct_store
            .get_read_state(&key_a)
            .await
            .unwrap()
            .is_unread(),
        "session must stay unread on broker while gate is closed"
    );

    fixture.proxy.open();
    fixture.wait_until_durable_read(&key_a).await;
    harness.drain_and_settle().await.unwrap();
}
