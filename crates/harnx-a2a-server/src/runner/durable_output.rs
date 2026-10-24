//! Advisory chunks aren't replayed on attach. Reconcile at durable boundaries,
//! after the sink drains, so history and live chunks cannot double-count output.
use super::*;
use a2a_lf::{Part, PartContent};
use harnx_core::{session::SessionLogEntry, session_reconstruct::apply_log_mutations_nats};
use harnx_runtime::nats_session_log::NatsSessionLog;

pub(super) fn part_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn invocation_text(
    entries: &[(u64, SessionLogEntry)],
    prompt: u64,
    through: Option<u64>,
) -> Result<String> {
    // Fixed admission prevents a successor before terminal proof. Still cap at
    // the first raw terminator: later invocations and rewinds can't extend it.
    let end = entries
        .iter()
        .find_map(|(seq, entry)| {
            (*seq > prompt
                && match entry {
                    SessionLogEntry::TurnEnd { through_seq, .. } => *through_seq >= prompt,
                    SessionLogEntry::Cancel { .. } | SessionLogEntry::Error { .. } => true,
                    _ => false,
                })
            .then_some(*seq)
        })
        .unwrap_or(u64::MAX)
        .min(through.unwrap_or(u64::MAX));
    let scoped: Vec<_> = entries
        .iter()
        .filter(|(seq, _)| *seq > prompt && *seq <= end)
        .cloned()
        .collect();
    let effective = apply_log_mutations_nats(&scoped)?;
    Ok(effective
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::ToolCalls { text, .. } => Some(text.clone()),
            SessionLogEntry::Message { role, content, .. } if role.is_assistant() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect())
}

impl Publisher {
    pub(super) async fn reconcile_output(&mut self, through: Option<u64>) -> Result<()> {
        let entries = NatsSessionLog::new_with_replicas(
            self.store.metadata().jetstream().clone(),
            &self.storage_key,
            self.store.metadata().replicas(),
        )
        .load_events_latest_required_async()
        .await?;
        let text = invocation_text(&entries, self.record().user_msg_seq, through)?;
        let limit = super::limits::task_output_limit(&self.store, &self.authority.context).await?;
        if text != self.output.text {
            let mut recovered = Output::default();
            recovered.accept(text, limit)?;
            recovered.sent = self.output.sent;
            self.output = recovered;
            self.replace_output = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{MessageContent, MessageRole};

    fn assistant(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.into()),
            timestamp: None,
            fence_token: None,
        }
    }

    #[test]
    fn resume_output_recovers_rounds_and_excludes_other_invocations() -> Result<()> {
        harnx_core::require_nextest();
        let mut entries = vec![
            (1, assistant("old")),
            (
                3,
                SessionLogEntry::ToolCalls {
                    text: "before approval".into(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (5, assistant("after approval")),
        ];
        // A raw Cancel is a terminator even when later history exists.
        entries.push((
            6,
            SessionLogEntry::Cancel {
                fence_token: 0,
                cancellation_id: None,
                requested_by: None,
                timestamp: None,
            },
        ));
        entries.push((8, assistant("next invocation")));
        assert_eq!(invocation_text(&entries, 2, Some(4))?, "before approval");
        assert_eq!(
            invocation_text(&entries, 2, None)?,
            "before approvalafter approval"
        );
        Ok(())
    }
}
