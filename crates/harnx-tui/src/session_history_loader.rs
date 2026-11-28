use anyhow::Result;
use harnx_core::abort::wait_abort_signal;
use harnx_runtime::config::{
    remote_session_ops::{load_remote_transcript_for_render_readonly, RemoteTranscriptState},
    GlobalConfig,
};
use harnx_runtime::utils::AbortSignal;

/// Load remote session transcript strictly read-only without mutating
/// `GlobalConfig`, creating JetStream buckets/streams, claiming metadata, or
/// installing/altering the active session persistence sink.
pub(crate) async fn load_remote_session_history(
    config: &GlobalConfig,
    agent: String,
    cluster: String,
    session_id: String,
    abort_signal: Option<&AbortSignal>,
) -> Result<RemoteTranscriptState> {
    if let Some(abort) = abort_signal {
        if abort.aborted() {
            anyhow::bail!("session history load cancelled");
        }
    }
    let config_snapshot = config.read().clone();
    let storage_key = harnx_core::session_identity::session_key(Some(&agent), &session_id);
    let load = async move {
        let client = config_snapshot
            .nats_client_with_abort(&cluster, abort_signal)
            .await?;
        let jetstream = async_nats::jetstream::new(client);
        load_remote_transcript_for_render_readonly(&jetstream, &storage_key, &session_id).await
    };
    match abort_signal {
        Some(abort) => {
            tokio::select! {
                biased;
                _ = wait_abort_signal(abort) => anyhow::bail!("session history load cancelled"),
                result = load => result,
            }
        }
        None => load.await,
    }
}
