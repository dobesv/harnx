use super::*;
use crate::nats_session::fixed_admission::FixedAdmissionTicket;
use crate::nats_session_metadata::{
    CallTimeoutOverride, InvocationEdgeKind, JoinedDispatch, RunLimitsRecord, SessionBusy,
};
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use crate::nats_worker::SessionActivate;
use serde_json::json;

mod round2;
mod round3;
mod round4;
mod round5;

struct Fixture {
    _server: NatsServerHandle,
    js: async_nats::jetstream::Context,
    parent: RunLimitsRecord,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = spawn_nats_server()
            .await?
            .context("nats-server required for dispatch tests")?;
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let parent = RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            chrono::Utc::now(),
            Default::default(),
            None,
            CallTimeoutOverride::Omitted,
        )?;
        Ok(Self {
            _server: server,
            js,
            parent,
        })
    }

    /// Freeze the active invocation's limits the way a running worker does.
    async fn freeze_limits(
        &self,
        session: &NatsSession,
        original: &AppendedPrompt,
    ) -> Result<RunLimitsRecord> {
        let frozen = RunLimitsRecord::admit_child(
            &self.parent,
            crate::nats_session_metadata::InvocationIdentity::from_string(
                original.execution_id().unwrap().into(),
            ),
            InvocationEdgeKind::Delegation,
            self.parent.admitted_at,
            Default::default(),
            None,
            CallTimeoutOverride::Omitted,
        )?;
        session
            .metadata_store
            .put_invocation_limits(session.storage_key(), &frozen)
            .await?;
        Ok(frozen)
    }

    async fn assert_no_activation(&self) {
        assert!(self.js.get_stream("WORK_NOTIFY_dispatch").await.is_err());
    }

    async fn session(&self, id: &str, invocation: &str) -> Result<NatsSession> {
        Ok(NatsSession::new(
            super::super::NatsSessionConfig {
                cluster: "dispatch".into(),
                initializer: crate::SessionInitializer::named("helper", Default::default()),
                session_id: Some(id.into()),
                activation_route: crate::SessionActivationRoute::ClusterShared,
            },
            self.js.client().clone(),
            self.js.clone(),
            crate::utils::create_abort_signal(),
        )
        .await?
        .with_inherited_admission(
            self.parent.clone(),
            invocation.into(),
            self.parent.admitted_at,
            InvocationEdgeKind::Delegation,
            None,
        )
        .with_execution_parent("trusted-parent".into(), invocation.into()))
    }
}

fn text(value: &str) -> MessageContent {
    MessageContent::Text(value.into())
}

#[tokio::test]
async fn admission_binding_and_append_failures_replay_original_intent() -> Result<()> {
    let f = Fixture::new().await?;
    for (index, boundary) in [
        DispatchFault::AfterAdmission,
        DispatchFault::AfterBinding,
        DispatchFault::BeforeAppend,
    ]
    .into_iter()
    .enumerate()
    {
        let session = f.session(&format!("boundary-{index}"), "original").await?;
        let error = session
            .append_dispatch_content_inner(text("original content"), boundary.into())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected dispatch failure"));
        assert!(session.load_durable_entries().await?.is_empty());
        let saved = session
            .metadata_store
            .admission(session.storage_key(), "original")
            .await?
            .unwrap();
        let replay = session.clone().with_inherited_admission(
            f.parent.clone(),
            "original".into(),
            chrono::Utc::now() + chrono::Duration::hours(1),
            InvocationEdgeKind::Delegation,
            Some(9999),
        );
        let prompt = replay
            .append_dispatch_content(text("changed retry content"))
            .await?;
        assert_eq!(
            (prompt.user_msg_id(), prompt.execution_id()),
            ("original", Some("original"))
        );
        assert_eq!(session.load_durable_entries().await?.len(), 1);
        assert_eq!(
            session
                .metadata_store
                .prompt_admission(session.storage_key(), "original")
                .await?,
            Some(saved)
        );
        let competing = f.session(session.session_id(), "independent").await?;
        // A competing call to the busy session joins the active turn instead
        // of failing with "session busy" (issue #2672).
        let joined = competing
            .append_dispatch_content(text("competing steering"))
            .await?;
        assert_eq!(
            (joined.user_msg_id(), joined.execution_id()),
            ("independent", Some("original"))
        );
        assert_eq!(session.load_durable_entries().await?.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn lost_append_ack_recovers_durable_prompt_and_binding() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("append-ack", "original").await?;
    let original = session
        .append_dispatch_content_inner(text("content"), DispatchFault::AppendAckLost.into())
        .await?;
    let replay = session.append_dispatch_content(text("retry")).await?;
    assert_eq!(original, replay);
    assert_eq!(
        (original.user_msg_seq(), original.execution_id()),
        (replay.user_msg_seq(), Some("original"))
    );
    assert_eq!(session.load_durable_entries().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn activation_failure_and_ack_loss_republish_exact_sequence_idempotently() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("activation", "original").await?;
    let prompt = session.append_dispatch_content(text("content")).await?;
    assert!(session
        .activate_dispatch_prompt_inner(&prompt, DispatchFault::BeforeActivation.into())
        .await
        .is_err());
    f.assert_no_activation().await;
    assert!(session
        .activate_dispatch_prompt_inner(&prompt, DispatchFault::ActivationAckLost.into())
        .await
        .is_err());
    let mut stream = f.js.get_stream("WORK_NOTIFY_dispatch").await?;
    assert_eq!(stream.info().await?.state.messages, 1);
    let before = stream.get_raw_message(1).await?;
    let activation: SessionActivate = serde_json::from_slice(&before.payload)?;
    assert_activation_target(&activation, &session, &prompt);
    assert_eq!(
        session.activate_dispatch_prompt(&prompt).await?,
        PromptActivation::Published
    );
    assert_eq!(
        stream.info().await?.state.messages,
        1,
        "exact activation must deduplicate at broker"
    );
    assert_eq!(session.load_durable_entries().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn replay_does_not_retarget_steering_or_later_active_invocation() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("historical", "original").await?;
    let original = session.append_dispatch_content(text("original")).await?;
    let log = NatsSessionLog::new_with_replicas(f.js.clone(), session.storage_key(), 1);
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "steering", "original")
        .await?;
    let steering = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("steering".into()),
            role: MessageRole::User,
            content: text("steering"),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    assert_eq!(
        session
            .metadata_store
            .invocation_prompt_seq(
                session.storage_key(),
                "original",
                &session.load_durable_entries().await?
            )
            .await?,
        Some(steering)
    );
    let replay = session.append_dispatch_content(text("retry")).await?;
    assert_eq!(replay.user_msg_seq(), original.user_msg_seq());
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: steering,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await?;
    let next = f.session("historical", "later").await?;
    next.append_dispatch_content(text("later independent work"))
        .await?;
    let before = session.load_durable_entries().await?;
    let replay = session
        .append_dispatch_content(text("changed again"))
        .await?;
    assert_eq!(replay.user_msg_seq(), original.user_msg_seq());
    assert_eq!(
        session.activate_dispatch_prompt(&replay).await?,
        PromptActivation::NotNeeded
    );
    assert_eq!(before, session.load_durable_entries().await?);
    f.assert_no_activation().await;
    Ok(())
}

#[tokio::test]
async fn expired_admission_does_not_append_or_activate_and_terminal_replay_stays_available(
) -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("expiry", "original").await?;
    let prompt = session.append_dispatch_content(text("original")).await?;
    let frozen = RunLimitsRecord::admit_child(
        &f.parent,
        session
            .metadata_store
            .admission(session.storage_key(), "original")
            .await?
            .unwrap()
            .invocation_id,
        InvocationEdgeKind::Delegation,
        chrono::Utc::now() - chrono::Duration::hours(2),
        Default::default(),
        None,
        CallTimeoutOverride::from_optional(Some(1)),
    )?;
    session
        .metadata_store
        .put_invocation_limits(session.storage_key(), &frozen)
        .await?;
    assert!(session
        .activate_dispatch_prompt(&prompt)
        .await
        .unwrap_err()
        .to_string()
        .contains("deadline expired"));
    let replay = session.append_dispatch_content(text("retry")).await?;
    assert_eq!(replay, prompt);
    let log = NatsSessionLog::new_with_replicas(f.js.clone(), session.storage_key(), 1);
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: prompt.user_msg_seq(),
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await?;
    assert_eq!(
        session.activate_dispatch_prompt(&replay).await?,
        PromptActivation::NotNeeded
    );
    let expired = f
        .session("never-appended", "expired")
        .await?
        .with_inherited_admission(
            f.parent.clone(),
            "expired".into(),
            chrono::Utc::now() - chrono::Duration::hours(2),
            InvocationEdgeKind::Delegation,
            Some(1),
        );
    assert!(expired
        .append_dispatch_content(text("too late"))
        .await
        .unwrap_err()
        .to_string()
        .contains("deadline expired"));
    assert!(expired.load_durable_entries().await?.is_empty());
    f.assert_no_activation().await;
    Ok(())
}

#[tokio::test]
async fn approval_visibility_is_not_an_activation_or_terminal_transition() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("approval", "original").await?;
    let prompt = session.append_dispatch_content(text("original")).await?;
    let log = NatsSessionLog::new_with_replicas(f.js.clone(), session.storage_key(), 1);
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "pending approval".into(),
        thought: None,
        calls: vec![harnx_core::tool::ToolCall::new(
            "read".into(),
            json!({}),
            Some("call".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "call".into(),
        summary: "approve".into(),
        fence_token: 1,
    })
    .await?;
    let before = session.load_durable_entries().await?;
    assert_eq!(
        session.activate_dispatch_prompt(&prompt).await?,
        PromptActivation::NotNeeded
    );
    assert_eq!(before, session.load_durable_entries().await?);
    assert!(super::super::invocation_terminal_seq(&before, prompt.user_msg_seq()).is_none());
    f.assert_no_activation().await;
    Ok(())
}

#[tokio::test]
async fn mismatched_parent_and_prompt_binding_fail_closed() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("scope", "original").await?;
    let prompt = session.append_dispatch_content(text("original")).await?;
    let wrong_parent = session
        .clone()
        .with_execution_parent("different-parent".into(), "original".into());
    assert!(wrong_parent
        .append_dispatch_content(text("original"))
        .await
        .is_err());
    assert!(wrong_parent
        .activate_dispatch_prompt(&prompt)
        .await
        .is_err());
    let wrong_seq = AppendedPrompt {
        user_msg_seq: prompt.user_msg_seq() + 1,
        ..prompt.clone()
    };
    assert!(session.activate_dispatch_prompt(&wrong_seq).await.is_err());
    let wrong_execution = AppendedPrompt {
        execution_id: Some("other".into()),
        ..prompt.clone()
    };
    assert!(session
        .activate_dispatch_prompt(&wrong_execution)
        .await
        .is_err());
    let conflict = f.session("binding-conflict", "original").await?;
    conflict
        .metadata_store
        .bind_prompt_admission(conflict.storage_key(), "original", "unrelated")
        .await?;
    assert!(conflict
        .append_dispatch_content(text("original"))
        .await
        .unwrap_err()
        .to_string()
        .contains("binding conflict"));
    assert!(conflict.load_durable_entries().await?.is_empty());
    assert_eq!(session.load_durable_entries().await?.len(), 1);
    Ok(())
}

fn assert_activation_target(
    activation: &SessionActivate,
    session: &NatsSession,
    prompt: &AppendedPrompt,
) {
    assert_eq!(
        (activation.requested_seq, activation.session_id.as_str()),
        (Some(prompt.user_msg_seq()), session.storage_key()),
    );
}

// Issue #2672: an inherited caller joins a busy session's active turn instead
// of failing with "session busy". The joined-dispatch record (create-only in
// session metadata KV) owns the caller's identity and original content.

/// A distinct root parent, so a joining caller's identity differs from the
/// fixture starter's even when both act as trusted parents of the session.
async fn foreign_parent() -> Result<RunLimitsRecord> {
    Ok(RunLimitsRecord::admit_root(
        Default::default(),
        Default::default(),
        chrono::Utc::now(),
        Default::default(),
        None,
        CallTimeoutOverride::Omitted,
    )?)
}

/// A second caller of the same session: different parent run and storage
/// scope, dispatching under its own call id.
async fn joining_caller(
    f: &Fixture,
    session_id: &str,
    call: &str,
    parent: &RunLimitsRecord,
) -> Result<NatsSession> {
    Ok(f.session(session_id, call)
        .await?
        .with_inherited_admission(
            parent.clone(),
            call.into(),
            parent.admitted_at,
            InvocationEdgeKind::Delegation,
            None,
        )
        .with_execution_parent("joiner-parent".into(), call.into()))
}

fn user_messages<'a>(entries: &'a [(u64, SessionLogEntry)], id: &str) -> Vec<&'a MessageContent> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(found),
                role,
                content,
                ..
            } if found == id && role.is_user() => Some(content),
            _ => None,
        })
        .collect()
}

async fn assert_unchanged_joined_limits(
    session: &NatsSession,
    frozen: &RunLimitsRecord,
    joined: &AppendedPrompt,
) -> Result<()> {
    // No limits record is minted or renewed for the joined message (D1).
    assert_eq!(
        session
            .metadata_store
            .get_invocation_limits(session.storage_key(), frozen.invocation_id.as_str())
            .await?,
        Some(frozen.clone())
    );
    assert_eq!(
        session
            .metadata_store
            .get_invocation_limits(session.storage_key(), joined.user_msg_id())
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn busy_send_joins_active_invocation_once_with_unchanged_limits() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("join", "original").await?;
    let original = session
        .append_dispatch_content(text("original work"))
        .await?;
    let frozen = f.freeze_limits(&session, &original).await?;
    let parent = foreign_parent().await?;
    let joiner = joining_caller(&f, "join", "join-call", &parent).await?;
    let joined = joiner
        .append_dispatch_content(text("joined steering"))
        .await?;
    assert_eq!(
        (joined.user_msg_id(), joined.execution_id()),
        ("join-call", Some(original.execution_id().unwrap()))
    );
    assert_eq!(original.execution_id(), Some("original"));
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "join-call"),
        vec![&text("joined steering")]
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "join-call")
        .await?
        .context("joined prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "original");
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "join-call")
        .await?
        .context("joined dispatch record missing")?;
    assert_eq!(record.parent_storage_key.as_deref(), Some("joiner-parent"));
    assert_eq!(
        record.parent_invocation_id.as_deref(),
        Some(parent.invocation_id.as_str())
    );
    assert_eq!(record.invocation_id, "original");
    assert_eq!(record.prompt_content, text("joined steering"));
    assert_unchanged_joined_limits(&session, &frozen, &joined).await?;
    // Retry of the same call returns the same handle and appends nothing.
    let replay = joiner
        .append_dispatch_content(text("changed retry"))
        .await?;
    assert_eq!(replay, joined);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    // The joining caller can activate its prompt under the joined identity;
    // the invocation starter cannot.
    assert_eq!(
        joiner.activate_dispatch_prompt(&joined).await?,
        PromptActivation::Published
    );
    let activation = {
        let stream = f.js.get_stream("WORK_NOTIFY_dispatch").await?;
        let raw = stream.get_raw_message(1).await?;
        serde_json::from_slice::<SessionActivate>(&raw.payload)?
    };
    assert_eq!(activation.execution_id.as_deref(), Some("original"));
    assert_eq!(activation.requested_seq, Some(joined.user_msg_seq()));
    let error = session.activate_dispatch_prompt(&joined).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("dispatch parent storage scope mismatch"));
    Ok(())
}

#[tokio::test]
async fn join_fault_injection_replays_recorded_join_without_second_append() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("join-fault", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    for (index, boundary) in [
        DispatchFault::AfterJoinedRecord,
        DispatchFault::AfterJoinedBinding,
        // F8: the joined path's own pre-append boundary must replay the join
        // without a second append, like the reserved path's.
        DispatchFault::BeforeAppend,
    ]
    .into_iter()
    .enumerate()
    {
        let call = format!("join-call-{index}");
        let before = session.load_durable_entries().await?.len();
        let joiner = joining_caller(&f, "join-fault", &call, &parent).await?;
        let error = joiner
            .append_dispatch_content_inner(text("joined content"), boundary.into())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected dispatch failure"));
        assert_eq!(
            session.load_durable_entries().await?.len(),
            before,
            "no joined append before the fault boundary"
        );
        // Retry completes the join to the same invocation with the recorded
        // content, not the retry's changed message.
        let joined = joiner
            .append_dispatch_content(text("changed retry"))
            .await?;
        assert_eq!(
            (joined.user_msg_id(), joined.execution_id()),
            (call.as_str(), Some("original"))
        );
        let entries = session.load_durable_entries().await?;
        assert_eq!(
            user_messages(&entries, &call),
            vec![&text("joined content")]
        );
        let replay = joiner.append_dispatch_content(text("again")).await?;
        assert_eq!(replay, joined);
        assert_eq!(session.load_durable_entries().await?.len(), before + 1);
    }
    Ok(())
}

#[tokio::test]
async fn leftover_busy_reserve_intent_loses_to_a_successful_join() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("leftover", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "leftover", "candidate", &parent).await?;
    // Reproduce the busy-reserve fault: reserve_admission writes its candidate
    // intent before the head check refuses the active turn.
    let entries = session.load_durable_entries().await?;
    let authority = crate::nats_session_metadata::AdmissionAuthority::Inherited {
        parent: parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: parent.admitted_at,
    };
    let mut intent = InvocationAdmission::new(&authority, "candidate".into(), None, None);
    intent.parent_storage_key = Some("joiner-parent".into());
    intent.prompt_content = Some(text("candidate content"));
    let error = session
        .metadata_store
        .reserve_admission(session.storage_key(), &intent, &entries)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("session busy"));
    assert!(
        session
            .metadata_store
            .admission(session.storage_key(), "candidate")
            .await?
            .is_some(),
        "candidate intent left behind by the failed reserve"
    );
    // The join wins over the leftover candidate intent, and the replay stays
    // the join even though the intent is still stored.
    let joined = caller
        .append_dispatch_content(text("joined content"))
        .await?;
    assert_eq!(
        (joined.user_msg_id(), joined.execution_id()),
        ("candidate", Some("original"))
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "candidate")
        .await?
        .context("joined prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "original");
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "candidate")
        .await?
        .context("joined dispatch record missing")?;
    assert_eq!(record.prompt_content, text("joined content"));
    let replay = caller.append_dispatch_content(text("changed")).await?;
    assert_eq!(replay, joined);
    assert_eq!(session.load_durable_entries().await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn mismatched_caller_cannot_replay_a_foreign_joined_dispatch() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("foreign", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let joiner = joining_caller(&f, "foreign", "join-call", &foreign_parent().await?).await?;
    let joined = joiner
        .append_dispatch_content(text("joined content"))
        .await?;
    // A different caller claiming the same call id is refused at append and
    // activation alike.
    let other = joining_caller(&f, "foreign", "join-call", &foreign_parent().await?).await?;
    let error = other
        .append_dispatch_content(text("imposter"))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("dispatch admission parent scope mismatch"));
    assert!(other.activate_dispatch_prompt(&joined).await.is_err());
    assert_eq!(session.load_durable_entries().await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn inherited_steering_joins_active_turn_and_plain_inherited_is_refused() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steering", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    // Only sessions that opted into steering join a busy turn.
    let steering = f
        .session("steering", "steer-call")
        .await?
        .with_steering_admission();
    let enqueued = steering.enqueue_text("steering content").await?;
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "steer-call")
        .await?
        .context("steered prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "original");
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "steer-call"),
        vec![&text("steering content")]
    );
    assert_eq!(enqueued.user_msg_seq(), 2);
    let refused = f.session("steering", "refuse-call").await?;
    let error = match refused.enqueue_text("no steering").await {
        Ok(_) => panic!("plain inherited caller joined a busy session"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("session busy"));
    Ok(())
}

// Issue #2672 review round 1: the join race, the fixed gate, and the steering
// join path (F1/F2/F3/F8/F9). New helpers keep the races deterministic.

/// End the invocation's turn in the log the way a worker's `TurnEnd` would.
async fn end_invocation_turn(f: &Fixture, session: &NatsSession, invocation: &str) -> Result<()> {
    let entries = session.load_durable_entries().await?;
    let seq = session
        .metadata_store
        .invocation_prompt_seq(session.storage_key(), invocation, &entries)
        .await?
        .context("invocation has no prompt row to end")?;
    NatsSessionLog::new_with_replicas(f.js.clone(), session.storage_key(), 1)
        .append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: seq,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;
    Ok(())
}

/// A fixed-admission head the way a coordinated frontend would leave one: the
/// ticket's outcome is still `Pending`, so the head is active and non-terminal.
async fn fixed_admission_head(session: &NatsSession, f: &Fixture) -> Result<()> {
    let ticket = FixedAdmissionTicket::from_parts(
        session.storage_key().to_owned(),
        "fixed-invocation".into(),
        "fixed-prompt".into(),
        "fixed-closure".into(),
        0,
    )?;
    let authority = AdmissionAuthority::Inherited {
        parent: f.parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: f.parent.admitted_at,
    };
    let mut intent = InvocationAdmission::new(&authority, "fixed-invocation".into(), None, None);
    intent.parent_storage_key = Some("trusted-parent".into());
    intent.prompt_content = Some(text("fixed work"));
    intent.fixed_ticket = Some(ticket);
    let entries = session.load_durable_entries().await?;
    session
        .metadata_store
        .reserve_admission(session.storage_key(), &intent, &entries)
        .await?;
    assert_eq!(
        session
            .metadata_store
            .admission_head(session.storage_key())
            .await?,
        Some("fixed-invocation".to_owned())
    );
    Ok(())
}

/// The joined record of a join whose message never landed.
fn void_join_record(parent: &RunLimitsRecord, content: &str, invocation: &str) -> JoinedDispatch {
    JoinedDispatch {
        parent_storage_key: Some("joiner-parent".into()),
        parent_invocation_id: Some(parent.invocation_id.as_str().to_owned()),
        prompt_content: text(content),
        invocation_id: invocation.into(),
    }
}

#[tokio::test]
async fn joined_turn_end_race_redelivers_and_keeps_call_id_usable() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("turn-end-race", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let joiner = joining_caller(&f, "turn-end-race", "join-call", &parent).await?;
    // The joined turn ends between the join decision and the append: the join
    // record and binding are already written, the append is refused (F1).
    let delivered = joiner
        .append_dispatch_content_inner(
            text("joined steering"),
            DispatchFault::JoinedTurnEnds.into(),
        )
        .await
        .context("the same call id must redeliver after the turn-end race")?;
    // The call id stayed usable: the recorded content was redelivered under a
    // fresh reservation of the same prompt id, not refused forever.
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("join-call", Some("join-call"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "join-call"),
        vec![&text("joined steering")],
        "the recorded content wins over the retry's text"
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "join-call")
        .await?
        .context("redelivered prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "join-call");
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "join-call")
        .await?
        .context("retargeted join record missing")?;
    assert_eq!(record.invocation_id, "join-call");
    assert_eq!(record.prompt_content, text("joined steering"));
    // A retry of the same call replays the redelivered prompt identically.
    let replay = joiner
        .append_dispatch_content(text("changed retry"))
        .await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn void_join_record_replay_redelivers_as_fresh_invocation() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("void-replay", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "void-replay", "burned-call", &parent).await?;
    // Seed the journal-replay shape directly: the join record and binding were
    // written, the crash lost the append, and the joined turn has since ended.
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "burned-call",
            &void_join_record(&parent, "joined content", "original"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "burned-call", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    // Before the fix this retry failed forever with "late steering".
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("replay of a void join must redeliver, not refuse")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("burned-call", Some("burned-call"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "burned-call"),
        vec![&text("joined content")],
        "the join record's content is authoritative on redelivery"
    );
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "burned-call")
        .await?
        .context("retargeted join record missing")?;
    assert_eq!(record.invocation_id, "burned-call");
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "burned-call")
        .await?
        .context("redelivered prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "burned-call");
    let replay = caller.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn fixed_admission_head_is_never_joined() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("fixed-head", "original").await?;
    fixed_admission_head(&session, &f).await?;
    let parent = foreign_parent().await?;
    let joiner = joining_caller(&f, "fixed-head", "join-call", &parent).await?;
    let error = joiner
        .append_dispatch_content(text("join attempt"))
        .await
        .expect_err("a fixed-admission head must refuse the join");
    // Exactly the pre-feature refusal shape (F2): the typed busy marker, the
    // ordinary reserve error, and no join record or binding written.
    assert_eq!(
        error.downcast_ref::<SessionBusy>(),
        Some(&SessionBusy::ActiveInvocation {
            invocation_id: "fixed-invocation".into()
        })
    );
    assert!(session
        .metadata_store
        .joined_dispatch(session.storage_key(), "join-call")
        .await?
        .is_none());
    assert!(session
        .metadata_store
        .prompt_admission(session.storage_key(), "join-call")
        .await?
        .is_none());
    assert!(session.load_durable_entries().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn joined_append_ack_loss_returns_committed_prompt() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("join-ack", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let joiner = joining_caller(&f, "join-ack", "join-call", &parent).await?;
    // A lost PubAck resolves the committed append instead of surfacing an
    // error: the joined call returns success, one message, recorded content.
    let committed = joiner
        .append_dispatch_content_inner(text("joined content"), DispatchFault::AppendAckLost.into())
        .await
        .context("joined append ack loss must resolve the committed append")?;
    assert_eq!(
        (committed.user_msg_id(), committed.execution_id()),
        ("join-call", Some("original"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "join-call"),
        vec![&text("joined content")]
    );
    let replay = joiner.append_dispatch_content(text("retry")).await?;
    assert_eq!(replay, committed);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn busy_reserve_race_retries_into_a_join() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("busy-race", "race-call").await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "busy-race", "race-call", &parent).await?;
    // A competing caller wins the admission head between this call's active
    // check and its own reservation, inside one `session_send`: the bounded
    // retry rounds must end in a join of the competing turn, not a refusal.
    let joined = caller
        .append_dispatch_content_inner(
            text("raced content"),
            DispatchFault::CompetingReservation.into(),
        )
        .await
        .context("the busy race must retry into a join")?;
    assert_eq!(
        (joined.user_msg_id(), joined.execution_id()),
        ("race-call", Some("competing"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "race-call"),
        vec![&text("raced content")]
    );
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "race-call")
        .await?
        .context("raced join record missing")?;
    assert_eq!(record.invocation_id, "competing");
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "race-call")
        .await?
        .context("raced join has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "competing");
    let replay = caller.append_dispatch_content(text("retry")).await?;
    assert_eq!(replay, joined);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn inherited_steering_never_joins_a_fixed_admission_head() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-fixed", "original").await?;
    fixed_admission_head(&session, &f).await?;
    let steering = f
        .session("steer-fixed", "steer-call")
        .await?
        .with_steering_admission();
    let error = match steering.enqueue_text("steering content").await {
        Ok(_) => panic!("inherited steering joined a fixed admission head"),
        Err(error) => error,
    };
    // The ordinary reserve refusal, exactly like a non-steering caller (F2):
    // no binding may be written for the steered prompt id.
    assert_eq!(
        error.downcast_ref::<SessionBusy>(),
        Some(&SessionBusy::ActiveInvocation {
            invocation_id: "fixed-invocation".into()
        })
    );
    assert!(session
        .metadata_store
        .prompt_admission(session.storage_key(), "steer-call")
        .await?
        .is_none());
    assert!(session.load_durable_entries().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn void_steering_binding_recovery_redelivers() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-void", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let steering = f
        .session("steer-void", "steer-call")
        .await?
        .with_steering_admission();
    // Seed the session_prompt steering burn: the join binding was written, the
    // append was lost, and the joined turn has since ended (F1/M-A).
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "steer-call", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    let enqueued = steering
        .enqueue_text("recovered steering")
        .await
        .context("a dead steering binding must be recovered, not refused")?;
    let entries = session.load_durable_entries().await?;
    assert_eq!(enqueued.user_msg_seq, entries.last().context("appended")?.0);
    assert_eq!(
        user_messages(&entries, "steer-call"),
        vec![&text("recovered steering")]
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "steer-call")
        .await?
        .context("recovered steering has no admission binding")?;
    assert_eq!(
        binding.invocation_id.as_str(),
        "steer-call",
        "the binding must move to the fresh reservation"
    );
    Ok(())
}

#[tokio::test]
async fn external_steering_to_fixed_head_keeps_todays_refusal() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-ext", "original").await?;
    fixed_admission_head(&session, &f).await?;
    // `with_external_admission` keeps the admission id, so the submission is
    // steered under the known prompt id.
    let steering = f
        .session("steer-ext", "steer-call")
        .await?
        .with_external_admission()
        .with_steering_admission();
    // External (TUI/web) steering is unchanged (F2): it still binds the fixed
    // head and the append refuses with the fixed-ticket error.
    let error = match steering.enqueue_text("external steering").await {
        Ok(_) => panic!("the fixed admission head accepted an external steering append"),
        Err(error) => error,
    };
    let chain = format!("{error:#}");
    assert!(
        chain.contains("fixed admission requires fixed-ticket append"),
        "unexpected external steering refusal: {chain}"
    );
    let bound = session
        .metadata_store
        .prompt_admission_binding(session.storage_key(), "steer-call")
        .await?
        .context("external steering still binds before the refusal")?;
    assert_eq!(bound, "fixed-invocation");
    Ok(())
}
