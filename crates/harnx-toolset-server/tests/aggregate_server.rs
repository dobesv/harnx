mod common;

use anyhow::Result;
use async_nats::jetstream::kv;
use common::{request_headers, spawn_nats_server, TestToolset, TOKEN};
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{ToolReply, ToolRequest};
use harnx_toolset_server::{
    registration_key, serve_many_with_shutdown, ServeLifecycle, TOOL_REGISTRY_BUCKET,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn registers_and_serves_every_toolset_as_one_lifecycle() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let server_client = connect(&server.url).await?;
    let client = connect(&server.url).await?;
    let instance_id = ServerScope::new();
    let shutdown = CancellationToken::new();
    let readiness = harnx_healthz::Readiness::default();
    let task = start_aggregate(
        server_client,
        instance_id.clone(),
        shutdown.clone(),
        readiness.clone(),
    );
    let registry = wait_for_registry(&client).await?;
    let keys = registration_keys(&instance_id);

    wait_until_ready(&registry, &keys, &readiness).await?;
    invoke_named(&client, &instance_id, "first").await?;
    invoke_named(&client, &instance_id, "second").await?;

    shutdown.cancel();
    task.await??;
    assert!(!readiness.is_ready());
    assert_registrations_removed(&registry, &keys).await
}

async fn connect(url: &str) -> Result<async_nats::Client> {
    Ok(async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(url)
        .await?)
}

fn start_aggregate(
    client: async_nats::Client,
    instance_id: ServerScope,
    shutdown: CancellationToken,
    readiness: harnx_healthz::Readiness,
) -> JoinHandle<Result<()>> {
    tokio::spawn(serve_many_with_shutdown(
        vec![
            Arc::new(TestToolset::named("first")),
            Arc::new(TestToolset::named("second")),
        ],
        instance_id,
        NatsConnection {
            client,
            replicas: 1,
        },
        ServeLifecycle::new(shutdown, Some(readiness)),
    ))
}

fn registration_keys(instance_id: &ServerScope) -> [String; 2] {
    [
        registration_key(instance_id, "____first"),
        registration_key(instance_id, "____second"),
    ]
}

async fn wait_for_registry(client: &async_nats::Client) -> Result<kv::Store> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(registry) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            return Ok(registry);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("aggregate server did not create its registry bucket");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_until_ready(
    registry: &kv::Store,
    keys: &[String],
    readiness: &harnx_healthz::Readiness,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if registrations_present(registry, keys).await? && readiness.is_ready() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("aggregate toolsets did not all register and become ready");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn registrations_present(registry: &kv::Store, keys: &[String]) -> Result<bool> {
    for key in keys {
        if registry.get(key).await?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn invoke_named(client: &async_nats::Client, scope: &ServerScope, name: &str) -> Result<()> {
    let request = ToolRequest {
        replay: None,
        operation_id: format!("call-{name}"),
        call_id: format!("call-{name}"),
        tool: "echo".to_string(),
        args: json!({"server": name}),
        parent_session_id: Some("session-1".to_string()),
        tool_call_id: None,
        capabilities: Default::default(),
    };
    let message = client
        .request_with_headers(
            scope.tool_subject(&format!("____{name}"), "echo"),
            request_headers(&request.call_id, &format!("logical-{name}")),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    assert_eq!(reply.result.unwrap(), request.args);
    Ok(())
}

async fn assert_registrations_removed(registry: &kv::Store, keys: &[String]) -> Result<()> {
    for key in keys {
        assert!(registry.get(key).await?.is_none());
    }
    Ok(())
}
