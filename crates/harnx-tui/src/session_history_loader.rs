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
    // Do not create this temporary read guard inside the async constructor's
    // argument list: temporaries can live through `.await`, while
    // `from_global_config` later needs a write guard to attach its metadata
    // persistence sink.
    let initializer = {
        let config = config.read();
        harnx_runtime::SessionInitializer::named_from_config(agent, &config)
    };
    let session = NatsSession::from_global_config(
        NatsSessionConfig {
            cluster,
            initializer,
            session_id: Some(session_id),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        config,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await?;
    load_remote_transcript_for_render(&session).await
}
