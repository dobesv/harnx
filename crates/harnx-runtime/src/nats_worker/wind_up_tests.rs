//! Wind-up closes out the round a `Cancel` interrupted: the journal's reply
//! where the call finished, a placeholder (and a resent cancel) where it did
//! not, and nothing at all on a second pass.

use super::backend::{test_session_authority, NatsSessionLogBackend, WoundUpRound};
use super::wind_up::{wind_up_interrupted_turn, WindUpInputs, WindUpOutcome};
use crate::config::session::INTERRUPTED_TOOL_RESPONSE_ERROR;
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
use crate::nats_test_common::spawn_nats_server;
use crate::nats_tool_provider::{InFlightRegistration, NatsInFlightCalls};
use futures_util::StreamExt;
use harnx_core::execution_context::{ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE};
use harnx_core::instance::ServerScope;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::session_reconstruct::{reconstruct_state_from_nats, TurnStatus};
use harnx_core::tool::ToolCall;
use harnx_toolset::{ControlKind, ControlMessage, ToolReply, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::Arc;
use std::time::Duration;

/// The identity the frontend's `Cancel` carries; every cancel the wind-up
/// resends and every placeholder it writes has to quote it back.
const CANCELLATION_ID: &str = "cancel-7";

/// The tool server the journal says took these calls. A cancel for a call this
/// process no longer holds is addressed to this scope's control subject.
fn journal_scope() -> ServerScope {
    ServerScope::from_string("wind-up-instance")
}

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        name: "probe".into(),
        arguments: serde_json::json!({}),
        id: Some(id.into()),
        thought_signature: None,
        reasoning_provenance: None,
    }
}

/// The wire id dispatch would have minted for a transcript call. A uuid in
/// production; anything unrelated to the transcript id will do here.
fn wire_id(call_id: &str) -> String {
    format!("wire-{call_id}")
}

/// The cancel a subscription received, or a failure naming what it waited for.
async fn resent_cancel(control: &mut async_nats::Subscriber) -> ControlMessage {
    let message = tokio::time::timeout(Duration::from_secs(2), control.next())
        .await
        .expect("timed out waiting for the resent cancel")
        .expect("control subscription closed early");
    let resent: ControlMessage = serde_json::from_slice(&message.payload).unwrap();
    assert_eq!(resent.kind, ControlKind::Cancel);
    assert_eq!(resent.cancellation_id, CANCELLATION_ID);
    resent
}

/// Whether the wind-up left this subject alone after the cancel it did send.
async fn nothing_further_sent(control: &mut async_nats::Subscriber) -> bool {
    tokio::time::timeout(Duration::from_millis(250), control.next())
        .await
        .is_err()
}

/// A session whose turn made tool calls and was cancelled before the log
/// carried a result for any of them: everything a wind-up reads, plus the
/// handles a test needs to seed the journal and watch what the wind-up sends.
struct InterruptedTurn {
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    journal: InvocationJournal,
    log: NatsSessionLog,
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
    storage_key: String,
    /// Sequence of the `ToolCalls` entry — the round these calls were made in,
    /// and the only way back from a transcript id to a journal row.
    round: u64,
    cancel_seq: u64,
}

impl InterruptedTurn {
    async fn seed(url: &str, calls: &[&str]) -> Self {
        let client = async_nats::connect(url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client.clone());
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
        metadata
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();
        let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key.clone(), 1);
        log.append_event_async(&user("run them")).await.unwrap();
        let round = log
            .append_event_async(&SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: calls.iter().map(|id| call(id)).collect(),
                timestamp: None,
                fence_token: None,
            })
            .await
            .unwrap();
        let cancel_seq = log
            .append_event_async(&SessionLogEntry::cancel_request(
                CANCELLATION_ID.into(),
                "frontend".into(),
            ))
            .await
            .unwrap();
        Self {
            journal: InvocationJournal::ensure(&jetstream, 1).await.unwrap(),
            lease: test_session_authority(&jetstream, &storage_key, &metadata).await,
            backend: NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1),
            client,
            jetstream,
            log,
            storage_key,
            round,
            cancel_seq,
        }
    }

    /// A request the way dispatch builds one: the wire id is minted per
    /// attempt and is what keys the journal row, while the id the transcript
    /// gave the call rides along inside as `tool_call_id`. The two are never
    /// the same string.
    fn request(&self, call_id: &str) -> ToolRequest {
        ToolRequest {
            replay: None,
            operation_id: format!("op-{}", wire_id(call_id)),
            call_id: wire_id(call_id),
            tool: "probe".into(),
            args: serde_json::json!({}),
            parent_session_id: Some(self.storage_key.clone()),
            tool_call_id: Some(call_id.into()),
            capabilities: Default::default(),
        }
    }

    /// Journal a dispatch to `owner` the way the provider does, into this
    /// turn's own round.
    async fn dispatched(&self, request: &ToolRequest, owner: &str) {
        self.journal
            .record(
                request,
                ("probe", journal_scope().as_str(), owner),
                self.round,
            )
            .await
            .unwrap();
    }

    /// Record the reply a tool wrote to the journal before anyone read it back.
    async fn answered(&self, request: &ToolRequest, output: serde_json::Value) {
        let reply = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(output),
            final_progress: None,
        };
        self.journal.complete(request, reply).await.unwrap();
    }

    /// Seed `completed_undelivered_results_are_persisted_at_wind_up`'s
    /// scenario: `c1` never replies, `c2` replies once, and `c3` is
    /// dispatched twice (a wire id is minted per attempt) with only the
    /// retry's reply in the journal — so the round ends up with two rows for
    /// `c3`, and the retry is the one that answers it.
    async fn seed_dispatch_and_replies(&self) {
        let c3_retry = ToolRequest {
            operation_id: "op-retry-c3".into(),
            call_id: "retry-c3".into(),
            ..self.request("c3")
        };
        self.dispatched(&self.request("c1"), "srv-1").await;
        self.dispatched(&self.request("c2"), "srv-2").await;
        self.dispatched(&self.request("c3"), "srv-3").await;
        self.dispatched(&c3_retry, "srv-3").await;
        self.answered(&self.request("c2"), serde_json::json!({"n": 2}))
            .await;
        self.answered(&c3_retry, serde_json::json!({"n": 3})).await;
    }

    async fn control(&self, scope: &ServerScope) -> async_nats::Subscriber {
        let control = self
            .client
            .subscribe(scope.control_subject())
            .await
            .unwrap();
        self.client.flush().await.unwrap();
        control
    }

    async fn wind_up(&self, in_flight: &NatsInFlightCalls) -> WindUpOutcome {
        wind_up_interrupted_turn(WindUpInputs {
            backend: &self.backend,
            lease: &self.lease,
            client: &self.client,
            jetstream: &self.jetstream,
            replicas: 1,
            in_flight,
            event_sink: None,
        })
        .await
        .unwrap()
    }

    /// A second wind-up after the round is closed must find nothing left to
    /// do: no further append, and the turn reads back as idle.
    async fn assert_repeat_wind_up_is_noop(&self, in_flight: &NatsInFlightCalls) {
        let before = self.log.load_events_latest_async().await.unwrap();
        assert_eq!(
            self.wind_up(in_flight).await,
            WindUpOutcome::Nothing,
            "the appended results already closed the round out"
        );
        let after = self.log.load_events_latest_async().await.unwrap();
        assert_eq!(
            after.len(),
            before.len(),
            "a repeated wind-up appends nothing"
        );
        assert_eq!(
            reconstruct_state_from_nats(&after).turn_status,
            TurnStatus::Idle,
            "the interrupted turn is over once its calls are answered"
        );
    }

    /// The one `ToolResults` entry a wind-up owes the round, and its sequence.
    async fn wound_results(&self) -> (u64, Vec<ToolOutput>) {
        let entries = self.log.load_events_latest_async().await.unwrap();
        let mut appended = entries.iter().filter_map(|(seq, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some((*seq, results.clone())),
            _ => None,
        });
        let wound = appended
            .next()
            .expect("wind-up appends a ToolResults entry");
        assert!(
            appended.next().is_none(),
            "a wind-up appends exactly one ToolResults entry"
        );
        assert!(wound.0 > self.cancel_seq, "the results follow the Cancel");
        wound
    }
}

/// One answered call's transcript output.
fn answer<'a>(results: &'a [ToolOutput], call_id: &str) -> &'a serde_json::Value {
    &results
        .iter()
        .find(|result| result.id.as_deref() == Some(call_id))
        .unwrap_or_else(|| panic!("{call_id} is answered"))
        .output
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_undelivered_results_are_persisted_at_wind_up() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedTurn::seed(server.url(), &["c1", "c2", "c3"]).await;

    // The journal is the only place `c2`'s and `c3`'s results exist; `c1`
    // never replied and is still owned by the server behind the control
    // subject below. `c3` was dispatched twice inside the one round: a wire id
    // is minted per attempt, so the round ends up holding two rows for it.
    turn.seed_dispatch_and_replies().await;
    let mut control = turn.control(&journal_scope()).await;

    let in_flight = NatsInFlightCalls::default();
    let outcome = turn.wind_up(&in_flight).await;
    let WindUpOutcome::Appended {
        seq,
        placeholders,
        real,
    } = outcome
    else {
        panic!("wind-up should have appended results, got {outcome:?}");
    };
    assert_eq!(
        (placeholders, real),
        (1, 2),
        "c1 is a placeholder, c2 and c3 come from the journal"
    );
    assert!(
        seq > turn.cancel_seq,
        "the results close out the cancelled turn"
    );

    let (results_seq, results) = turn.wound_results().await;
    assert_eq!(results_seq, seq);
    assert_eq!(results.len(), 3, "every interrupted call is answered");
    assert_eq!(
        answer(&results, "c1")["error"],
        INTERRUPTED_TOOL_RESPONSE_ERROR
    );
    assert_eq!(answer(&results, "c1")["cancellation_id"], CANCELLATION_ID);
    assert_eq!(
        answer(&results, "c2"),
        &serde_json::json!({"n": 2}),
        "the journal's reply is the answer, not a placeholder"
    );
    assert_eq!(
        answer(&results, "c3"),
        &serde_json::json!({"n": 3}),
        "of a retried call's two rows, the attempt that replied answers it"
    );

    // Only the call without a reply is cancelled again, addressed to the
    // server the journal says recorded it.
    let resent = resent_cancel(&mut control).await;
    assert_eq!(
        resent.call_id,
        wire_id("c1"),
        "a tool server knows the call by the wire id it journaled, not by the transcript's"
    );
    assert_eq!(resent.server, "srv-1");
    assert!(
        nothing_further_sent(&mut control).await,
        "a call the journal already answered must not be cancelled"
    );

    // Winding up again finds nothing left owed and writes nothing.
    turn.assert_repeat_wind_up_is_noop(&in_flight).await;

    turn.lease.release().await.unwrap();
}

/// A journaled value is the tool's raw handler output: the server writes it
/// before the reply envelope is finalized, so the execution context inside its
/// `_meta` is the tool server's own account of where it ran. Recovering that
/// reply has to decode it the way a replay does — otherwise the private block
/// lands in the transcript with nothing vouching for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovered_replys_private_execution_context_is_stripped() {
    const PRIVATE_PATH: &str = "/tool-server/private/workspace";
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedTurn::seed(server.url(), &["ctx"]).await;

    let mut observation = ExecutionContextObservation::observe(
        std::path::Path::new(PRIVATE_PATH),
        std::path::Path::new(PRIVATE_PATH),
    );
    observation.workspace_root = PRIVATE_PATH.to_string();
    observation.working_directory = PRIVATE_PATH.to_string();
    turn.dispatched(&turn.request("ctx"), "srv-ctx").await;
    turn.answered(
        &turn.request("ctx"),
        serde_json::json!({
            "ok": true,
            "_meta": { EXECUTION_CONTEXT_NAMESPACE: observation, "public": true },
        }),
    )
    .await;

    let outcome = turn.wind_up(&NatsInFlightCalls::default()).await;
    assert!(
        matches!(outcome, WindUpOutcome::Appended { real: 1, .. }),
        "the journal's reply answers the call, got {outcome:?}"
    );
    let (_, results) = turn.wound_results().await;
    assert_eq!(
        answer(&results, "ctx"),
        &serde_json::json!({"ok": true, "_meta": {"public": true}}),
        "the private execution context is taken out and the rest of _meta kept"
    );
    assert!(
        !results[0].output.to_string().contains(PRIVATE_PATH),
        "nothing of the tool server's own paths reaches the transcript"
    );

    turn.lease.release().await.unwrap();
}

/// The process winding up may still be the one holding the call. Its own
/// registration knows where the call was actually dispatched, which is the
/// live control subject rather than the scope the journal row recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_this_process_still_holds_is_cancelled_where_it_was_dispatched() {
    let live_scope = ServerScope::from_string("wind-up-live-instance");
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedTurn::seed(server.url(), &["held"]).await;
    turn.dispatched(&turn.request("held"), "srv-journaled")
        .await;

    let in_flight = NatsInFlightCalls::default();
    let _live = in_flight
        .register(InFlightRegistration {
            call_id: wire_id("held"),
            server: "srv-live".into(),
            session_id: turn.storage_key.clone(),
            control_subject: live_scope.control_subject(),
        })
        .await;
    let mut journaled = turn.control(&journal_scope()).await;
    let mut live = turn.control(&live_scope).await;

    turn.wind_up(&in_flight).await;

    let resent = resent_cancel(&mut live).await;
    assert_eq!(
        resent.call_id,
        wire_id("held"),
        "the registry is keyed by the same wire id the journal row carries"
    );
    assert_eq!(
        resent.server, "srv-live",
        "a live registration names the server the call is actually running on"
    );
    assert!(
        nothing_further_sent(&mut journaled).await,
        "the journal row's scope is the fallback for a call this process has let go"
    );

    turn.lease.release().await.unwrap();
}

/// A call retried inside one round leaves a row per attempt. With none of them
/// answered there is no result to recover, and the cancel goes to the newest —
/// the attempt that is still expected to reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_newest_unanswered_attempt_is_the_one_cancelled() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedTurn::seed(server.url(), &["twice"]).await;
    let newest = ToolRequest {
        operation_id: "op-second-twice".into(),
        call_id: "second-twice".into(),
        ..turn.request("twice")
    };
    turn.dispatched(&turn.request("twice"), "srv-first").await;
    // `started_at_ms` is a millisecond, so the two attempts have to be started
    // far enough apart for "newest" to mean anything at all.
    tokio::time::sleep(Duration::from_millis(5)).await;
    turn.dispatched(&newest, "srv-second").await;
    let mut control = turn.control(&journal_scope()).await;

    let outcome = turn.wind_up(&NatsInFlightCalls::default()).await;
    assert!(
        matches!(
            outcome,
            WindUpOutcome::Appended {
                placeholders: 1,
                ..
            }
        ),
        "neither attempt replied, so the call gets a placeholder, got {outcome:?}"
    );
    let (_, results) = turn.wound_results().await;
    assert_eq!(
        answer(&results, "twice")["error"],
        INTERRUPTED_TOOL_RESPONSE_ERROR
    );

    let resent = resent_cancel(&mut control).await;
    assert_eq!(
        resent.call_id, "second-twice",
        "the newest attempt is the one still expected to reply"
    );
    assert_eq!(resent.server, "srv-second");
    assert!(
        nothing_further_sent(&mut control).await,
        "an older attempt is left to the cancel its own dispatcher sends"
    );

    turn.lease.release().await.unwrap();
}

/// An empty session this worker holds the lease on: everything a fenced
/// append needs and nothing a wind-up does.
struct LeasedSession {
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
    log: NatsSessionLog,
}

impl LeasedSession {
    async fn open(url: &str) -> Self {
        let client = async_nats::connect(url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
        metadata
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();
        Self {
            lease: test_session_authority(&jetstream, &storage_key, &metadata).await,
            backend: NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1),
            log: NatsSessionLog::new_with_replicas(jetstream, storage_key, 1),
        }
    }
}

/// A turn that was interrupted between the tail its worker last saw and its
/// next append: the `Cancel` that ended it, and the wind-up entry the round
/// is now owed.
struct InterruptedAppend {
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
    log: NatsSessionLog,
    /// The tail this worker last saw, from before the `Cancel` moved it.
    tail: u64,
    cancel_seq: u64,
}

impl InterruptedAppend {
    async fn open(url: &str) -> Self {
        let LeasedSession {
            backend,
            lease,
            log,
        } = LeasedSession::open(url).await;
        let tail = log.append_event_async(&user("go")).await.unwrap();
        // Someone else terminates the turn between the read and the append.
        let cancel_seq = log
            .append_event_async(&SessionLogEntry::cancel_request(
                CANCELLATION_ID.into(),
                "frontend".into(),
            ))
            .await
            .unwrap();
        Self {
            backend,
            lease,
            log,
            tail,
            cancel_seq,
        }
    }

    /// The `ToolResults` one worker's wind-up would write for this round.
    /// `age` separates two workers' entries the way their own timestamps do.
    fn wind_up_entry(&self, age: chrono::TimeDelta) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: crate::config::session::interrupted_tool_outputs(
                &[call("c1")],
                Some(CANCELLATION_ID),
            ),
            timestamp: Some(chrono::Utc::now() + age),
        }
    }

    /// Append a wind-up from the stale tail this worker still expects.
    async fn wind_up_append(&self, entry: &SessionLogEntry) -> u64 {
        self.backend
            .append_wind_up_fenced_with_lease(
                entry,
                &self.lease,
                WoundUpRound {
                    cancel_seq: self.cancel_seq,
                    expected_tail: self.tail,
                },
            )
            .await
            .unwrap()
    }
}

/// The two writer rules the fenced append offers differ in exactly one way:
/// what a `Cancel` that beat the writer to the tail means. It ends the turn,
/// so the turn's own writer gives up; the wind-up owes the interrupted calls
/// a result either way, so it appends behind the Cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_writer_rules_part_ways_over_a_newer_cancel() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedAppend::open(server.url()).await;

    let error = turn
        .backend
        .append_event_fenced_with_lease(&user("worker output"), &turn.lease, turn.tail)
        .await
        .expect_err("the turn's writer must not append behind a Cancel");
    let interrupted = error
        .downcast_ref::<super::backend::TurnInterrupted>()
        .expect("callers recognise the interruption by downcast");
    assert_eq!(interrupted.cancel_seq, turn.cancel_seq);

    let seq = turn
        .wind_up_append(&turn.wind_up_entry(chrono::TimeDelta::zero()))
        .await;
    assert!(seq > turn.cancel_seq);

    turn.lease.release().await.unwrap();
}

/// A replacement worker closes the same round out from the same stale tail,
/// after the first wind-up is already durable. Its entry is not the same
/// bytes — it stamps its own timestamp, and a reply that reached the journal
/// in between would have turned a placeholder into a real result — but the
/// round is answered, so the log must not gain a second answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_wind_up_of_the_same_round_adopts_the_first_answer() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let turn = InterruptedAppend::open(server.url()).await;
    let first = turn.wind_up_entry(chrono::TimeDelta::zero());
    let replacement = turn.wind_up_entry(chrono::TimeDelta::seconds(1));
    assert_ne!(
        first, replacement,
        "two workers never produce the same wind-up entry"
    );

    let seq = turn.wind_up_append(&first).await;
    let repeat = turn.wind_up_append(&replacement).await;

    assert_eq!(
        repeat, seq,
        "the round was already answered; the second wind-up adopts that sequence"
    );
    assert_eq!(
        turn.log.load_events_latest_async().await.unwrap().len(),
        3,
        "the interrupted round is answered exactly once"
    );

    turn.lease.release().await.unwrap();
}

/// The lease is the authority for every round of a fenced append, not only
/// for the first. Each round decides afresh against a tail it has just
/// re-read, and a worker fenced out while it was doing that must not land its
/// entry under a token it no longer holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_lost_between_retry_rounds_stops_the_append() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let LeasedSession {
        backend,
        lease,
        log,
    } = LeasedSession::open(server.url()).await;
    let tail = log.append_event_async(&user("go")).await.unwrap();
    // A message typed behind the running turn moves the tail, so the first
    // round loses its race and there has to be a second one.
    log.append_event_async(&user("and this too")).await.unwrap();

    let entry = user("worker output");
    let append = backend.append_event_fenced_with_lease(&entry, &lease, tail);
    tokio::pin!(append);
    // Drive the append to its first publish — past the check the append used
    // to do only on the way in — and only then take the lease away, so the
    // round that follows the conflict is the one that has to notice.
    let mut finished = None;
    std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(result) = std::future::Future::poll(append.as_mut(), cx) {
            finished = Some(result);
        }
        std::task::Poll::Ready(())
    })
    .await;
    assert!(
        finished.is_none(),
        "the append has to still be in flight for the lease to be lost under it"
    );
    lease.release().await.unwrap();

    let error = append
        .await
        .expect_err("a fenced-out worker must not append");
    assert!(
        format!("{error:#}").contains("session lease not held"),
        "expected the lease rejection, got {error:#}"
    );
    assert_eq!(
        log.load_events_latest_async().await.unwrap().len(),
        2,
        "the entry must not have landed"
    );
}
