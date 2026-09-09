use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterruptResume {
    pub(crate) interrupt_id: String,
    pub(crate) status: InterruptResumeStatus,
    pub(crate) payload: InterruptResumePayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InterruptResumeStatus {
    Approved,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterruptResumePayload {
    pub(crate) approved: bool,
    pub(crate) reason: Option<String>,
}

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
        .map(|entry| {
            let status = match (entry.status.as_str(), entry.payload.approved) {
                ("resolved" | "approved", true) => InterruptResumeStatus::Approved,
                ("cancelled" | "denied" | "rejected", false) => InterruptResumeStatus::Denied,
                _ => anyhow::bail!(
                    "invalid resume status/payload for interrupt {}: status={}, approved={}",
                    entry.interrupt_id,
                    entry.status,
                    entry.payload.approved
                ),
            };
            Ok(InterruptResume {
                interrupt_id: entry.interrupt_id.clone(),
                status,
                payload: InterruptResumePayload {
                    approved: entry.payload.approved,
                    reason: entry.payload.reason.clone(),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
