use super::{live::DurableRound, snapshot::CallCheckpoint};
use crate::{
    interactions::interaction_request_from_entry,
    middleware::{AcceptedInteraction, MiddlewareResume},
};
use anyhow::{ensure, Context, Result};
use harnx_core::{interaction::InteractionRequest, session::SessionLogEntry, tool::ToolCall};
use harnx_engine::tool::PendingToolInteraction;

type Entries = [(u64, SessionLogEntry)];

pub(super) struct ResumeCall {
    pub resume: MiddlewareResume,
    pub waiting: Option<InteractionRequest>,
    pub reference: String,
}

impl DurableRound {
    pub(super) async fn resume_call(&self, call: &ToolCall) -> Result<Option<ResumeCall>> {
        let entries = self.session.backend.load_events_latest_async().await?;
        let Some((request_seq, entry, reference)) = latest_request(&entries, self.seq, call) else {
            return Ok(None);
        };
        let request =
            interaction_request_from_entry(entry).context("missing latest interaction request")?;
        // Changed chains never receive old frame data or accepted answers.
        if request.chain_version != self.chain_version {
            return Ok(None);
        }
        let stored: CallCheckpoint = self.store.read(&self.prefix(call)?, reference).await?;
        let restored = stored.restore()?;
        let responses = accepted_responses(&entries, self.seq, call, &self.chain_version)?;
        let answered = entries.iter().any(|(_, e)| {
            matches!(e,
            SessionLogEntry::InteractionResponse { request_seq: s, .. } if *s == request_seq)
        });
        // A tool challenge (producer `tool:<server>/<tool>`) resumes with the
        // latest challenge's responses and state only, never earlier ones
        // (MRTR repeated-challenge rule).
        let tool_input = if answered && request.producer.starts_with("tool:") {
            responses
                .iter()
                .rev()
                .find(|accepted| accepted.request.interaction_id == request.interaction_id)
                .map(|accepted| crate::middleware::ToolInputResume {
                    responses: std::collections::BTreeMap::from([(
                        request.key.clone(),
                        harnx_toolset::ToolInputResponse {
                            action: accepted.response.action.clone(),
                            data: accepted.response.data.clone(),
                            note: accepted.response.note.clone(),
                        },
                    )]),
                    request_state: crate::middleware::checkpoint_tool_request_state(&restored)
                        .unwrap_or(serde_json::Value::Null),
                })
        } else {
            None
        };
        Ok(Some(ResumeCall {
            resume: MiddlewareResume {
                checkpoint: restored,
                responses,
                tool_input,
            },
            waiting: (!answered).then_some(request),
            reference: reference.into(),
        }))
    }

    pub(super) async fn chain_changed_after_dispatch(&self, call: &ToolCall) -> Result<bool> {
        let entries = self.session.backend.load_events_latest_async().await?;
        let changed = latest_request(&entries, self.seq, call)
            .and_then(|(_, entry, _)| interaction_request_from_entry(entry))
            .is_some_and(|request| request.chain_version != self.chain_version);
        if !changed {
            return Ok(false);
        }
        self.store
            .call_has_dispatched(
                self.session.backend.session_id(),
                self.seq,
                call.id.as_deref().context("missing changed call ID")?,
            )
            .await
    }

    /// How many times this call has challenged so far (producer `tool:*`),
    /// counted from the journal. The current challenge is not journaled yet
    /// when this runs, so the count is the number of completed rounds; the
    /// MRTR round cap (SEP-2322) gives up once it is reached.
    pub(super) async fn tool_input_rounds(&self, tool_call_id: &str) -> Result<usize> {
        let entries = self.session.backend.load_events_latest_async().await?;
        Ok(entries
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    SessionLogEntry::InteractionRequested {
                        tool_call_id: id,
                        producer,
                        ..
                    } if id == tool_call_id && producer.starts_with("tool:")
                )
            })
            .count())
    }

    pub(super) async fn pending_reference(
        &self,
        pending: &PendingToolInteraction,
    ) -> Result<Option<String>> {
        let entries = self.session.backend.load_events_latest_async().await?;
        Ok(entries.iter().rev().find_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionRequested {
                tool_round_seq,
                interaction_id,
                checkpoint,
                ..
            } if *tool_round_seq == self.seq
                && interaction_id == &pending.interaction.interaction_id =>
            {
                Some(checkpoint.clone())
            }
            _ => None,
        }))
    }
}

fn latest_request<'a>(
    entries: &'a Entries,
    round: u64,
    call: &ToolCall,
) -> Option<(u64, &'a SessionLogEntry, &'a str)> {
    entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            checkpoint,
            ..
        } if *tool_round_seq == round && call.id.as_deref() == Some(tool_call_id) => {
            Some((*seq, entry, checkpoint.as_str()))
        }
        _ => None,
    })
}

fn accepted_responses(
    entries: &Entries,
    round: u64,
    call: &ToolCall,
    version: &str,
) -> Result<Vec<AcceptedInteraction>> {
    let mut responses = Vec::new();
    for (seq, entry) in entries {
        let SessionLogEntry::InteractionResponse {
            request_seq,
            interaction_id,
            tool_call_id,
            response,
            ..
        } = entry
        else {
            continue;
        };
        if call.id.as_deref() != Some(tool_call_id) {
            continue;
        }
        let Some((_, requested)) = entries.iter().find(|(s, _)| s == request_seq && s < seq) else {
            continue;
        };
        let SessionLogEntry::InteractionRequested { tool_round_seq, .. } = requested else {
            continue;
        };
        let Some(request) = interaction_request_from_entry(requested) else {
            continue;
        };
        if *tool_round_seq == round && request.chain_version == version {
            ensure!(
                request.interaction_id == *interaction_id,
                "accepted interaction identity mismatch"
            );
            responses.push(AcceptedInteraction {
                request,
                response: response.clone(),
            });
        }
    }
    Ok(responses)
}
