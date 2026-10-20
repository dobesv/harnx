use super::*;
use crate::test_support::{seed_nats_session, NatsSessionSeed, TestConfigSandbox};
use crate::transcript_snapshot::{tests::fixture, HistorySnapshot};
use harnx_runtime::config::LOCAL_CLUSTER_KEY;

async fn seed(config: &Config, through: u64) -> Option<SessionKey> {
    let session_id = format!("compaction-snapshot-{}", uuid::Uuid::new_v4());
    if !seed_nats_session(
        config,
        NatsSessionSeed {
            agent: "plain",
            session_id: &session_id,
            messages: &[],
        },
    )
    .await
    {
        return None;
    }
    let jetstream = config.nats_jetstream(LOCAL_CLUSTER_KEY).await.unwrap();
    let storage_key = harnx_core::session_identity::session_key(Some("plain"), &session_id);
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, storage_key)
        .with_replicas(1);
    for (seq, entry) in fixture(through) {
        assert_eq!(log.append_event_async(&entry).await.unwrap(), seq);
    }
    Some(SessionKey::local("plain", session_id))
}

fn expected(through: u64) -> HistorySnapshot {
    HistorySnapshot::from_entries(
        &fixture(through),
        &harnx_core::session::Session::default(),
        false,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_get_projects_both_compactions_from_shared_fixture() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "Read the compacted history.");
    let config = sandbox.config();
    let registry = SessionRegistry::new(config.clone());
    for through in [6, 23] {
        let Some(key) = seed(&config, through).await else {
            return;
        };
        let info =
            crate::test_support::wait_for_state(&registry.get_or_spawn(key), "idle", |state| {
                matches!(state, SessionState::Idle)
            })
            .await;
        assert_actor_snapshot(&info, &expected(through));
    }
}

fn assert_actor_snapshot(info: &SessionInfo, expected: &HistorySnapshot) {
    assert_eq!(info.compaction, expected.compaction);
    assert_eq!(info.history_snapshot[0], expected.messages[0]);
    assert_eq!(info.history_snapshot.len(), expected.messages.len());
}

fn relay(
    config: Config,
    key: SessionKey,
) -> (
    Arc<dyn AgentEventSink>,
    TurnEventRelay,
    broadcast::Receiver<Event>,
) {
    let (tx, events) = broadcast::channel(BROADCAST_BUFFER);
    let config = Arc::new(ConfigLock::new(config));
    let sink = Arc::new(BroadcastEventSender::new(
        tx,
        MessageId::random(),
        SessionEventContext::new(config.clone(), Vec::new()),
    ));
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Arc::new(TurnEventSender(tx)),
        TurnEventRelay {
            rx,
            sink,
            config,
            key,
        },
        events,
    )
}

fn completed() -> AgentEvent {
    AgentEvent::Session(SessionEvent::CompactingCompleted {
        compaction_id: Some("keep-nothing".into()),
        outcome: harnx_core::session::CompactOutcome::Compacted,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_refreshes_compaction_before_later_events_and_turn_return() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "Read the compacted history.");
    let config = sandbox.config();
    let Some(key) = seed(&config, 6).await else {
        return;
    };
    let (sender, relay, mut events) = relay(config, key);
    let turn = async move {
        sender.emit(completed());
        sender.emit(AgentEvent::Session(SessionEvent::CompactingStarted {
            compaction_id: None,
        }));
    };
    relay.run(Box::pin(turn)).await;
    assert_relay_events(&mut events);
}

fn assert_relay_events(events: &mut broadcast::Receiver<Event>) {
    let completed = serde_json::to_value(events.try_recv().unwrap()).unwrap();
    assert_eq!(completed["value"]["compaction_seq"], 5);
    for expected in expected(6).events() {
        assert_eq!(events.try_recv().unwrap(), expected);
    }
    let later = serde_json::to_value(events.try_recv().unwrap()).unwrap();
    assert_eq!(later["name"], "session_compacting_started");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_lag_recovers_compaction_metadata_before_snapshot() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "Read the compacted history.");
    let config = sandbox.config();
    let Some(key) = seed(&config, 6).await else {
        return;
    };
    let registry = SessionRegistry::new(config);
    let handle = registry.get_or_spawn(key);
    let (tx, rx) = broadcast::channel(1);
    tx.send(crate::transcript_snapshot::compaction_event(None))
        .unwrap();
    tx.send(crate::transcript_snapshot::compaction_event(None))
        .unwrap();
    let events = crate::ag_ui_events::live_subscription_events(&handle, rx);
    futures_util::pin_mut!(events);
    for expected in expected(6).events() {
        assert_eq!(
            futures_util::StreamExt::next(&mut events).await.unwrap(),
            expected
        );
    }
}
