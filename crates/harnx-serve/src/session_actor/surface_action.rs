//! Surface action routing for the serve session actor (plan T5.1).

use super::{
    open_actor_nats_session, prompt_config_for_agent_session_from_global, SessionActor,
    SessionActorConfig, SessionKey,
};
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};

type SurfaceActionReply = tokio::sync::oneshot::Sender<Result<SurfaceActionOutcome, String>>;

impl SessionActor {
    pub(super) fn answer_surface_action(
        &self,
        submission: SurfaceActionSubmission,
        reply: SurfaceActionReply,
    ) {
        let actor_config = self.actor_config.clone();
        let key = self.key.clone();
        // A direct tool target runs through the operator RPC inside the
        // worker, which can take minutes; the actor mailbox must stay
        // responsive to prompts and cancels meanwhile.
        tokio::spawn(async move {
            let result = route_surface_action(&actor_config, &key, &submission)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        });
    }
}

async fn route_surface_action(
    actor_config: &SessionActorConfig,
    key: &SessionKey,
    submission: &SurfaceActionSubmission,
) -> anyhow::Result<SurfaceActionOutcome> {
    let prompt_config = prompt_config_for_agent_session_from_global(
        &actor_config.base_config,
        key,
        actor_config.call_fn.is_some(),
    )
    .await;
    let session = open_actor_nats_session(
        &prompt_config,
        &actor_config.local_worker,
        key.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await?;
    session.submit_surface_action(submission).await
}
