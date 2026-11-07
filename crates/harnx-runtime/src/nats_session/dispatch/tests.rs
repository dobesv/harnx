use super::*;
use crate::nats_session_metadata::{CallTimeoutOverride, InvocationEdgeKind, RunLimitsRecord};
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use crate::nats_worker::SessionActivate;
use serde_json::json;

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
        assert!(competing
            .append_dispatch_content(text("must not queue"))
            .await
            .unwrap_err()
            .to_string()
            .contains("session busy"));
        assert_eq!(session.load_durable_entries().await?.len(), 1);
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
