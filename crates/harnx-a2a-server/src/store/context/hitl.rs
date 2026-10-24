//! HITL checkpoint state and store helpers for suspension/resume/reply.
//!
//! The active context document carries HITL state for a parked InputRequired task,
//! surviving context owner release and usable for recovery. Reply receipts bind
//! accepted replies to exact ApprovalRef identities for CAS-fenced delivery.
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};
use serde::{Deserialize, Serialize};

/// Maximum inline note length for the context payload ceiling.
pub const NOTE_BYTE_LIMIT: usize = 4 * 1024;

/// HITL checkpoint on the active task. Kept after the task moves back to Working
/// so a resumed execution isn't mistaken for an ordinary orphan.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct HitlCheckpoint {
    /// The exact approval generation for this checkpoint.
    pub request: ApprovalRef,
    /// Current phase of the checkpoint lifecycle.
    pub phase: HitlPhase,
    /// Accepted reply intent bound to the exact ApprovalRef.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyReceipt>,
    /// Observed log sequence at checkpoint creation time.
    #[serde(default)]
    pub observed_log_seq: u64,
}

impl Default for HitlCheckpoint {
    fn default() -> Self {
        Self {
            request: ApprovalRef {
                invocation_id: String::new(),
                tool_round_seq: 0,
                request_seq: 0,
                tool_call_id: String::new(),
            },
            phase: HitlPhase::default(),
            reply: None,
            observed_log_seq: 0,
        }
    }
}

/// Phase of the HITL checkpoint lifecycle.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HitlPhase {
    /// Checkpoint created for suspended InputRequired task.
    #[default]
    Waiting,
    /// Reply accepted and bound to exact ApprovalRef, awaiting delivery.
    DecisionIntent,
    /// Decision delivered, follower resuming.
    Following,
}

/// Accepted reply intent bound to an exact ApprovalRef.
///
/// The inline receipt is only cleared after the reply messageId dedupe mapping
/// is durable. Accepted reply mappings survive task retirement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyReceipt {
    /// MessageId of the reply message.
    pub message_id: String,
    /// Fingerprint of the reply message content.
    pub fingerprint: String,
    /// The exact approval generation this reply resolves.
    pub request: ApprovalRef,
    /// Whether the reply approves or rejects the approval.
    pub approved: bool,
    /// User-provided note (approval notes are audit-only, rejection notes are
    /// model-visible). Truncated to NOTE_BYTE_LIMIT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Outcome of decide_hitl_approval_for (stored for recovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

impl ReplyReceipt {
    /// Create a new reply receipt, truncating the note if necessary.
    pub fn new(
        message_id: String,
        fingerprint: String,
        request: ApprovalRef,
        approved: bool,
        note: Option<String>,
    ) -> Self {
        Self {
            message_id,
            fingerprint,
            request,
            approved,
            note: note.map(|n| truncate_note(&n)),
            outcome: None,
        }
    }

    /// Record the decision outcome for recovery purposes.
    pub fn with_outcome(mut self, outcome: &HitlDecisionOutcome) -> Self {
        self.outcome = match outcome {
            HitlDecisionOutcome::Applied => Some("applied".to_string()),
            HitlDecisionOutcome::AlreadyApplied { .. } => Some("already_applied".to_string()),
            HitlDecisionOutcome::Conflicting => Some("conflicting".to_string()),
            HitlDecisionOutcome::Stale => Some("stale".to_string()),
        };
        self
    }
}

/// Truncate a note to fit within NOTE_BYTE_LIMIT, respecting UTF-8 boundaries.
pub(super) fn truncate_note(note: &str) -> String {
    if note.len() <= NOTE_BYTE_LIMIT {
        return note.to_string();
    }
    // Find a valid UTF-8 boundary
    let mut end = NOTE_BYTE_LIMIT;
    while end > 0 && !note.is_char_boundary(end) {
        end -= 1;
    }
    note[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_notes() {
        let long = "x".repeat(5000);
        let truncated = truncate_note(&long);
        assert_eq!(truncated.len(), NOTE_BYTE_LIMIT);
    }

    #[test]
    fn preserves_short_notes() {
        let short = "short note";
        assert_eq!(truncate_note(short), short);
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let emoji = "🎉".repeat(2000);
        assert!(emoji.len() > NOTE_BYTE_LIMIT);
        let truncated = truncate_note(&emoji);
        // Should not panic and should be valid UTF-8
        let _ = truncated.chars().count();
        assert!(truncated.len() <= NOTE_BYTE_LIMIT);
    }

    #[test]
    fn checkpoint_serialization_roundtrip() {
        let checkpoint = HitlCheckpoint {
            request: ApprovalRef {
                invocation_id: "inv-123".into(),
                tool_round_seq: 5,
                request_seq: 10,
                tool_call_id: "call-1".into(),
            },
            phase: HitlPhase::Waiting,
            reply: None,
            observed_log_seq: 42,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let parsed: HitlCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, checkpoint);
    }

    #[test]
    fn checkpoint_with_reply_serialization() {
        let checkpoint = HitlCheckpoint {
            request: ApprovalRef {
                invocation_id: "inv-123".into(),
                tool_round_seq: 5,
                request_seq: 10,
                tool_call_id: "call-1".into(),
            },
            phase: HitlPhase::DecisionIntent,
            reply: Some(ReplyReceipt::new(
                "msg-1".into(),
                "fp-111".into(),
                ApprovalRef {
                    invocation_id: "inv-123".into(),
                    tool_round_seq: 5,
                    request_seq: 10,
                    tool_call_id: "call-1".into(),
                },
                true,
                Some("approval note".into()),
            )),
            observed_log_seq: 43,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let parsed: HitlCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, checkpoint);
    }

    #[test]
    fn old_documents_without_checkpoint_deserialize() {
        // Minimal ActiveTask without hitl_checkpoint field
        #[derive(Serialize, Deserialize)]
        struct OldActiveTask {
            #[serde(default)]
            stop_confirmed: bool,
            #[serde(default)]
            hitl_checkpoint: Option<HitlCheckpoint>,
        }
        let json = r#"{"stop_confirmed":false}"#;
        let parsed: OldActiveTask = serde_json::from_str(json).unwrap();
        assert!(!parsed.stop_confirmed);
        assert!(parsed.hitl_checkpoint.is_none());
    }

    #[test]
    fn reply_receipt_outcome_serializes() {
        let mut receipt = ReplyReceipt::new(
            "msg-1".into(),
            "fp-111".into(),
            ApprovalRef {
                invocation_id: "inv-123".into(),
                tool_round_seq: 5,
                request_seq: 10,
                tool_call_id: "call-1".into(),
            },
            true,
            None,
        );
        receipt = receipt.with_outcome(&HitlDecisionOutcome::Applied);
        assert_eq!(receipt.outcome, Some("applied".to_string()));

        receipt.outcome = None;
        receipt = receipt.with_outcome(&HitlDecisionOutcome::AlreadyApplied {
            approved: true,
            note: None,
        });
        assert_eq!(receipt.outcome, Some("already_applied".to_string()));
    }
}
