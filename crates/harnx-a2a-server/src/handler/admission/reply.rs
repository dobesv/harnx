//! Resolve replies before orphan reconciliation can treat a parked task as lost.
use super::*;
use crate::{
    hitl::{parse_reply, render_unrecognized, ReplyParse},
    store::{
        context::{AuthorityError, HitlPhase},
        StoreError,
    },
};
use a2a_lf::{Part, TaskState};
use harnx_runtime::PromptProgress;

impl HarnxHandler {
    pub(super) async fn admit_reply(
        &self,
        owner: &RequestIdentity,
        request: &MessageAdmission,
    ) -> Result<Option<Admission>, A2AError> {
        let Some(local_id) = &request.context else {
            return Ok(None);
        };
        let store = &self.backend.store;
        let storage = store
            .resolve_context(&self.export, owner, local_id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        let context = store.read_context(&storage).await.map_err(map_error)?;
        let active = context
            .as_ref()
            .and_then(|c| c.document.state.active.as_ref());
        // Durable mappings and inline receipts precede status/grammar checks.
        let mapping = store
            .get_reply_dedupe(&storage, &request.message.message_id)
            .await
            .map_err(map_error)?;
        let inline = active.and_then(|a| {
            a.hitl_checkpoint
                .as_ref()
                .and_then(|c| c.reply.as_ref())
                .filter(|r| r.message_id == request.message.message_id)
                .map(|r| (a.snapshot.task.id.clone(), r.fingerprint.clone()))
        });
        if let Some((id, fingerprint)) = mapping.or(inline) {
            if fingerprint != request.fingerprint {
                return Err(map_error(StoreError::FingerprintMismatch.into()));
            }
            if request
                .message
                .task_id
                .as_ref()
                .is_some_and(|target| target != &id)
            {
                return Err(A2AError::invalid_params(
                    "reply messageId belongs to a different task",
                ));
            }
            if !active.is_some_and(|a| {
                a.snapshot.task.id == id
                    && a.hitl_checkpoint.as_ref().is_some_and(|c| {
                        c.phase == HitlPhase::DecisionIntent
                            && c.reply
                                .as_ref()
                                .is_some_and(|r| r.message_id == request.message.message_id)
                    })
                    && !a.snapshot.task.status.state.is_terminal()
            }) {
                let record = self.task(owner, &id).await?;
                return Ok(Some(Admission::reply_snapshot(record)));
            }
        } else if let Some(record) = self.dedupe(owner, request).await? {
            // Initial-message retries also win over reply parsing.
            return Ok(Some(Admission::retry(self.reconcile(owner, record).await?)));
        }
        let Some(active) = active else {
            return Ok(None);
        };
        if request
            .message
            .task_id
            .as_ref()
            .is_some_and(|id| id != &active.snapshot.task.id)
        {
            return Ok(None);
        }
        if active.snapshot.task.status.state.is_terminal() {
            return Ok(None);
        }
        let Some(checkpoint) = active.hitl_checkpoint.as_ref().filter(|c| {
            active.snapshot.task.status.state == TaskState::InputRequired
                || c.phase == HitlPhase::DecisionIntent
        }) else {
            return Ok(None);
        };
        let exact = checkpoint.request.clone();
        if matches!(
            parse_reply(&request.message.parts),
            ReplyParse::Unrecognized
        ) {
            let session = self.session(owner, Some(local_id)).await?;
            let ticket =
                harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket::from_parts(
                    storage,
                    active.admission.invocation_id.clone(),
                    active.admission.prompt_id.clone(),
                    active
                        .admission
                        .closure_id
                        .clone()
                        .ok_or_else(|| A2AError::internal("request failed"))?,
                    active.admission.fixed_predecessor,
                )
                .map_err(map_error)?;
            let mut record = active.snapshot.clone();
            if let PromptProgress::InputRequired(pending) = session
                .fixed_prompt_progress(&ticket)
                .await
                .map_err(map_error)?
            {
                record.task.status.message = Some(Message::new(
                    a2a_lf::Role::Agent,
                    vec![Part::text(render_unrecognized(&pending))],
                ));
            }
            return Ok(Some(Admission::reply_snapshot(record)));
        }
        self.check_running()?;
        self.check_input_budget(&request.message).await?;
        let boundary = active.snapshot.stream_seq;
        let session = self.session(owner, Some(local_id)).await?;
        let started = self
            .backend
            .runner
            .reply_hitl(
                TurnRequest {
                    export: &self.export,
                    owner,
                    session,
                    message: request.message.clone(),
                },
                exact,
            )
            .await
            .map_err(|error| {
                if error.downcast_ref::<AuthorityError>() == Some(&AuthorityError::Conflict) {
                    A2AError::invalid_params(
                        "approval changed or another reply was accepted; fetch the current task",
                    )
                } else {
                    map_error(error)
                }
            })?;
        if started.deduped {
            return Ok(Some(Admission::reply_snapshot(started.snapshot)));
        }
        let following = started.snapshot.task.status.state == TaskState::Working;
        Ok(Some(Admission {
            snapshot: started.snapshot,
            deduped: false,
            response_after: following.then_some(boundary),
            direct: !following,
            events: following.then_some(started.events),
        }))
    }
}
