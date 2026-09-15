//! Private conditional sink for committed decisions. Message IDs are read from
//! durable stream headers; the broker's finite deduplication window is not proof.
use super::*;
use std::collections::HashMap;

pub(crate) struct ProjectionSnapshot {
    pub tail: u64,
    committed: HashMap<String, (u64, Vec<u8>)>,
}

impl ProjectionSnapshot {
    pub(crate) fn sequence(&self, id: &str) -> Option<u64> {
        self.committed.get(id).map(|(seq, _)| *seq)
    }
}

impl NatsSessionLog {
    pub(crate) async fn projection_snapshot(&self) -> Result<ProjectionSnapshot> {
        let stream = self.ensure_stream().await?;
        let tail = match stream.get_last_raw_message_by_subject(&self.subject).await {
            Ok(raw) => raw.sequence,
            Err(error) if error.kind() == LastRawMessageErrorKind::NoMessageFound => 0,
            Err(error) => return Err(error.into()),
        };
        let mut committed = HashMap::new();
        for seq in 1..=tail {
            // Session streams retain committed entries until session deletion.
            // Missing evidence fails closed rather than duplicating an append.
            let raw = stream.get_raw_message(seq).await?;
            if let Some(id) = raw.headers.get(async_nats::header::NATS_MESSAGE_ID) {
                committed.insert(id.to_string(), (seq, raw.payload.to_vec()));
            }
        }
        Ok(ProjectionSnapshot { tail, committed })
    }

    async fn project_entry(
        &self,
        id: &str,
        entry: &SessionLogEntry,
        expected: Option<u64>,
    ) -> Result<Option<u64>> {
        loop {
            let snapshot = self.projection_snapshot().await?;
            if let Some((seq, payload)) = snapshot.committed.get(id) {
                // HashMap key order can change between projectors; content must not.
                anyhow::ensure!(
                    serde_json::to_value(deserialize_entry(payload)?)?
                        == serde_json::to_value(entry)?,
                    "projection commit ID has different transcript content"
                );
                return Ok(Some(*seq));
            }
            if expected.is_some_and(|expected| expected != snapshot.tail) {
                return Ok(None);
            }
            match self
                .append_event_with_expected_last_sequence_and_message_id_async(
                    entry,
                    snapshot.tail,
                    id,
                )
                .await
            {
                Ok(seq) => return Ok(Some(seq)),
                Err(error) if is_sequence_conflict(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

fn is_sequence_conflict(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<jetstream::context::PublishError>()
            .is_some_and(|error| {
                error.kind() == jetstream::context::PublishErrorKind::WrongLastSequence
            })
    })
}

use crate::execution_fence::GenerationFence;
use harnx_execution_control::{
    CommitAction, CommitReceipt, CommittedAction, CommittedDecision, GateAction, OutputKind,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct TranscriptOutput {
    entry: SessionLogEntry,
    expected_tail: Option<u64>,
}

impl NatsSessionLog {
    pub(crate) async fn append_output(
        &self,
        fence: &GenerationFence,
        entry: &SessionLogEntry,
        expected_tail: Option<u64>,
    ) -> Result<Option<u64>> {
        anyhow::ensure!(
            fence.context.generation().session_id == self.session_id,
            "transcript generation session mismatch"
        );
        anyhow::ensure!(
            !matches!(entry, SessionLogEntry::Cancel { .. }),
            "Cancel is control, not output"
        );
        let receipt = fence
            .output(
                OutputKind::Transcript,
                serde_json::to_value(TranscriptOutput {
                    entry: entry.clone(),
                    expected_tail,
                })?,
            )
            .await?;
        self.project_through(fence, &receipt).await
    }

    pub(crate) async fn append_cancellation(
        &self,
        fence: &GenerationFence,
        coverage: (u64, u64),
    ) -> Result<Option<u64>> {
        let (through_seq, fence_token) = coverage;
        // G2 can reserve/append input before its worker installs the gate member.
        // Gate-current G1 alone cannot authorize a new Cancel over that input.
        // Ownership validates this snapshot; expected-tail CAS fences later appends.
        let entries = self.load_events_latest_async().await?;
        self.validate_cancel_ownership(&fence.store, fence.context.generation(), &entries)
            .await?;
        anyhow::ensure!(
            fence.context.generation().session_id == self.session_id,
            "cancel generation session mismatch"
        );
        let receipt = fence
            .store
            .commit_if_admissible(
                &fence.context,
                CommitAction {
                    id: format!("cancel-coverage-{through_seq}-{fence_token}"),
                    kind: GateAction::RecordCancellation {
                        through_seq,
                        fence_token,
                    },
                },
            )
            .await?;
        self.project_through(fence, &receipt).await
    }

    /// Drain in gate order, including older generations. G2 cannot append ahead
    /// of G1's committed output/Cancel, even if G1's projector crashed or paused.
    pub(crate) async fn project_through(
        &self,
        fence: &GenerationFence,
        receipt: &CommitReceipt,
    ) -> Result<Option<u64>> {
        let target = fence.store.committed_decision(receipt).await?;
        anyhow::ensure!(
            receipt.gate_root == *fence.context.gate_root(),
            "projector gate mismatch"
        );
        let projector = format!("transcript-{}", crate::utils::sha256(&self.session_id));
        while !Box::pin(self.project_step(fence, &target, &projector)).await? {}
        Ok(self
            .projection_snapshot()
            .await?
            .sequence(&projection_id(receipt)))
    }

    async fn project_step(
        &self,
        fence: &GenerationFence,
        target: &CommittedDecision,
        projector: &str,
    ) -> Result<bool> {
        let receipt = &target.receipt;
        let cursor = fence
            .store
            .projection_cursor(&receipt.gate_root, projector)
            .await?;
        let start = cursor.as_ref().map_or(1, |cursor| cursor.sequence + 1);
        if start > receipt.sequence {
            return Ok(true);
        }
        for sequence in start..=receipt.sequence {
            let decision = fence
                .store
                .gate_decision_at(&receipt.gate_root, sequence)
                .await?;
            Box::pin(self.project_decision(fence, &decision)).await?;
        }
        advance_cursor(&fence.store, target, (projector, cursor)).await
    }

    async fn project_decision(
        &self,
        fence: &GenerationFence,
        decision: &CommittedDecision,
    ) -> Result<()> {
        if decision.context.generation().session_id != self.session_id {
            return Ok(());
        }
        let CommittedAction::Action { action } = &decision.action else {
            return Ok(());
        };
        let output = match &action.kind {
            GateAction::CommitOutput { output } if output.kind == OutputKind::Transcript => {
                let payload = fence
                    .store
                    .committed_output_payload(&decision.receipt)
                    .await?;
                let output: TranscriptOutput = serde_json::from_value(payload)?;
                anyhow::ensure!(
                    !matches!(output.entry, SessionLogEntry::Cancel { .. }),
                    "Cancel requires control projection"
                );
                output
            }
            GateAction::RecordCancellation {
                through_seq,
                fence_token,
            } => TranscriptOutput {
                entry: SessionLogEntry::Cancel {
                    fence_token: *fence_token,
                },
                // A control boundary for G must not cover a prompt appended by G2
                // after acceptance but before this projector ran.
                expected_tail: Some(*through_seq),
            },
            GateAction::CommitOutput { output } if output.kind == OutputKind::SessionMetadata => {
                let store = crate::nats_session_metadata::SessionMetadataStore::from_store(
                    self.jetstream
                        .get_key_value(crate::nats_session_metadata::SESSION_METADATA_BUCKET)
                        .await?,
                    self.jetstream.client().clone(),
                );
                store.project_output(&fence.store, decision).await?;
                return Ok(());
            }
            _ => return Ok(()),
        };
        self.project_entry(
            &projection_id(&decision.receipt),
            &output.entry,
            output.expected_tail,
        )
        .await?;
        Ok(())
    }
}

async fn advance_cursor(
    store: &harnx_execution_control::ExecutionStore,
    target: &CommittedDecision,
    cursor: (&str, Option<CommitReceipt>),
) -> Result<bool> {
    let (projector, expected_cursor) = cursor;
    let receipt = &target.receipt;
    let action = CommitAction {
        id: format!("{projector}-{}", receipt.commit_id),
        kind: GateAction::ProjectCommitted {
            commit: receipt.clone(),
            projector: projector.into(),
            expected_cursor: expected_cursor.clone(),
        },
    };
    match store.commit_if_admissible(&target.context, action).await {
        Ok(_) => Ok(true),
        Err(error) => {
            let advanced = store
                .projection_cursor(&receipt.gate_root, projector)
                .await?;
            if advanced == expected_cursor {
                return Err(error);
            }
            Ok(false)
        }
    }
}

fn projection_id(receipt: &CommitReceipt) -> String {
    format!("gate-{}", receipt.commit_id)
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
