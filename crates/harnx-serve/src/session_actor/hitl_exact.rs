use super::{open_actor_nats_session, prompt_config_for_agent_session_from_global, SessionActor};
use harnx_core::abort::create_abort_signal;
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};

type Reply = tokio::sync::oneshot::Sender<Result<HitlDecisionOutcome, String>>;

impl SessionActor {
    pub(super) fn answer_hitl_exact(
        &self,
        request: ApprovalRef,
        approved: bool,
        note: Option<String>,
        reply: Reply,
    ) {
        let actor_config = self.actor_config.clone();
        let key = self.key.clone();
        // Waiting for worker control acknowledgements must not hold the actor
        // mailbox or block cancellation/subscriptions on the child session.
        tokio::spawn(async move {
            let result = async {
                let config = prompt_config_for_agent_session_from_global(
                    &actor_config.base_config,
                    &key,
                    actor_config.call_fn.is_some(),
                )
                .await;
                let session = open_actor_nats_session(
                    &config,
                    &actor_config.local_worker,
                    key,
                    create_abort_signal(),
                )
                .await?;
                session
                    .decide_hitl_approval_for(&request, approved, note)
                    .await
            }
            .await
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = reply.send(result);
        });
    }
}
