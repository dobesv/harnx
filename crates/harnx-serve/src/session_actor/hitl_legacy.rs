use super::{HitlApprovalFinished, SessionActor};

impl SessionActor {
    pub(super) fn answer_hitl_legacy(
        &self,
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<bool, String>>,
    ) {
        let actor_config = self.actor_config.clone();
        let key = self.key.clone();
        let done_tx = self.hitl_approval_done_tx.clone();
        tokio::spawn(async move {
            let result =
                Self::route_hitl_approval_decision(actor_config, key, tool_call_id, approved, note)
                    .await
                    .map_err(|error| format!("{error:#}"));
            let _ = done_tx.send(HitlApprovalFinished { result, reply }).await;
        });
    }
}
