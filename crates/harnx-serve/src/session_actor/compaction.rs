use super::*;
use harnx_runtime::nats_session::CompactSubmit;

impl SessionActor {
    /// Handle a manual compaction request from the frontend.
    ///
    /// This submits the durable `CompactRequest` entry and announces it to
    /// workers via `ControlCommand::Compact` + `SessionActivate`. The actual
    /// compaction runs inside a worker that picks up the request.
    pub(super) async fn answer_compact(
        &self,
        reply: tokio::sync::oneshot::Sender<Result<CompactSubmit, String>>,
    ) {
        let result = self.submit_compaction().await;
        let _ = reply.send(result);
    }

    /// Submit a compaction request via the frontend session.
    ///
    /// The actor is a frontend context — it must submit the durable request +
    /// activation, NOT compact locally. The worker picks this up and runs
    /// the actual compaction.
    async fn submit_compaction(&self) -> Result<CompactSubmit, String> {
        // Injected test executor runs in-process: no NATS session.
        // Return a simulated response.
        if self.actor_config.call_fn.is_some() {
            return Ok(CompactSubmit::Submitted {
                compaction_id: uuid::Uuid::new_v4().to_string(),
            });
        }

        let session = self
            .control_session()
            .await
            .map_err(|error| format!("{error:#}"))?;
        session
            .request_compaction(Some("web".to_string()))
            .await
            .map_err(|error| format!("{error:#}"))
    }
}
