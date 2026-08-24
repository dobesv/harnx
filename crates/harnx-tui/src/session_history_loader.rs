use anyhow::Result;
use harnx_runtime::config::{
    remote_session_ops::{load_remote_transcript_for_render, RemoteTranscriptState},
    GlobalConfig,
};
use harnx_runtime::{NatsSession, NatsSessionConfig};

pub(crate) async fn load_remote_session_history(
    config: &GlobalConfig,
    agent: String,
    cluster: String,
    session_id: String,
) -> Result<RemoteTranscriptState> {
    let session = NatsSession::from_global_config(
        NatsSessionConfig {
            cluster,
            agent,
            session_id: Some(session_id),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        config,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await?;
    load_remote_transcript_for_render(&session).await
}
