use super::*;
use futures_util::StreamExt;

#[tokio::test]
async fn lost_cas_ack_is_reconciled_without_repeating_the_mutation() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.into())
        .connect(&server.url)
        .await?;
    let mut js = jetstream::new(client.clone());
    js.set_timeout(Duration::from_millis(100));
    let store = js
        .create_key_value(jetstream::kv::Config {
            bucket: "lost_ack".into(),
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
                "$KV.lost_ack.operation",
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
    let observed =
        harnx_nats_common::cas::update(&fault_store, "operation".into(), "after".into(), revision)
            .await?;
    assert_eq!(observed, proxy.await??);
    assert_eq!(
        store
            .get("operation")
            .await?
            .context("stored mutation")?
            .as_ref(),
        b"after"
    );
    let conflict =
        harnx_nats_common::cas::update(&store, "operation".into(), "wrong".into(), revision)
            .await
            .unwrap_err();
    assert_eq!(
        conflict.kind(),
        jetstream::kv::UpdateErrorKind::WrongLastRevision
    );
    Ok(())
}
