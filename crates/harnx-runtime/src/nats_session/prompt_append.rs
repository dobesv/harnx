//! Ordinary interactive prompt appends may follow a moving tail; fixed tickets cannot.
use super::{invocation_terminal_seq, NatsSession};
use crate::nats_session_log::{FencedAppend, NatsSessionLog};
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;

/// The bound invocation's turn already ended before this prompt's message
/// was appended — a steering join raced the turn's `TurnEnd` (CodeRabbit O1).
/// The refusal stays an error for callers without an in-call recovery path;
/// inherited steering callers re-decide the admission when they catch it.
#[derive(Debug)]
pub(super) struct LateSteeringRefusal;

impl std::fmt::Display for LateSteeringRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session admission already completed; cannot append late steering"
        )
    }
}

impl std::error::Error for LateSteeringRefusal {}

impl NatsSession {
    /// Retry at the new tail on every conflict; a conflict carrying our own
    /// message id is our own append whose ack was lost, not a rejection.
    ///
    /// An append requires the prompt's admission binding to exist first, and
    /// refuses once the bound invocation is terminal. That bind-before-append
    /// order is what keeps the void-join release proof (`join_is_void` in
    /// `nats_session/dispatch.rs`) stable: a prompt proven "terminal turn +
    /// message absent" can never gain a message afterwards.
    pub(super) async fn append_prompt_entry(
        &self,
        log: &NatsSessionLog,
        entry: &SessionLogEntry,
        message_id: &str,
    ) -> Result<u64> {
        let mut entries = log.load_events_latest_async().await?;
        let mut tail = entries.last().map_or(0, |(seq, _)| *seq);
        for _ in 0..16 {
            let admission = self
                .metadata_store
                .prompt_admission(&self.storage_key, message_id)
                .await?
                .context("prompt has no admission")?;
            anyhow::ensure!(
                admission.fixed_ticket.is_none(),
                "fixed admission requires fixed-ticket append"
            );
            anyhow::ensure!(
                !super::fixed_admission::admission_closed(
                    &entries,
                    admission.invocation_id.as_str(),
                    message_id
                ),
                "fixed admission closed without prompt; cannot replay through ordinary admission"
            );
            if let Some(seq) = self
                .metadata_store
                .invocation_prompt_seq(
                    &self.storage_key,
                    admission.invocation_id.as_str(),
                    &entries,
                )
                .await?
            {
                if invocation_terminal_seq(&entries, seq).is_some() {
                    // Typed (CodeRabbit O1) so an inherited steering caller
                    // can re-decide this proven-undeliverable join in-call;
                    // everyone else still surfaces the refusal.
                    return Err(LateSteeringRefusal.into());
                }
            }

            match log.append_fenced(entry, tail, message_id).await? {
                FencedAppend::Appended(seq) => return Ok(seq),
                FencedAppend::Conflict { entries } => {
                    if let Some((seq, _)) = entries.iter().find(|(_, e)| {
                        matches!(e, SessionLogEntry::Message { id: Some(id), .. } if id == message_id)
                    }) {
                        return Ok(*seq);
                    }
                    tail = entries.last().map_or(tail, |(seq, _)| *seq);
                    // Refresh full history before validating the same admission on retry.
                    // Conflict entries start after the previous tail.
                }
            }
            entries = log.load_events_latest_async().await?;
        }
        anyhow::bail!("session log tail kept moving; prompt not appended")
    }
}
