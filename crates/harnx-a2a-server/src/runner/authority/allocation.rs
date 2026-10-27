//! Build reserved task state without changing admission or publication.
use super::*;
use crate::store::context::{AdmissionPhase, AdmissionState, RetainedMessage};

impl TaskReservation<'_> {
    pub(super) fn into_active(self) -> ActiveTask {
        let TaskReservation {
            allocation,
            message,
            predecessor,
        } = self;
        let fingerprint = crate::store::message_fingerprint(&message.parts);
        ActiveTask {
            snapshot: TaskRecord {
                version: 1,
                task: Task {
                    id: allocation.task_id.clone(),
                    context_id: allocation.local_id.clone(),
                    status: status(TaskState::Submitted, None),
                    artifacts: None,
                    history: Some(vec![message.clone()]),
                    metadata: None,
                },
                user_msg_id: allocation.prompt_id.clone(),
                user_msg_seq: 0,
                execution_id: allocation.invocation_id.clone(),
                revision: 1,
                stream_seq: 0,
                created_at: allocation.created_at,
                updated_at: allocation.created_at,
            },
            message: RetainedMessage {
                message_id: message.message_id,
                fingerprint,
            },
            admission: AdmissionState {
                invocation_id: allocation.invocation_id.clone(),
                prompt_id: allocation.prompt_id.clone(),
                closure_id: Some(allocation.closure_id.clone()),
                fixed_predecessor: predecessor,
                phase: AdmissionPhase::Reserved,
                prompt_sequence: None,
            },
            cancel: None,
            publication: Default::default(),
            projections: Default::default(),
            stop_confirmed: false,
            hitl_checkpoint: None,
        }
    }
}
