//! Accepted replies are control intent on an existing ticket, never new prompts.
use super::*;
use harnx_runtime::ApprovalRef;

mod accept;
mod advance;
mod intent;
mod retry;
mod target;

pub(crate) use target::ReplyTarget;

impl Runner {
    /// `request` is captured before attempting ownership and never reselected.
    pub(crate) async fn reply_hitl(
        self: &Arc<Self>,
        turn: TurnRequest<'_>,
        request: ApprovalRef,
    ) -> Result<StartTurnResult> {
        let TurnRequest {
            export,
            owner,
            session,
            message,
        } = turn;
        self.authorize_session(export, owner, &session).await?;
        let fingerprint = crate::store::message_fingerprint(&message.parts);
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::HitlReplyBound)
            .await;

        let target = ReplyTarget {
            session: &session,
            request: &request,
            message: &message,
            fingerprint: &fingerprint,
        };

        let mut authority = match retry::claim_reply_authority_or_retry(self, &target).await? {
            retry::ClaimOutcome::Authority(auth) => *auth,
            retry::ClaimOutcome::Snapshot(res) => return Ok(*res),
        };

        let resume = match accept::accept_reply(self, &target, &mut authority).await {
            Ok(resume) => resume,
            Err(error) => return retry::handle_accept_error(self, &target, authority, error).await,
        };

        if resume {
            self.resume_owned_hitl(export, session, authority).await
        } else {
            park_reply_snapshot(&self.store, session.storage_key(), authority).await
        }
    }
}

async fn park_reply_snapshot(
    store: &A2aStore,
    storage_key: &str,
    authority: OwnedTask,
) -> Result<StartTurnResult> {
    let snapshot = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("reply task missing")?
        .snapshot
        .clone();
    authority.release(store, storage_key).await?;
    Ok(reply_snapshot(snapshot))
}

pub(super) fn reply_snapshot(snapshot: TaskRecord) -> StartTurnResult {
    let (_, events) = broadcast::channel(EVENT_CAPACITY);
    StartTurnResult {
        snapshot,
        events,
        deduped: true,
    }
}
