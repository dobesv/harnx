use anyhow::{anyhow, Result};
use harnx_runtime::config::{Config, LOCAL_CLUSTER_KEY};

pub(crate) async fn serve_nats_client(
    config: &Config,
    cluster: &str,
) -> Result<async_nats::Client> {
    config
        .nats_client(cluster)
        .await
        .map_err(|error| sanitize_nats_cluster_error(cluster, error))
}

pub(crate) async fn serve_nats_jetstream(
    config: &Config,
    cluster: &str,
) -> Result<async_nats::jetstream::Context> {
    config
        .nats_jetstream(cluster)
        .await
        .map_err(|error| sanitize_nats_cluster_error(cluster, error))
}

/// Logs full error server-side and returns a credential-free client message.
fn nats_cluster_unavailable(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    log::error!("NATS cluster '{cluster}' is unavailable: {error:#}");
    anyhow!("NATS cluster '{cluster}' is unavailable")
}

pub(crate) fn sanitize_nats_cluster_error(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    if cluster == LOCAL_CLUSTER_KEY {
        error
    } else {
        nats_cluster_unavailable(cluster, error)
    }
}

pub(crate) fn sanitize_nats_session_error(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    let is_connect_error = error.chain().any(|cause| {
        cause.downcast_ref::<async_nats::ConnectError>().is_some()
            || cause
                .to_string()
                .starts_with("Failed to connect to NATS cluster")
    });
    if is_connect_error {
        nats_cluster_unavailable(cluster, error)
    } else {
        error
    }
}
