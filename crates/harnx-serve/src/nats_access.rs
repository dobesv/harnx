use anyhow::Result;
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

/// Marker for "the broker is unreachable, or the shared local broker could
/// not be started". Attached where the failure happens, so downstream code
/// classifies unavailability by type instead of by error text. Carries the
/// cluster name so the error it becomes is the whole client-facing message:
/// a second Display in the chain would change messages other code compares
/// or sends to clients.
#[derive(Debug)]
pub(crate) struct NatsClusterUnavailable {
    cluster: String,
}

// qual:allow(dry, boilerplate) reason: "marker error carries the cluster name; Display is written by hand because harnx-serve has no error-derive dependency."
impl std::fmt::Display for NatsClusterUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NATS cluster '{}' is unavailable", self.cluster)
    }
}

impl std::error::Error for NatsClusterUnavailable {}

/// Logs the full failure server-side, with NATS URL userinfo redacted, and
/// returns the credential-free marker error for the client-facing path.
pub(crate) fn nats_cluster_unavailable(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    log::error!(
        "NATS cluster '{cluster}' is unavailable: {}",
        sanitized_error_chain(&error)
    );
    anyhow::Error::new(NatsClusterUnavailable {
        cluster: cluster.to_string(),
    })
}

/// Render an error chain with inline NATS URL credentials redacted, for
/// log lines only. Cluster URLs can carry userinfo
/// (`nats://user:pass@host`), and the context messages `Config::nats_client`
/// adds echo them verbatim.
pub(crate) fn sanitized_error_chain(error: &anyhow::Error) -> String {
    crate::nats_redaction::redact_url_credentials(&format!("{error:#}"))
}

pub(crate) fn sanitize_nats_cluster_error(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    if cluster == LOCAL_CLUSTER_KEY {
        error
    } else {
        nats_cluster_unavailable(cluster, error)
    }
}

/// Whether this error is a failure to reach the broker, as opposed to a
/// failure of a store operation on a working connection. Both a raw
/// `ConnectError` and the context `Config::nats_client` adds count.
pub(crate) fn is_nats_connect_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<async_nats::ConnectError>().is_some()
            || cause
                .to_string()
                .starts_with("Failed to connect to NATS cluster")
    })
}

pub(crate) fn sanitize_nats_session_error(cluster: &str, error: anyhow::Error) -> anyhow::Error {
    if is_nats_connect_error(&error) {
        nats_cluster_unavailable(cluster, error)
    } else {
        error
    }
}

/// Whether an attachment object-store failure means the broker or JetStream
/// could not be reached, as opposed to a store failure on a reachable
/// broker. Classified by error type and JetStream error code, never by
/// message text. The upload paths produce these chains:
///
/// - the [`NatsClusterUnavailable`] marker, set where the failure happens;
/// - `ConnectError`, kept in the chain by the context `Config::nats_client`
///   adds around it;
/// - core-NATS request round trips that found no responder or timed out
///   (`async_nats::RequestError`, and the JetStream context's
///   `context::RequestError`, whose conversion from the core error drops
///   the cause for no-responders);
/// - the JetStream API wrappers the upload paths go through, whose error
///   kinds move the server's `jetstream::Error` into the wrapper's kind,
///   out of the source chain: stream lookup and creation (`GetStreamError`,
///   `CreateStreamError`, reached through `get_key_value`,
///   `get_object_store` and `create_object_store`) and raw-message reads
///   (`LastRawMessageError`), matched by their `JetStream(error)` kinds'
///   codes plus the bare no-responder/timeout kinds async-nats maps to;
/// - the recovery deadline marker, when a leader read exhausted its retry
///   window without confirming completion.
// qual:allow(coupling, deh) reason: "the anyhow error chain is a dynamic-typing boundary: async-nats publishes its failure modes as concrete error types and codes, and the upload error contract requires classifying by those types instead of message text (is_nats_connect_error already downcasts the same way). An enum over the variants would have to re-wrap every async-nats error in harnx-blob-store and harnx-runtime."
pub(crate) fn is_nats_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<NatsClusterUnavailable>().is_some()
            || cause.downcast_ref::<async_nats::ConnectError>().is_some()
            || cause
                // qual:allow(coupling, deh) reason: "same as the function-level marker: the recovery marker is only distinguishable by downcasting the anyhow chain."
                .downcast_ref::<harnx_nats_common::recovery::RecoveryDeadlineExceeded>()
                .is_some()
            || cause
                // qual:allow(coupling, deh) reason: "same as the function-level marker: the request kinds are only distinguishable by downcasting the anyhow chain."
                .downcast_ref::<async_nats::RequestError>()
                .is_some_and(|request| {
                    matches!(
                        request.kind(),
                        async_nats::RequestErrorKind::TimedOut
                            | async_nats::RequestErrorKind::NoResponders
                    )
                })
            || cause
                // The JetStream context wraps every core round trip of its
                // API calls; its conversion keeps the core cause for
                // timeouts but drops it for no-responders, so the wrapper
                // kind must be matched too.
                // qual:allow(coupling, deh) reason: "same as the function-level marker: the JetStream context's request kinds are only distinguishable by downcasting the anyhow chain."
                .downcast_ref::<async_nats::jetstream::context::RequestError>()
                .is_some_and(|request| {
                    matches!(
                        request.kind(),
                        async_nats::jetstream::context::RequestErrorKind::TimedOut
                            | async_nats::jetstream::context::RequestErrorKind::NoResponders
                    )
                })
            || jetstream_wrapper_unavailable(cause)
    })
}

/// Whether this chain element is a JetStream API wrapper whose failure mode
/// means the JetStream service or the stream is unavailable. async-nats
/// moves the server's `jetstream::Error` into the wrapper's kind (out of
/// the source chain) and its `From` conversions collapse core no-responders
/// and timeouts into bare wrapper kinds, so the kinds are matched here at
/// the wrapper level. `ObjectStoreError::GetStore`, `KeyValueError::GetBucket`
/// and `CreateObjectStoreError::BucketCreate` keep their `GetStreamError`/
/// `CreateStreamError` cause in the chain, so the wrapper types below cover
/// the whole ensure path.
// qual:allow(iosp) reason: "one typed dispatch over the wrapper kinds async-nats publishes; each arm names a different upload path's failure mode, and splitting the arms would scatter the kind-by-code pairing the classifier is."
fn jetstream_wrapper_unavailable(cause: &(dyn std::error::Error + 'static)) -> bool {
    use async_nats::jetstream::{
        context::{CreateStreamErrorKind, GetStreamErrorKind},
        stream::LastRawMessageErrorKind,
        ErrorCode,
    };
    fn unavailable_code(error: &async_nats::jetstream::Error) -> bool {
        matches!(
            error.error_code(),
            ErrorCode::CLUSTER_NOT_AVAILABLE | ErrorCode::STREAM_OFFLINE
        )
    }
    // qual:allow(coupling, deh) reason: "same as the function-level marker."
    if let Some(get_stream) = cause.downcast_ref::<async_nats::jetstream::context::GetStreamError>()
    {
        return matches!(get_stream.kind(), GetStreamErrorKind::JetStream(error)
            if unavailable_code(&error));
    }
    // qual:allow(coupling, deh) reason: "same as the function-level marker."
    if let Some(create_stream) =
        cause.downcast_ref::<async_nats::jetstream::context::CreateStreamError>()
    {
        return match create_stream.kind() {
            // No-responders and request timeouts, as async-nats maps them
            // from the core request error (cause dropped, kind retained).
            CreateStreamErrorKind::JetStreamUnavailable | CreateStreamErrorKind::TimedOut => true,
            CreateStreamErrorKind::JetStream(error) => unavailable_code(&error),
            _ => false,
        };
    }
    // qual:allow(coupling, deh) reason: "same as the function-level marker."
    if let Some(raw_message) =
        cause.downcast_ref::<async_nats::jetstream::stream::LastRawMessageError>()
    {
        return matches!(raw_message.kind(), LastRawMessageErrorKind::JetStream(error)
            if unavailable_code(&error));
    }
    // A `jetstream::Error` can still surface bare on paths that hand the
    // server's API error through unchanged.
    // qual:allow(coupling, deh) reason: "same as the function-level marker."
    cause
        .downcast_ref::<async_nats::jetstream::Error>()
        .is_some_and(unavailable_code)
}
