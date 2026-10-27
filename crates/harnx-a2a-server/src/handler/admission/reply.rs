//! Resolve replies before orphan reconciliation can treat a parked task as lost.
use super::*;
use crate::{
    hitl::{parse_reply, render_unrecognized, ReplyParse},
    store::{
        context::{ActiveTask, AuthorityError, HitlCheckpoint, HitlPhase},
        DedupeEntry, StoreError,
    },
};
use a2a_lf::{Part, TaskState};
use harnx_runtime::{ApprovalRef, PromptProgress};

mod dedupe;
mod response;

struct ReplyLookup<'a> {
    storage: &'a str,
    active: Option<&'a ActiveTask>,
}

struct ReplyCandidate<'a> {
    storage: &'a str,
    active: &'a ActiveTask,
}

struct BoundReply<'a> {
    local_id: &'a str,
    request: ApprovalRef,
    boundary: u64,
}

impl HarnxHandler {
    async fn read_reply_context(
        &self,
        owner: &RequestIdentity,
        local_id: &str,
    ) -> Result<(String, Option<crate::store::context::ContextSnapshot>), A2AError> {
        let store = &self.backend.store;
        let storage = store
            .resolve_context(&self.export, owner, local_id)
            .await
            .map_err(map_error)?
            .ok_or_else(not_found)?;
        let context = store.read_context(&storage).await.map_err(map_error)?;
        Ok((storage, context))
    }

    pub(super) async fn admit_reply(
        &self,
        owner: &RequestIdentity,
        request: &MessageAdmission,
    ) -> Result<Option<Admission>, A2AError> {
        let Some(local_id) = &request.context else {
            return Ok(None);
        };
        let (storage, context) = self.read_reply_context(owner, local_id).await?;
        let active = context
            .as_ref()
            .and_then(|c| c.document.state.active.as_ref());
        if let Some(retry) = self
            .lookup_reply_retry(
                owner,
                request,
                ReplyLookup {
                    storage: &storage,
                    active,
                },
            )
            .await?
        {
            return Ok(Some(retry));
        }
        let Some((active, checkpoint)) = reply_candidate(active, &request.message) else {
            return Ok(None);
        };
        let bound = BoundReply {
            local_id,
            request: checkpoint.request.clone(),
            boundary: active.snapshot.stream_seq,
        };
        if matches!(
            parse_reply(&request.message.parts),
            ReplyParse::Unrecognized
        ) {
            let candidate = ReplyCandidate {
                storage: &storage,
                active,
            };
            return self
                .unrecognized_reply(owner, local_id, candidate)
                .await
                .map(Some);
        }
        self.resume_reply(owner, request, bound).await.map(Some)
    }
}

fn reply_candidate<'a>(
    active: Option<&'a ActiveTask>,
    message: &Message,
) -> Option<(&'a ActiveTask, &'a HitlCheckpoint)> {
    let active = active?;
    if message
        .task_id
        .as_ref()
        .is_some_and(|id| id != &active.snapshot.task.id)
    {
        return None;
    }
    if active.snapshot.task.status.state.is_terminal() {
        return None;
    }
    let checkpoint = active.hitl_checkpoint.as_ref().filter(|c| {
        active.snapshot.task.status.state == TaskState::InputRequired
            || c.phase == HitlPhase::DecisionIntent
    })?;
    Some((active, checkpoint))
}
