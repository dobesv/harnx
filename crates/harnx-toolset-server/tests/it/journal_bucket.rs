//! The journal bucket's stream settings, on a bucket this build creates and on
//! one an older build left behind.
use crate::common::{self, TOKEN};
use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, stream};
use harnx_toolset_server::invocation_journal::{InvocationJournal, BUCKET};

async fn connect(server_url: &str) -> Result<jetstream::Context> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server_url)
        .await
        .context("connect to the test broker")?;
    Ok(jetstream::new(client))
}

/// The journal stream's configuration as the server reports it.
async fn journal_stream_config(js: &jetstream::Context) -> Result<stream::Config> {
    Ok(js
        .get_stream(format!("KV_{BUCKET}"))
        .await?
        .info()
        .await?
        .config
        .clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_journal_bucket_is_compressed() -> Result<()> {
    let (server, _journal) = common::journal().await;
    let js = connect(&server.url).await?;

    assert_eq!(
        journal_stream_config(&js).await?.compression,
        Some(stream::Compression::S2)
    );
    Ok(())
}

/// Builds before compression created the bucket without it. Opening the
/// journal turns it on and keeps the rows already there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_an_uncompressed_journal_bucket_compresses_it() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .expect("nats-server binary is required for journal tests");
    let js = connect(&server.url).await?;
    let older = js
        .create_key_value(kv::Config {
            bucket: BUCKET.into(),
            num_replicas: 1,
            ..Default::default()
        })
        .await
        .context("create the bucket the way older builds did")?;
    older.put("standalone/kept", "row".into()).await?;
    assert_ne!(
        journal_stream_config(&js).await?.compression,
        Some(stream::Compression::S2),
        "the older bucket must start uncompressed for this test to mean anything"
    );

    InvocationJournal::ensure(&js, 1).await?;

    let config = journal_stream_config(&js).await?;
    assert_eq!(config.compression, Some(stream::Compression::S2));
    assert_eq!(config.num_replicas, 1);
    assert_eq!(older.get("standalone/kept").await?, Some("row".into()));
    Ok(())
}
