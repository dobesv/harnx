//! Durable activity has its own origin; a new gate head cannot relabel old TurnEnd.
use super::*;
use harnx_core::session::SessionLogEntry;

impl SessionEventStream {
    /// Generation that admitted the latest effective user row. Unknown legacy
    /// ownership remains history-only, never a live busy/idle transition.
    pub async fn history_generation(&self) -> Result<Option<String>> {
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&self.history)?;
        let Some((seq, id)) = effective.iter().rev().find_map(|(seq, entry)| match entry {
            SessionLogEntry::Message { id, role, .. } if role.is_user() => {
                Some((*seq, id.as_deref()))
            }
            _ => None,
        }) else {
            return Ok(None);
        };
        let store = harnx_execution_control::ExecutionStore::from_store(
            self.jetstream
                .get_key_value(harnx_execution_control::BUCKET)
                .await?,
        );
        let history = store.recovery_history(&self.session_id).await?;
        Ok(
            crate::nats_session_log::recovery::prompt_owner(&history, id, seq)?
                .map(|owner| owner.reference.execution_id.clone()),
        )
    }
}
