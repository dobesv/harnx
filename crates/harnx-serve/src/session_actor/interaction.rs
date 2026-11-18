use super::{
    open_actor_nats_session, prompt_config_for_agent_session_from_global, SessionActor,
    SessionActorConfig, SessionKey,
};
use ag_ui_core::event::Event;
use harnx_core::abort::create_abort_signal;
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::config::Config;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use tokio::sync::broadcast;

type DecisionReply = tokio::sync::oneshot::Sender<Result<InteractionDecisionOutcome, String>>;

impl SessionActor {
    pub(super) fn broadcast_pending_interaction(&self) {
        if let Some(event) = self
            .log_entries
            .as_deref()
            .and_then(crate::ag_ui_interactions::pending_event)
        {
            let _ = self.broadcast_tx.send(event);
        }
    }

    pub(super) fn answer_interaction(
        &self,
        interaction_id: String,
        request_seq: u64,
        response: InteractionResponse,
        reply: DecisionReply,
    ) {
        let actor_config = self.actor_config.clone();
        let key = self.key.clone();
        let events = self.broadcast_tx.clone();
        // Worker startup/control acknowledgements must not hold the actor mailbox:
        // Cancel, attach and other decisions remain responsive during this wait.
        tokio::spawn(async move {
            let result =
                route_decision(&actor_config, &key, &interaction_id, request_seq, &response)
                    .await
                    .map_err(|error| format!("{error:#}"));
            if result.as_ref().is_ok_and(|outcome| outcome.is_applied()) {
                broadcast_latest(&actor_config.base_config, &key, &events).await;
            }
            let _ = reply.send(result);
        });
    }
}

async fn route_decision(
    actor_config: &SessionActorConfig,
    key: &SessionKey,
    interaction_id: &str,
    request_seq: u64,
    response: &InteractionResponse,
) -> anyhow::Result<InteractionDecisionOutcome> {
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
        create_abort_signal(),
    )
    .await?;
    session
        .decide_interaction(interaction_id, request_seq, response)
        .await
}

async fn broadcast_latest(config: &Config, key: &SessionKey, events: &broadcast::Sender<Event>) {
    match crate::load_nats_session(config, key.target(), key.session()).await {
        Ok((_, entries)) => {
            if let Some(event) = crate::ag_ui_interactions::pending_event(&entries) {
                let _ = events.send(event);
            }
        }
        Err(error) => log::warn!("cannot refresh interaction after response: {error:#}"),
    }
}
