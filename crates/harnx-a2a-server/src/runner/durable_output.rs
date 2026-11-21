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

fn is_raw_terminator(entry: &SessionLogEntry, prompt: u64) -> bool {
    match entry {
        SessionLogEntry::TurnEnd { through_seq, .. } => *through_seq >= prompt,
        SessionLogEntry::Cancel { .. } | SessionLogEntry::Error { .. } => true,
        _ => false,
    }
}

fn invocation_end_seq(
    entries: &[(u64, SessionLogEntry)],
    prompt: u64,
    through: Option<u64>,
) -> u64 {
    let cap = through.unwrap_or(u64::MAX);
    // A live steered drain (issue #2774): rows past the prompt's first
    // terminator belong to this invocation and are owed continuation-round
    // answers, so the bound is the caller's own reconciliation point (the
    // approval challenge sequence), not the first terminator — otherwise an
    // InputRequired reconciliation would drop the continuation's assistant
    // text before the challenge. While the drain is live no later
    // invocation can exist (see the invariant on steered_rows_outstanding),
    // so the caller's bound cannot leak another invocation's rows. A read
    // failure falls back to the conservative first-terminator bound.
    if harnx_runtime::nats_session::steered_rows_outstanding(entries, prompt).unwrap_or(false) {
        return cap;
    }
    let end = entries
        .iter()
        .find_map(|(seq, entry)| {
            (*seq > prompt && is_raw_terminator(entry, prompt)).then_some(*seq)
        })
        .unwrap_or(u64::MAX);
    // Steered continuation rounds (issue #2774) extend the invocation past
    // its first terminator; durable output reconciles through the extended
    // bound so the recovered text includes the steered answers.
    let end = harnx_runtime::nats_session::steered_terminal_seq(entries, prompt).unwrap_or(end);
    end.min(cap)
}

fn extract_assistant_text(entry: &SessionLogEntry) -> Option<String> {
    match entry {
        SessionLogEntry::ToolCalls { text, .. } => Some(text.clone()),
        SessionLogEntry::Message { role, content, .. } if role.is_assistant() => {
            Some(content.to_text())
        }
        _ => None,
    }
}

fn invocation_text(
    entries: &[(u64, SessionLogEntry)],
    prompt: u64,
    through: Option<u64>,
) -> Result<String> {
    let end = invocation_end_seq(entries, prompt, through);
    let scoped: Vec<_> = entries
        .iter()
        .filter(|(seq, _)| *seq > prompt && *seq <= end)
        .cloned()
        .collect();
    let effective = apply_log_mutations_nats(&scoped)?;
    Ok(effective
        .iter()
        .filter_map(|(_, entry)| extract_assistant_text(entry))
        .collect())
}

pub(super) async fn load_invocation_text(
    store: &A2aStore,
    storage: &str,
    prompt: u64,
    through: Option<u64>,
) -> Result<String> {
    let entries = NatsSessionLog::new_with_replicas(
        store.metadata().jetstream().clone(),
        storage,
        store.metadata().replicas(),
    )
    .load_events_latest_required_async()
    .await?;
    invocation_text(&entries, prompt, through)
}

impl Publisher {
    pub(super) async fn reconcile_output(&mut self, through: Option<u64>) -> Result<()> {
        let text = load_invocation_text(
            &self.store,
            &self.storage_key,
            self.record().user_msg_seq,
            through,
        )
        .await?;
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

    fn steered_user(id: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: Some(id.into()),
            role: MessageRole::User,
            content: MessageContent::Text("steered".into()),
            timestamp: None,
            fence_token: None,
        }
    }

    fn turn_end(through_seq: u64) -> SessionLogEntry {
        SessionLogEntry::TurnEnd {
            through_seq,
            fence_token: 1,
            timestamp: None,
            usage: None,
        }
    }

    #[test]
    fn live_drain_reconcile_includes_continuation_text_up_to_challenge() -> Result<()> {
        harnx_core::require_nextest();
        // The continuation round raises its approval challenge before any
        // TurnEnd covers the steered row: the drain is live, so the bound is
        // the challenge sequence, not the root round's first terminator.
        let entries = vec![
            (1, assistant("old")),
            (3, assistant("root answer")),
            (4, turn_end(2)),
            (5, steered_user("steered")),
            (6, assistant("continuation text")),
        ];
        assert_eq!(
            invocation_text(&entries, 2, Some(6))?,
            "root answercontinuation text"
        );
        // Once a covering TurnEnd settles the drain, the extended bound
        // still includes the steered answer, and rows after the abnormal
        // terminator — the next invocation's — stay excluded.
        let mut drained = entries.clone();
        drained.push((7, turn_end(5)));
        drained.push((
            9,
            SessionLogEntry::Cancel {
                fence_token: 0,
                cancellation_id: None,
                requested_by: None,
                timestamp: None,
            },
        ));
        drained.push((10, assistant("after")));
        assert_eq!(
            invocation_text(&drained, 2, None)?,
            "root answercontinuation text"
        );
        assert_eq!(
            invocation_text(&entries, 2, None)?,
            "root answercontinuation text"
        );
        Ok(())
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
