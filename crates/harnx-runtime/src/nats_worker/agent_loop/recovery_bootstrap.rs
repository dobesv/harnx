use super::*;

// Direct loop callers do not go through WorkerExecution::claim. They still
// capture generation authority before reconstruction and before constructing sinks.
pub(super) async fn prepare_standalone_generation(
    params: &PrepareAgentSessionParams<'_>,
    js: &jetstream::Context,
) -> Result<crate::execution_fence::GenerationFence> {
    let (store, reference) = standalone_execution(params, js).await?;
    let owner = params.lease.map_or_else(
        || harnx_execution_control::Owner::invocation("standalone-worker"),
        |lease| harnx_execution_control::Owner {
            instance_id: lease.worker_id().into(),
            fence: lease.fence_token(),
        },
    );
    reject_registered_stop(&store, js, &reference).await?;
    store.claim(&reference, owner).await?;
    let context = store.activate_gate(&reference).await?;
    params.config.write().execution_control = Some((store.clone(), reference));
    Ok(crate::execution_fence::GenerationFence::new(store, context))
}

async fn standalone_execution(
    params: &PrepareAgentSessionParams<'_>,
    js: &jetstream::Context,
) -> Result<(
    harnx_execution_control::ExecutionStore,
    harnx_execution_control::OperationRef,
)> {
    let existing = params.config.read().execution_control.clone();
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let store = harnx_execution_control::ExecutionStore::ensure(js, 1).await?;
    let pending =
        crate::nats_session::cancellation::resolve_pending_execution(&store, js, params.session_id)
            .await?;
    let previous = pending.or(store.current(params.session_id).await?);
    let operation = match previous {
        Some(operation)
            if operation.state != harnx_execution_control::OperationState::Completed =>
        {
            operation
        }
        _ => store.session(params.session_id, None, None).await?,
    };
    let reference = operation.reference;
    Ok((store, reference))
}

async fn reject_registered_stop(
    store: &harnx_execution_control::ExecutionStore,
    js: &jetstream::Context,
    reference: &harnx_execution_control::OperationRef,
) -> Result<()> {
    let registered = store
        .get(reference)
        .await?
        .is_some_and(|op| op.gate_registration.is_some());
    if !registered {
        return Ok(());
    }
    let log = crate::nats_session_log::NatsSessionLog::new(js.clone(), &reference.session_id);
    if let Some(interrupted) = log.recover_stop(store, reference).await? {
        return Err(interrupted.into());
    }
    Ok(())
}
