use anyhow::Result;
use harnx_execution_control::{ExecutionStore, Owner};
use harnx_runtime::execution_fence::GenerationFence;

pub async fn generation_fence(
    js: &async_nats::jetstream::Context,
    session: &str,
    owner: Owner,
) -> Result<GenerationFence> {
    let store = ExecutionStore::ensure(js, 1).await?;
    let operation = store.session(session, None, None).await?;
    store.claim(&operation.reference, owner).await?;
    let context = store.activate_gate(&operation.reference).await?;
    Ok(GenerationFence::new(store, context))
}

pub async fn output_backend(
    js: &async_nats::jetstream::Context,
    session: &str,
) -> Result<harnx_runtime::nats_worker::NatsSessionLogBackend> {
    let fence = generation_fence(js, session, Owner::invocation("test-worker")).await?;
    Ok(
        harnx_runtime::nats_worker::NatsSessionLogBackend::new(js.clone(), session)
            .with_execution(Some(fence)),
    )
}
