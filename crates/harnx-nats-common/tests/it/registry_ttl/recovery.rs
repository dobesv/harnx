use super::*;
use futures_util::StreamExt;

const BUCKET: &str = "lost_ack";

/// A broker whose direct gets for the bucket reach an empty stream, which
/// answers "not found" as a follower that hasn't applied a write does.
fn lagging_replica_config() -> String {
    format!(
        r#"mappings = {{
  "$JS.API.DIRECT.GET.KV_{BUCKET}": "$JS.API.DIRECT.GET.STALE_REPLICA"
  "$JS.API.DIRECT.GET.KV_{BUCKET}.>": "$JS.API.DIRECT.GET.STALE_REPLICA.>"
}}
"#
    )
}

struct LostAck {
    js: jetstream::Context,
    store: jetstream::kv::Store,
    /// Sends its writes through the proxy, which applies one and loses its ack.
    fault_store: jetstream::kv::Store,
    revision: u64,
    proxy: tokio::task::JoinHandle<Result<u64>>,
}

/// Create the key, then route the next write to it through a proxy that
/// applies it once and never answers the caller.
async fn lose_next_ack(server: &NatsServerHandle) -> Result<LostAck> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.into())
        .connect(&server.url)
        .await?;
    let mut js = jetstream::new(client.clone());
    js.set_timeout(Duration::from_millis(100));
    let store = js
        .create_key_value(jetstream::kv::Config {
            bucket: BUCKET.into(),
            ..Default::default()
        })
        .await?;
    let revision = store.create("operation", "before".into()).await?;
    let mut fault_store = store.clone();
    fault_store.put_prefix = Some("fault.".into());
    let mut requests = client.subscribe("fault.operation").await?;
    client.flush().await?;
    let actual_js = js.clone();
    let proxy = tokio::spawn(async move {
        let request = requests.next().await.context("CAS request")?;
        let ack = actual_js
            .publish_with_headers(
                format!("$KV.{BUCKET}.operation"),
                request.headers.unwrap_or_default(),
                request.payload,
            )
            .await?
            .await?;
        // Apply once, but deliberately lose its response to the original caller.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), requests.next())
                .await
                .is_err(),
            "ambiguous mutation was replayed"
        );
        Ok::<_, anyhow::Error>(ack.sequence)
    });
    Ok(LostAck {
        js,
        store,
        fault_store,
        revision,
        proxy,
    })
}

#[tokio::test]
async fn lost_cas_ack_is_reconciled_without_repeating_the_mutation() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let lost = lose_next_ack(&server).await?;
    let observed = harnx_nats_common::cas::update(
        &lost.fault_store,
        "operation".into(),
        "after".into(),
        lost.revision,
    )
    .await?;
    assert_eq!(observed, lost.proxy.await??);
    assert_eq!(
        lost.store
            .get("operation")
            .await?
            .context("stored mutation")?
            .as_ref(),
        b"after"
    );
    let conflict = harnx_nats_common::cas::update(
        &lost.store,
        "operation".into(),
        "wrong".into(),
        lost.revision,
    )
    .await
    .unwrap_err();
    assert_eq!(
        conflict.kind(),
        jetstream::kv::UpdateErrorKind::WrongLastRevision
    );
    Ok(())
}

/// Only a read of the key can tell whether a write whose ack was lost
/// landed. A follower that hasn't applied it reports the key missing, which
/// left a write that had landed unconfirmed.
#[tokio::test]
async fn lost_cas_ack_is_confirmed_while_replicas_lag() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_configured_nats_server(Some(&lagging_replica_config())).await? else {
        return Ok(());
    };
    let lost = lose_next_ack(&server).await?;
    lost.js
        .create_stream(jetstream::stream::Config {
            name: "STALE_REPLICA".into(),
            subjects: vec!["harnx.test.stale-replica".into()],
            allow_direct: true,
            storage: jetstream::stream::StorageType::Memory,
            ..Default::default()
        })
        .await?;
    anyhow::ensure!(
        lost.store.get("operation").await?.is_none(),
        "a direct get saw a write the stale replica never applied"
    );

    let observed = harnx_nats_common::cas::update(
        &lost.fault_store,
        "operation".into(),
        "after".into(),
        lost.revision,
    )
    .await?;
    assert_eq!(observed, lost.proxy.await??);
    Ok(())
}
