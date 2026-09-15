use super::*;
use futures_util::{FutureExt, StreamExt};

#[tokio::test]
async fn abort_during_hook_shutdown_retains_pending_route_cleanup() -> Result<()> {
    let nats = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(nats.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let scope = ServerScope::new();
    let key = harnx_hookset_server::hook_registration_key(&scope, "hook");
    let mut stores = Vec::new();
    for bucket in [
        harnx_hookset::HOOK_REGISTRY_BUCKET,
        HOOK_EXPECTATIONS_BUCKET,
    ] {
        let store = js
            .create_key_value(kv::Config {
                bucket: bucket.into(),
                ..Default::default()
            })
            .await?;
        store.put(&key, "registered".into()).await?;
        stores.push(store);
    }
    let mut updates =
        futures_util::stream::select(stores[0].watch_all().await?, stores[1].watch_all().await?);
    let mut supervisor = HookServerSupervisor {
        _process_manager: ChildProcessManager::new(),
        processes: Arc::default(),
        tasks: Vec::new(),
        client,
        instance_id: scope,
        registrations: vec!["hook".into()],
    };
    // On a current-thread runtime the first broker request cannot complete in
    // this poll. Dropping shutdown here reproduces turn abort during deletion.
    assert!(supervisor.shutdown().now_or_never().is_none());
    assert_eq!(supervisor.registrations, ["hook"]);
    drop(supervisor);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if stores[0].get(&key).await?.is_none() && stores[1].get(&key).await?.is_none() {
                return Ok::<_, anyhow::Error>(());
            }
            updates.next().await.context("cleanup watch closed")??;
        }
    })
    .await??;
    Ok(())
}
