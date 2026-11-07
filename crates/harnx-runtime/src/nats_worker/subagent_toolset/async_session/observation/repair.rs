//! Optional delivery repair is outside the read-only snapshot scanner.
use super::{
    budget::{Budget, Stop},
    scope::Scope,
    snapshot::Snapshot,
    DispatchConfirmation, SessionPromptStatus, SubagentToolset,
};
use crate::nats_worker::{
    publish_session_activate, publish_targeted_session_activate, LocalWorkerTarget,
    SessionActivate, SessionActivationRoute,
};

pub(super) async fn repair_once(
    toolset: &SubagentToolset,
    scope: &Scope,
    snapshot: &mut Snapshot,
    budget: &Budget,
) -> Result<Option<DispatchConfirmation>, Stop> {
    // Refresh right before any publication. A terminal or approval that became
    // durable while formatting the last snapshot must suppress repair too.
    let log =
        crate::nats_session_log::NatsSessionLog::new(toolset.jetstream.clone(), &scope.storage);
    let sequence = snapshot.prompt.ok_or(Stop::Unavailable)?;
    let original = budget
        .io(log.load_entry_async(sequence))
        .await?
        .map_err(|_| Stop::Unavailable)?;
    if !original
        .as_ref()
        .is_some_and(|entry| scope.matches_prompt(entry))
    {
        return Err(Stop::Unavailable);
    }
    snapshot.refresh(&log, scope, budget).await?;
    if snapshot.status() != SessionPromptStatus::Pending || scope.call_id.is_empty() {
        return Ok(None);
    }
    let expired = budget
        .io(crate::nats_session::dispatch::dispatch_deadline_expired(
            &toolset.session_metadata,
            &scope.storage,
            &scope.admission,
        ))
        .await?
        .map_err(|_| Stop::Unavailable)?;
    if expired {
        return Ok(None);
    }
    budget.check()?;
    let call_id = &scope.call_id;
    let activation = SessionActivate {
        session_id: scope.storage.clone(),
        execution_id: Some(scope.admission.invocation_id.as_str().into()),
        // Fresh relative to send replay, stable across lost-reply replay of THIS
        // wait call. A later wait call has its own transport invocation identity.
        epoch: format!("observe:{call_id}:{sequence}"),
        agent_name: Some(toolset.agent.clone()),
        requested_seq: Some(sequence),
        target_worker_id: None,
        token_budget: None,
        tool_confirmation_subject: None,
    };
    let result = budget.io(publish(toolset, activation)).await?;
    Ok(Some(if result.is_ok() {
        DispatchConfirmation::Confirmed
    } else {
        DispatchConfirmation::Unconfirmed
    }))
}

async fn publish(toolset: &SubagentToolset, mut activation: SessionActivate) -> anyhow::Result<()> {
    match toolset.route.activation_route() {
        SessionActivationRoute::ClusterShared => {
            publish_session_activate(
                &toolset.jetstream,
                toolset.route.cluster(),
                &activation,
                toolset.replicas,
            )
            .await?;
        }
        SessionActivationRoute::WorkerTargeted {
            session_scope,
            worker_id,
        } => {
            activation.target_worker_id = Some(worker_id.clone());
            publish_targeted_session_activate(
                &toolset.jetstream,
                LocalWorkerTarget::new(session_scope, worker_id)?,
                &activation,
            )
            .await?;
        }
    }
    Ok(())
}
