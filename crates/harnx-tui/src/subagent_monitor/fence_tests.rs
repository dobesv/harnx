//! A cancel fence is a sequence number in one session's log. Each session's
//! log numbers its entries from 1, so a fence raised for one session must
//! never be compared with another session's advisories.
use super::*;
use crate::tests::test_config;
use harnx_core::event::NoticeEvent;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_runtime::nats_event_sink::{AdvisoryEnvelope, LiveEventState};

/// No server is configured for this cluster, so the monitor that
/// `ensure_subagent_monitor` spawns fails to attach right away and these
/// tests never start or join a broker.
const UNCONFIGURED_CLUSTER: &str = "unconfigured-cluster";

fn child_key(agent: &str) -> MonitoredSessionKey {
    MonitoredSessionKey {
        cluster: UNCONFIGURED_CLUSTER.into(),
        agent: agent.into(),
        session_id: "asl7Gw".into(),
    }
}

fn child_live(tui: &Tui, key: &MonitoredSessionKey) -> LiveEventState {
    tui.app.monitored_sessions[key].live_events.clone()
}

fn advisory(after_seq: u64) -> AdvisoryEnvelope {
    AdvisoryEnvelope::new(
        after_seq,
        AgentEvent::Notice(NoticeEvent::Info("live output".into())),
    )
}

/// The history of a child invocation that ran out its deadline: the log
/// ends with the deadline's `Cancel`, at `cancel_seq` in the child's log.
fn deadline_cancelled_history(cancel_seq: u64) -> Vec<(u64, SessionLogEntry)> {
    let prompt = SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("review the change".into()),
        timestamp: None,
        fence_token: None,
    };
    let cancel = SessionLogEntry::cancel_request("deadline:child".into(), "worker deadline".into());
    vec![(1, prompt), (cancel_seq, cancel)]
}

fn open_root_session(config: &GlobalConfig, name: &str) {
    let mut cfg = config.write();
    let session = harnx_runtime::config::session::new(&cfg, name, None).unwrap();
    cfg.session = Some(session);
}

#[tokio::test]
async fn a_cancelled_child_leaves_the_parent_turn_live() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    // `start_prompt` hands the parent turn's follower a clone of the root
    // state, and the follower checks every advisory against it.
    let parent_follower = tui.live_events.clone();
    let reviewer = child_key("euterpe");
    tui.ensure_subagent_monitor(reviewer.clone());

    // A reviewer hit its deadline after more tool rounds than the parent
    // has log entries.
    fence_live_events_from_history(
        &child_live(&tui, &reviewer),
        &deadline_cancelled_history(108),
    );

    assert!(
        parent_follower.should_render(&advisory(24), 24),
        "a child's Cancel fenced the parent turn's live output"
    );
}

#[tokio::test]
async fn a_cancelled_child_leaves_its_siblings_live() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let cancelled = child_key("euterpe");
    let sibling = child_key("thalia");
    tui.ensure_subagent_monitor(cancelled.clone());
    tui.ensure_subagent_monitor(sibling.clone());

    fence_live_events_from_history(
        &child_live(&tui, &cancelled),
        &deadline_cancelled_history(108),
    );

    assert!(
        child_live(&tui, &sibling).should_render(&advisory(3), 3),
        "one child's Cancel fenced a sibling's live output"
    );
    assert!(
        !child_live(&tui, &cancelled).should_render(&advisory(107), 0),
        "the cancelled child must stay fenced at its own Cancel"
    );
}

#[tokio::test]
async fn a_parent_interrupt_leaves_a_later_child_live() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    // An earlier Ctrl+C left a `Cancel` at seq 500 of the parent's log.
    tui.live_events.accept_interrupt(500);
    let reviewer = child_key("euterpe");
    tui.ensure_subagent_monitor(reviewer.clone());

    assert!(
        child_live(&tui, &reviewer).should_render(&advisory(3), 3),
        "the parent's Cancel fenced a child's live output"
    );
}

#[tokio::test]
async fn a_restarted_child_monitor_keeps_the_childs_own_fence() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let reviewer = child_key("euterpe");
    tui.ensure_subagent_monitor(reviewer.clone());
    let first = child_live(&tui, &reviewer);
    fence_live_events_from_history(&first, &deadline_cancelled_history(40));

    // A monitor whose attachment ended is replaced by the next sync.
    tui.subagent_monitor_handles[&reviewer].abort();
    while !tui.subagent_monitor_handles[&reviewer].is_finished() {
        tokio::task::yield_now().await;
    }
    tui.ensure_subagent_monitor(reviewer.clone());

    let second = child_live(&tui, &reviewer);
    assert!(!second.same_attachment(&first));
    assert!(
        !second.should_render(&advisory(39), 0),
        "the new attachment lost the child's own Cancel"
    );
    assert!(second.should_render(&advisory(40), 0));
}

#[tokio::test]
async fn switching_the_root_session_starts_a_fresh_fence() {
    let config = test_config();
    open_root_session(&config, "root-one");
    let mut tui = Tui::init(&config).await.unwrap();
    tui.sync_subagent_monitor_root();
    // root-one's history ends in a `Cancel` at seq 500 of its log.
    tui.live_events.accept_interrupt(500);

    tui.sync_subagent_monitor_root();
    assert!(
        !tui.live_events.should_render(&advisory(499), 0),
        "a sync that keeps the same root must keep its fence"
    );

    open_root_session(&config, "root-two");
    tui.sync_subagent_monitor_root();
    assert!(
        tui.live_events.should_render(&advisory(3), 0),
        "root-one's Cancel fenced root-two's live output"
    );
}
