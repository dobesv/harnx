use super::*;
use harnx_execution_control::{OperationRef, Owner};
use harnx_runtime::execution_fence::GenerationFence;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_cancel_cannot_cover_g2_prompt_before_g2_worker_claim() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let first = session(server.url(), "cancel-before-g2-gate-install").await?;
    let config = local_nats_runtime_config(server.url());
    let input = harnx_runtime::config::input::from_str(&config, "G1", None);
    let admitted = first.admit_input(&input, None).await?;
    let reference = OperationRef::new(first.storage_key(), admitted.execution_id());
    let store = first.execution_store();
    store
        .claim(&reference, Owner::invocation("old-worker"))
        .await?;
    let context = store.activate_gate(&reference).await?;
    assert!(first.cancel_pending_turn().await?);

    let second = session(server.url(), first.session_id()).await?;
    let input = harnx_runtime::config::input::from_str(&config, "G2", None);
    let next = second.admit_input(&input, None).await?;
    assert_ne!(admitted.execution_id(), next.execution_id());
    // Physical pointer/admission is G2, but the gate still selects G1 until claim.
    assert_eq!(
        store
            .gate_generation(context.gate_root(), first.storage_key())
            .await?,
        reference
    );
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), first.storage_key());
    let before = log.load_events_latest_async().await?;
    let backend = NatsSessionLogBackend::new(js, first.storage_key())
        .with_execution(Some(GenerationFence::new(store.clone(), context.clone())));
    let error = backend
        .append_event(&SessionLogEntry::Cancel {
            fence_token: context.owner().fence,
        })
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot project old cancellation over another generation"),
        "{error:#}"
    );
    assert_eq!(
        serde_json::to_value(log.load_events_latest_async().await?)?,
        serde_json::to_value(before)?
    );
    Ok(())
}
