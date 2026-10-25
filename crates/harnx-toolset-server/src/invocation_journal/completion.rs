//! Bound only the terminal reply, then commit it through the existing row CAS.
use super::*;
use crate::reply_limits::{is_size_error, PendingReply, ReplyAttachments};
use crate::tool_observation::finalize_journal_reply;

impl InvocationJournal {
    /// The first terminal reply wins. Measure the complete row and wire reply
    /// before writing; neither the cache nor transport may change that winner.
    pub async fn complete(&self, request: &ToolRequest, reply: ToolReply) -> Result<ToolReply> {
        self.complete_checked(request, reply, false).await
    }

    /// NATS server completions include capability filtering and attestation in
    /// the committed bytes. Raw journal clients retain the existing metadata
    /// contract; the worker also reads those rows directly during recovery.
    pub(crate) async fn complete_for_delivery(
        &self,
        request: &ToolRequest,
        reply: ToolReply,
    ) -> Result<ToolReply> {
        self.complete_checked(request, reply, true).await
    }

    async fn complete_checked(
        &self,
        request: &ToolRequest,
        reply: ToolReply,
        for_delivery: bool,
    ) -> Result<ToolReply> {
        self.validate_replay(request).await?;
        ensure!(reply.call_id == request.call_id, "reply call ID mismatch");
        let result = self.complete_bounded(request, reply, for_delivery).await;
        if result.is_err() {
            // A rival may have committed while attachment storage or our write
            // failed. Its durable completion still outranks our local outcome.
            // A failed recheck must not hide the original durability failure.
            let winner = if for_delivery {
                self.completed_reply_for_delivery(request).await
            } else {
                self.completed_reply(request).await
            };
            if let Ok(Some(winner)) = winner {
                return Ok(winner);
            }
        }
        result
    }

    async fn complete_bounded(
        &self,
        request: &ToolRequest,
        reply: ToolReply,
        for_delivery: bool,
    ) -> Result<ToolReply> {
        let key = key(request);
        let mut raw = Some(reply);
        let mut pending = None;
        let mut limits = Some(self.0.stream.cached_info().config.clone());
        let mut size_retry = false;
        loop {
            let entry = self
                .entry(&key)
                .await?
                .context("durable tool invocation missing")?;
            ensure!(
                entry.operation == kv::Operation::Put,
                "tool invocation was deleted"
            );
            let mut record: RecordedInvocation = serde_json::from_slice(&entry.value)?;
            if record.reply.is_some() {
                return Ok(recorded_reply(record, for_delivery).expect("reply was present"));
            }
            if let Some(reply) = raw.take() {
                pending = Some(prepare_pending_reply(reply, &record, for_delivery)?);
            }
            if limits.is_none() {
                limits = Some(self.0.status().await?.info.config);
            }
            let config = limits.as_ref().expect("limits loaded");
            let budget = CompletionBudget::for_record(
                &record,
                entry.revision,
                config,
                self.1.client().server_info().max_payload,
            )?;
            let attachments = ReplyAttachments::new(request, &self.1, config.num_replicas);
            let reply = pending
                .as_mut()
                .expect("pending reply initialized")
                .bounded(&attachments, budget.reply_bytes, size_retry)
                .await?;
            record.reply = Some(reply.clone());
            let payload = budget.encode(&record)?;
            let update = self.0.update(&key, payload.into(), entry.revision).await;
            match CompletionUpdate::classify(update, size_retry)? {
                CompletionUpdate::Saved => return Ok(reply),
                CompletionUpdate::Conflict => continue,
                CompletionUpdate::SizeLimit => {
                    // A changed KV/broker limit can invalidate the preflight.
                    // Retry a durable size outcome, never the tool execution.
                    size_retry = true;
                    limits = None;
                }
            }
        }
    }
}

/// Only a CAS conflict or the first typed size failure permits another write.
#[derive(Debug, PartialEq, Eq)]
enum CompletionUpdate {
    Saved,
    Conflict,
    SizeLimit,
}

impl CompletionUpdate {
    fn classify(update: Result<u64, kv::UpdateError>, size_retry: bool) -> Result<Self> {
        let error = match update {
            Ok(_) => return Ok(Self::Saved),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                return Ok(Self::Conflict);
            }
            Err(error) => anyhow::Error::from(error),
        };
        if !size_retry && is_size_error(&error) {
            return Ok(Self::SizeLimit);
        }
        Err(error).context("persist terminal tool invocation reply")
    }
}

fn prepare_pending_reply(
    mut reply: ToolReply,
    record: &RecordedInvocation,
    for_delivery: bool,
) -> Result<PendingReply> {
    let original = serde_json::to_vec(&reply).context("encode original tool reply")?;
    // The timestamp and private metadata are part of both budgets.
    // Attachments retain the raw output, not the attested projection.
    if for_delivery {
        finalize_journal_reply(record, &mut reply, false);
    }
    PendingReply::new(reply, original)
}

pub(super) fn recorded_reply(
    mut record: RecordedInvocation,
    for_delivery: bool,
) -> Option<ToolReply> {
    let mut reply = record.reply.take()?;
    // Old rows contain raw handler metadata. Sanitize those as before, but do
    // not re-stamp a reply already attested before commit by this implementation.
    if for_delivery {
        finalize_journal_reply(&record, &mut reply, true);
    }
    Some(reply)
}

/// A single CAS attempt's terminal headroom and full-row ceiling.
struct CompletionBudget {
    reply_bytes: usize,
    row_bytes: usize,
    headers: usize,
}

impl CompletionBudget {
    fn for_record(
        record: &RecordedInvocation,
        revision: u64,
        config: &jetstream::stream::Config,
        wire_limit: usize,
    ) -> Result<Self> {
        ensure!(
            wire_limit > 0,
            "broker advertised no tool reply payload budget"
        );
        let row_bytes = if config.max_message_size > 0 {
            wire_limit.min(config.max_message_size as usize)
        } else {
            wire_limit
        };
        // `reply: null` occupies four bytes. Everything else, including
        // inputs, checkpoints and partial results, remains byte-for-byte data.
        let overhead = serde_json::to_vec(record)?.len() - 4;
        let headers = cas_header_bytes(revision);
        Ok(Self {
            reply_bytes: wire_limit.min(row_bytes.saturating_sub(overhead + headers)),
            row_bytes,
            headers,
        })
    }

    fn encode(&self, record: &RecordedInvocation) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(record)?;
        ensure!(
            payload.len() + self.headers <= self.row_bytes,
            "bounded tool invocation row exceeds payload budget"
        );
        Ok(payload)
    }
}

fn cas_header_bytes(revision: u64) -> usize {
    // async-nats kv::Store::update adds this one header. NATS counts the
    // header block against max_payload AND the stream's max_message_size.
    "NATS/1.0\r\nNats-Expected-Last-Subject-Sequence: \r\n\r\n".len() + revision.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_header_budget_includes_revision_growth() {
        assert_eq!(cas_header_bytes(1), 52);
        assert_eq!(cas_header_bytes(10), 53);
        assert_eq!(cas_header_bytes(u64::MAX), 71);
    }

    #[test]
    fn completion_update_keeps_conflicts_retryable_after_a_size_failure() {
        for size_retry in [false, true] {
            assert_eq!(
                CompletionUpdate::classify(Ok(1), size_retry).unwrap(),
                CompletionUpdate::Saved
            );
            let conflict = kv::UpdateError::new(kv::UpdateErrorKind::WrongLastRevision);
            assert_eq!(
                CompletionUpdate::classify(Err(conflict), size_retry).unwrap(),
                CompletionUpdate::Conflict
            );
        }
    }

    #[test]
    fn completion_update_size_recovery_is_single_attempt() {
        use jetstream::context::{PublishError, PublishErrorKind};
        let rejected = || {
            Err(kv::UpdateError::from(PublishError::new(
                PublishErrorKind::MaxPayloadExceeded,
            )))
        };
        assert_eq!(
            CompletionUpdate::classify(rejected(), false).unwrap(),
            CompletionUpdate::SizeLimit
        );
        let error = CompletionUpdate::classify(rejected(), true).unwrap_err();
        assert!(is_size_error(&error), "typed cause must remain available");
        assert_eq!(error.to_string(), "persist terminal tool invocation reply");
    }

    #[test]
    fn completion_update_non_size_failures_are_never_retried() {
        use jetstream::context::{PublishError, PublishErrorKind};
        for size_retry in [false, true] {
            let rejected = kv::UpdateError::from(PublishError::new(PublishErrorKind::TimedOut));
            let error = CompletionUpdate::classify(Err(rejected), size_retry).unwrap_err();
            assert!(!is_size_error(&error));
            assert_eq!(error.to_string(), "persist terminal tool invocation reply");
            // async-nats converts a publish timeout into a source-less KV timeout.
            assert_eq!(
                error.downcast_ref::<kv::UpdateError>().unwrap().kind(),
                kv::UpdateErrorKind::TimedOut
            );
        }
    }
}
