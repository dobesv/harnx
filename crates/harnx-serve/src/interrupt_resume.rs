use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::session_actor::{
    InterruptResume, InterruptResumePayload, InterruptResumeStatus, SessionState,
};

#[derive(Debug, Deserialize)]
pub(crate) struct InterruptResumeParam {
    #[serde(rename = "interruptId", alias = "interrupt_id")]
    pub(crate) interrupt_id: String,
    pub(crate) status: String,
    pub(crate) payload: InterruptResumePayloadParam,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InterruptResumePayloadParam {
    pub(crate) approved: bool,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

pub(crate) fn parse_resume_params(
    params: &[InterruptResumeParam],
) -> anyhow::Result<Vec<InterruptResume>> {
    params
        .iter()
        .map(|param| {
            let status = match param.status.as_str() {
                "approved" | "resolved" if param.payload.approved => {
                    InterruptResumeStatus::Approved
                }
                "cancelled" | "denied" | "rejected" if !param.payload.approved => {
                    InterruptResumeStatus::Denied
                }
                other => {
                    anyhow::bail!(
                        "invalid resume status/payload for interrupt {}: status={}, approved={}",
                        param.interrupt_id,
                        other,
                        param.payload.approved
                    )
                }
            };
            Ok(InterruptResume {
                interrupt_id: param.interrupt_id.clone(),
                status,
                payload: InterruptResumePayload {
                    approved: param.payload.approved,
                    reason: param.payload.reason.clone(),
                },
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeValidationError {
    SessionNotInterrupted,
    UnknownInterruptIds(Vec<String>),
    MismatchedRunIds {
        expected_run_id: String,
        actual_run_ids: Vec<String>,
    },
    MissingInterruptIds(Vec<String>),
}

impl ResumeValidationError {
    pub(crate) fn details(&self) -> Value {
        match self {
            Self::SessionNotInterrupted => {
                json!({ "detail": "resume requires interrupted session state" })
            }
            Self::UnknownInterruptIds(invalid_ids) => json!({
                "detail": "resume interrupt ids do not match pending batch",
                "invalid_interrupt_ids": invalid_ids,
            }),
            Self::MismatchedRunIds {
                expected_run_id,
                actual_run_ids,
            } => json!({
                "detail": "resume run_id does not match interrupted run",
                "expected_run_id": expected_run_id,
                "actual_run_ids": actual_run_ids,
            }),
            Self::MissingInterruptIds(missing_ids) => json!({
                "detail": "resume decisions must cover every pending interrupt",
                "missing_interrupt_ids": missing_ids,
            }),
        }
    }
}

impl std::fmt::Display for ResumeValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            self.details()["detail"]
                .as_str()
                .expect("resume validation detail is a string"),
        )
    }
}

impl std::error::Error for ResumeValidationError {}

pub(crate) fn validate_resume<'a>(
    resume: &[InterruptResume],
    state: &'a SessionState,
) -> Result<&'a str, ResumeValidationError> {
    let SessionState::Interrupted {
        run_id, pending, ..
    } = state
    else {
        return Err(ResumeValidationError::SessionNotInterrupted);
    };

    let pending_ids: BTreeSet<&str> = pending
        .interrupts
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    let resume_ids: BTreeSet<&str> = resume
        .iter()
        .map(|entry| entry.interrupt_id.as_str())
        .collect();

    if !resume_ids.is_subset(&pending_ids) {
        let invalid_ids = resume
            .iter()
            .filter(|entry| !pending_ids.contains(entry.interrupt_id.as_str()))
            .map(|entry| entry.interrupt_id.clone())
            .collect();
        return Err(ResumeValidationError::UnknownInterruptIds(invalid_ids));
    }

    let mismatched_run_ids = resume
        .iter()
        .filter_map(|entry| {
            entry
                .interrupt_id
                .split(':')
                .next()
                .filter(|prefix| prefix.starts_with("run_") && *prefix != run_id)
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    if !mismatched_run_ids.is_empty() {
        return Err(ResumeValidationError::MismatchedRunIds {
            expected_run_id: run_id.clone(),
            actual_run_ids: mismatched_run_ids,
        });
    }

    if resume_ids != pending_ids {
        let missing_ids = pending
            .interrupts
            .iter()
            .filter(|entry| !resume_ids.contains(entry.id.as_str()))
            .map(|entry| entry.id.clone())
            .collect();
        return Err(ResumeValidationError::MissingInterruptIds(missing_ids));
    }

    Ok(&pending.text)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::session_actor::{PendingInterruptBatch, ToolApprovalInterruptEntry};

    fn resume_param(interrupt_id: &str, status: &str, approved: bool) -> InterruptResumeParam {
        InterruptResumeParam {
            interrupt_id: interrupt_id.to_string(),
            status: status.to_string(),
            payload: InterruptResumePayloadParam {
                approved,
                reason: None,
            },
        }
    }

    fn resume(interrupt_id: &str) -> InterruptResume {
        InterruptResume {
            interrupt_id: interrupt_id.to_string(),
            status: InterruptResumeStatus::Approved,
            payload: InterruptResumePayload {
                approved: true,
                reason: None,
            },
        }
    }

    fn interrupted_state(run_id: &str, interrupt_ids: &[&str]) -> SessionState {
        SessionState::Interrupted {
            run_id: run_id.to_string(),
            started_at: Utc::now(),
            pending: Box::new(PendingInterruptBatch {
                interrupt_run_id: run_id.to_string(),
                text: "original prompt".to_string(),
                attachment_refs: Vec::new(),
                completion_output: String::new(),
                completion_thought: None,
                tool_calls: Vec::new(),
                interrupts: interrupt_ids
                    .iter()
                    .map(|id| ToolApprovalInterruptEntry {
                        id: (*id).to_string(),
                        tool_call_id: (*id).to_string(),
                        name: "approval_tool".to_string(),
                        arguments: json!({}),
                        message: "Approve tool call".to_string(),
                        reason: None,
                    })
                    .collect(),
                metadata: Value::Null,
            }),
        }
    }

    #[test]
    fn parse_resume_params_maps_supported_status_payload_pairs() {
        let params = [
            resume_param("resolved-call", "resolved", true),
            resume_param("cancelled-call", "cancelled", false),
            resume_param("denied-call", "denied", false),
        ];

        let parsed = parse_resume_params(&params).expect("supported resume parameters");

        assert_eq!(
            parsed,
            vec![
                InterruptResume {
                    interrupt_id: "resolved-call".to_string(),
                    status: InterruptResumeStatus::Approved,
                    payload: InterruptResumePayload {
                        approved: true,
                        reason: None,
                    },
                },
                InterruptResume {
                    interrupt_id: "cancelled-call".to_string(),
                    status: InterruptResumeStatus::Denied,
                    payload: InterruptResumePayload {
                        approved: false,
                        reason: None,
                    },
                },
                InterruptResume {
                    interrupt_id: "denied-call".to_string(),
                    status: InterruptResumeStatus::Denied,
                    payload: InterruptResumePayload {
                        approved: false,
                        reason: None,
                    },
                },
            ]
        );
    }

    #[test]
    fn parse_resume_params_rejects_invalid_status_payload_pairs() {
        for (interrupt_id, status, approved) in [
            ("resolved-false", "resolved", false),
            ("unknown-status", "pending", true),
        ] {
            let params = [resume_param(interrupt_id, status, approved)];
            let error = parse_resume_params(&params).expect_err("invalid resume parameters");

            assert_eq!(
                error.to_string(),
                format!(
                    "invalid resume status/payload for interrupt {interrupt_id}: status={status}, approved={approved}"
                )
            );
        }
    }

    #[test]
    fn validate_resume_rejects_non_interrupted_states() {
        let resume = [resume("call-1")];
        let states = [
            SessionState::Idle,
            SessionState::Running {
                run_id: "run_expected".to_string(),
                started_at: Utc::now(),
            },
        ];

        for state in states {
            assert_eq!(
                validate_resume(&resume, &state).expect_err("session must be interrupted"),
                ResumeValidationError::SessionNotInterrupted
            );
        }
    }

    #[test]
    fn validate_resume_rejects_mismatched_prefixed_run_ids() {
        let state = interrupted_state("run_expected", &["run_actual:call-1"]);
        let resume = [resume("run_actual:call-1")];

        assert_eq!(
            validate_resume(&resume, &state).expect_err("run id must match interrupted run"),
            ResumeValidationError::MismatchedRunIds {
                expected_run_id: "run_expected".to_string(),
                actual_run_ids: vec!["run_actual".to_string()],
            }
        );
    }

    #[test]
    fn validate_resume_accepts_interrupt_ids_without_run_prefix() {
        let state = interrupted_state("run_expected", &["call-1"]);
        let resume = [resume("call-1")];

        assert_eq!(validate_resume(&resume, &state), Ok("original prompt"));
    }
}
