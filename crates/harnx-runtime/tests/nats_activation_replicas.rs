mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::nats_worker::{publish_session_activate, SessionActivate};

#[tokio::test]
async fn activation_work_queue_uses_configured_replicas() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);

    let single_replica_cluster = "activation_replicas_one";
    publish_session_activate(
        &jetstream,
        single_replica_cluster,
        &SessionActivate::new("activation-session-one"),
        1,
    )
    .await?;
    let mut stream = jetstream
        .get_stream("WORK_NOTIFY_activation_replicas_one")
        .await?;
    let info = stream.info().await?;
    assert_eq!(
        info.config.num_replicas, 1,
        "configured replica count must reach the activation work queue"
    );

    let error = publish_session_activate(
        &jetstream,
        "activation_replicas_three",
        &SessionActivate::new("activation-session-three"),
        3,
    )
    .await
    .expect_err("a three-replica work queue must fail on a non-clustered server");
    let error = format!("{error:#}");
    assert!(
        error.contains("with 3 replicas"),
        "creation context must report the requested replica count: {error}"
    );
    assert!(
        error.contains("replicas > 1 not supported in non-clustered mode"),
        "server must reject three replicas rather than silently creating R1: {error}"
    );

    Ok(())
}
