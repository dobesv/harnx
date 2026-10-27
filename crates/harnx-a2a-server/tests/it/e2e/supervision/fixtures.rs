use super::*;

pub(super) async fn await_worker_lease_renewed(
    leases: &async_nats::jetstream::kv::Store,
    worker_key: &str,
    before_rev: u64,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(entry) = harnx_nats_common::leader_reads::entry(leases, worker_key).await? {
                if entry.revision > before_rev {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("worker must keep renewing after owner death")?
}

pub(super) async fn await_parked_ownerless(
    store: &A2aStore,
    storage_key: &str,
) -> Result<harnx_a2a_server::store::context::ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = store.read_context(storage_key).await?.context("context")?;
            if context.document.owner.is_none() {
                return Ok::<_, anyhow::Error>(context);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await?
}

pub(super) fn assert_preserved_admission(
    before: &harnx_a2a_server::store::context::ContextSnapshot,
    after: &harnx_a2a_server::store::context::ContextSnapshot,
) {
    let original = before.document.state.active.as_ref().unwrap();
    let settled = after.document.state.active.as_ref().unwrap();
    assert_eq!(original.message, settled.message);
    assert_eq!(
        original.admission.invocation_id,
        settled.admission.invocation_id
    );
    assert_eq!(original.admission.prompt_id, settled.admission.prompt_id);
    assert_eq!(
        original.admission.fixed_predecessor,
        settled.admission.fixed_predecessor
    );
}
