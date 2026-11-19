use super::*;
use harnx_runtime::nats_session_metadata::{CallTimeoutOverride, RunLimitsRecord};
use harnx_runtime::{NatsSessionConfig, SessionActivationRoute, SessionInitializer};

pub(super) async fn mixed_fixture_2_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup =
        setup_interaction_owned_with_call(t, "time_get_current_time", json!({"timezone": "UTC"}))
            .await?;
    put_stale_checkpoint(
        &t.h.jetstream,
        setup.session.storage_key(),
        setup.tool_round_seq,
    )
    .await?;
    Ok(setup)
}

async fn handoff_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    setup_interaction_owned_with_call(
        t,
        "target_session_handoff",
        json!({"prompt": "execute plan"}),
    )
    .await
}

async fn selected_results(
    t: &TwoBackends,
    setup: &InteractionOwnedSetup,
    target: Option<&str>,
) -> Result<()> {
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    log.append_event_async(&SessionLogEntry::ToolResults {
        results: vec![ToolOutput {
            id: Some("call-1".into()),
            name: "target_session_handoff".into(),
            output: json!({"status": "handoff"}),
            markdown: None,
            content: vec![],
            switch_agent: Some(SwitchAgentData {
                agent: "target".into(),
                prompt: "execute plan".into(),
                session_id: target.map(str::to_owned),
                tool_call_id: Some("call-1".into()),
            }),
        }],
        handoff_selection: Some("call-1".into()),
        timestamp: None,
    })
    .await?;
    Ok(())
}

pub(super) async fn mixed_fixture_4a_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let setup = handoff_setup(t).await?;
    selected_results(t, &setup, None).await?;
    Ok(setup)
}

async fn parent_limits(t: &TwoBackends, setup: &InteractionOwnedSetup) -> Result<RunLimitsRecord> {
    let invocation = &setup
        .before
        .document
        .state
        .active
        .as_ref()
        .context("active task")?
        .admission
        .invocation_id;
    if let Some(record) =
        t.h.metadata
            .get_invocation_limits(setup.session.storage_key(), invocation)
            .await?
    {
        return Ok(record);
    }
    let intent =
        t.h.metadata
            .admission(setup.session.storage_key(), invocation)
            .await?
            .context("source admission")?;
    let record = RunLimitsRecord::admit_root(
        intent.run_id,
        intent.invocation_id,
        intent.admitted_at,
        Default::default(),
        None,
        CallTimeoutOverride::Omitted,
    )?;
    t.h.metadata
        .put_invocation_limits(setup.session.storage_key(), &record)
        .await?;
    Ok(record)
}

async fn enqueue_valid_handoff_target(
    t: &TwoBackends,
    setup: &InteractionOwnedSetup,
) -> Result<String> {
    let parent = parent_limits(t, setup).await?;
    let edge = format!("{}:call-1", setup.tool_round_seq);
    let invocation =
        harnx_core::session_identity::session_key(Some(parent.invocation_id.as_str()), &edge);
    let admitted_at =
        chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis())
            .context("handoff admission time")?;
    let lineage_key = format!(
        "sessions/{}/handoff_admissions/{invocation}",
        setup.session.storage_key()
    );
    // Match admission_cas_lineage, so recovery reuses the same policy and time.
    let lineage = json!({"snapshot": parent, "started_at_ms": admitted_at.timestamp_millis()});
    t.h.metadata
        .kv_store()
        .update(&lineage_key, serde_json::to_vec(&lineage)?.into(), 0)
        .await?;
    let target = NatsSession::new(
        NatsSessionConfig {
            cluster: "runner".into(),
            initializer: SessionInitializer::named("target", Default::default()),
            session_id: Some("mixed-target-1".into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        t.h.jetstream.client().clone(),
        t.h.jetstream.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await?
    .with_inherited_admission(
        parent,
        invocation,
        admitted_at,
        harnx_runtime::nats_session_metadata::InvocationEdgeKind::Handoff,
        None,
    );
    target
        .enqueue_text("execute plan")
        .await?
        .into_activation_result()?;
    Ok(target.storage_key().to_string())
}

pub(super) async fn mixed_fixture_4b_setup(t: &TwoBackends) -> Result<InteractionOwnedSetup> {
    let mut setup = handoff_setup(t).await?;
    setup.target_storage = Some(enqueue_valid_handoff_target(t, &setup).await?);
    selected_results(t, &setup, Some("mixed-target-1")).await?;
    Ok(setup)
}
