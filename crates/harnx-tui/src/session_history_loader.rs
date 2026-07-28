use harnx_runtime::config::{
    remote_session_ops::{load_remote_transcript_for_render, RemoteTranscriptState},
    GlobalConfig,
};
use harnx_runtime::{ThinClientConfig, ThinClientSession};
use log::warn;

pub(crate) fn load_remote_session_history(
    config: &GlobalConfig,
    agent: String,
    cluster: String,
    session_id: String,
) -> Option<RemoteTranscriptState> {
    let remote_load = || async {
        let thin = ThinClientSession::from_global_config(
            ThinClientConfig {
                cluster,
                agent,
                session_id: Some(session_id),
            },
            config,
            harnx_runtime::utils::create_abort_signal(),
        )
        .await?;
        load_remote_transcript_for_render(&thin).await
    };
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(remote_load())),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(remote_load())),
    };
    match result {
        Ok(state) => Some(state),
        Err(error) => {
            warn!("failed to load remote session transcript for resume: {error:#}");
            None
        }
    }
}
