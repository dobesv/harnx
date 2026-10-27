//! Bind accepted replies to the challenged generation and response cursor.
use super::*;
use crate::runner::StartTurnResult;

impl HarnxHandler {
    pub(super) async fn unrecognized_reply(
        &self,
        owner: &RequestIdentity,
        local_id: &str,
        candidate: ReplyCandidate<'_>,
    ) -> Result<Admission, A2AError> {
        let session = self.session(owner, Some(local_id)).await?;
        let active = candidate.active;
        let ticket =
            harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket::from_parts(
                candidate.storage.into(),
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
        Ok(Admission::reply_snapshot(record))
    }

    pub(super) async fn resume_reply(
        &self,
        owner: &RequestIdentity,
        request: &MessageAdmission,
        bound: BoundReply<'_>,
    ) -> Result<Admission, A2AError> {
        self.check_running()?;
        self.check_input_budget(&request.message).await?;
        let session = self.session(owner, Some(bound.local_id)).await?;
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
                bound.request,
            )
            .await
            .map_err(reply_error)?;
        Ok(reply_admission(started, bound.boundary))
    }
}

fn reply_error(error: anyhow::Error) -> A2AError {
    if error.downcast_ref::<AuthorityError>() == Some(&AuthorityError::Conflict) {
        return A2AError::invalid_params(
            "approval changed or another reply was accepted; fetch the current task",
        );
    }
    map_error(error)
}

fn reply_admission(started: StartTurnResult, boundary: u64) -> Admission {
    if started.deduped {
        return Admission::reply_snapshot(started.snapshot);
    }
    let following = started.snapshot.task.status.state == TaskState::Working;
    Admission {
        snapshot: started.snapshot,
        deduped: false,
        response_after: following.then_some(boundary),
        direct: !following,
        events: following.then_some(started.events),
    }
}
