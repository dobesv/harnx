//! Keep the worker's synchronous middleware fallback out of the legacy permission UI.

use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::apply_log_mutations_nats;
use harnx_runtime::config::GlobalConfig;

#[derive(Clone)]
pub(crate) struct LegacyProbe {
    pub config: GlobalConfig,
    pub cluster: String,
    pub storage_key: String,
}

impl LegacyProbe {
    pub async fn is_interaction(&self, tool_call_id: Option<&str>) -> Result<bool> {
        let Some(tool_call_id) = tool_call_id else {
            return Ok(false);
        };
        let snapshot = self.config.read().clone();
        let jetstream = snapshot.nats_jetstream(&self.cluster).await?;
        let log =
            harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, &self.storage_key);
        let history = log.load_events_latest_async().await?;
        let effective = apply_log_mutations_nats(&history)?;
        Ok(owns_callback(&effective, tool_call_id))
    }
}

fn owns_callback(entries: &[(u64, SessionLogEntry)], tool_call_id: &str) -> bool {
    let round = entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::ToolCalls { calls, .. }
            if calls
                .iter()
                .any(|call| call.id.as_deref() == Some(tool_call_id)) =>
        {
            Some(*seq)
        }
        _ => None,
    });
    // Ownership survives acceptance: a fast durable answer must not open a second legacy dialog.
    entries.iter().any(|(_, entry)| {
        matches!(entry,
        SessionLogEntry::InteractionRequested { tool_round_seq, tool_call_id: id, .. }
            if Some(*tool_round_seq) == round && id == tool_call_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn middleware_owns_callback_after_answer_but_not_reused_id_in_new_round() -> Result<()> {
        let round: SessionLogEntry = serde_json::from_value(json!({
            "type":"tool_calls", "text":"", "calls":[{"id":"call-1","name":"run","arguments":{}}]
        }))?;
        let request = serde_json::from_value(json!({
            "type":"interaction_requested", "tool_round_seq":1,"tool_call_id":"call-1",
            "interaction_id":"confirm-1","producer":"middleware:approval","step_path":[],
            "key":"gate","kind":"confirm","payload":{},"summary":"Run?",
            "chain_version":"v1","checkpoint":"private","fence_token":1
        }))?;
        let answer = serde_json::from_value(json!({
            "type":"interaction_response","request_seq":2,"tool_call_id":"call-1",
            "interaction_id":"confirm-1","response":{"action":"approve","data":{}},"fence_token":1
        }))?;
        let mut entries = vec![(1, round.clone()), (2, request)];
        assert!(owns_callback(&entries, "call-1"));
        assert!(!owns_callback(&entries, "other-call"));
        entries.push((3, answer));
        assert!(owns_callback(&entries, "call-1"));
        entries.push((4, round));
        assert!(!owns_callback(&entries, "call-1"));
        Ok(())
    }
}
