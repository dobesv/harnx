//! Deriving each turn's input from reconstructed session state: cancel
//! tombstones, resumable in-flight turns, and queued next-turn messages.

use super::agent_loop::fold_new_user_messages_since;
use super::ancestor_check::{check_ancestors, AncestorVerdict};
use super::backend::NatsSessionLogBackend;
use super::daemon::SessionActivate;
use super::session_turn::TurnWorker;
use crate::config::{GlobalConfig, Input};
use crate::nats_lease::NatsSessionLease;
use crate::nats_session::{interrupt_session, InterruptRequest};
use anyhow::{Context, Result};
use harnx_core::session_reconstruct::TurnStatus;

/// Shared, borrowed context for the `derive_*_turn_input` helpers. Groups the
/// three references they all thread through so each helper stays within the
/// function-argument budget.
#[derive(Clone, Copy)]
pub(super) struct TurnInputCtx<'a> {
    pub(super) activation: &'a SessionActivate,
    pub(super) per_session: &'a GlobalConfig,
    pub(super) backend: &'a NatsSessionLogBackend,
}

struct ResumableTurnState<'a> {
    context: Option<&'a harnx_core::session_reconstruct::ResumableCtx>,
    next_turn_messages: &'a [harnx_core::message::Message],
}

impl TurnWorker {
    /// Reconstruct session state using the canonical algorithm.
    ///
    /// Returns the session's turn status, effective pending message, and
    /// resumable context for driving the agent loop correctly.
    pub(super) async fn reconstruct_session_state(
        &self,
        backend: &NatsSessionLogBackend,
    ) -> harnx_core::session_reconstruct::ReconstructedState {
        match backend.load_events_latest_async().await {
            Ok(entries) => harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries),
            Err(err) => {
                log::warn!(
                    "failed to load session log for reconstruction: session_id={} worker_id={} err={err}",
                    backend.session_id(),
                    self.worker_id,
                );
                harnx_core::session_reconstruct::ReconstructedState {
                    turn_status: harnx_core::session_reconstruct::TurnStatus::Idle,
                    next_turn_messages: Vec::new(),
                    resumable_ctx: None,
                }
            }
        }
    }
    pub(super) async fn derive_continuation_turn_input(
        &self,
        ctx: TurnInputCtx<'_>,
        high_water: Option<u64>,
    ) -> Option<(Input, Option<u64>)> {
        // Continuation turns (high_water set) derive input CURSOR-based, not
        // barrier-based. A user message that arrives DURING a turn is logged at
        // a seq BELOW that turn's assistant barrier, so the barrier-based
        // reconstruct would treat it as already-answered and never fold it. The
        // cursor (high-water mark of messages already fed this activation) is the
        // authoritative "unanswered" boundary and matches the drain decision.
        let hw = high_water?;
        let tail = match ctx.backend.load_events_latest_async().await {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!(
                    "failed to load session log for continuation drain: session_id={} worker_id={} err={err}",
                    ctx.backend.session_id(),
                    self.worker_id,
                );
                Vec::new()
            }
        };
        let (new_messages, latest_seq) = fold_new_user_messages_since(&tail, Some(hw));
        if new_messages.is_empty() {
            return None;
        }
        let (mut input, seed) = self
            .derive_idle_turn_input(ctx.activation, ctx.per_session, new_messages)
            .await;
        input.with_session = true;
        input.skip_user_log_append = true;
        Some((input, seed.or(latest_seq)))
    }

    /// Derive the turn input for an activation from the reconstructed session
    /// state, honoring cancel tombstones, resumable turns, and pending messages.
    pub(super) async fn derive_turn_input(
        &self,
        ctx: TurnInputCtx<'_>,
        lease: &NatsSessionLease,
        high_water: Option<u64>,
    ) -> Result<(Input, Option<u64>)> {
        if let Some(continuation) = self.derive_continuation_turn_input(ctx, high_water).await {
            return Ok(continuation);
        }
        let TurnInputCtx {
            activation,
            per_session,
            backend,
        } = ctx;

        let reconstructed = self.reconstruct_session_state(backend).await;
        log::debug!(
            "derive_turn_input: session_id={} turn_status={:?} next_turn_count={} resumable_ctx={}",
            activation.session_id,
            reconstructed.turn_status,
            reconstructed.next_turn_messages.len(),
            reconstructed.resumable_ctx.is_some(),
        );
        let (mut input, seed_cursor) = match reconstructed.turn_status {
            // Wind-up is the activation preflight's job; a steering message
            // queued behind the Cancel is the only thing left to run here.
            TurnStatus::InterruptedPendingWindUp { .. } | TurnStatus::Idle => {
                self.derive_idle_turn_input(
                    activation,
                    per_session,
                    reconstructed.next_turn_messages,
                )
                .await
            }
            TurnStatus::InFlightResumable { .. } => {
                match self.refuse_resume_under_interrupted_parent(backend).await? {
                    true => (crate::config::input::from_str(per_session, "", None), None),
                    false => self.derive_resumable_turn_input(
                        ctx,
                        lease,
                        ResumableTurnState {
                            context: reconstructed.resumable_ctx.as_ref(),
                            next_turn_messages: &reconstructed.next_turn_messages,
                        },
                    )?,
                }
            }
        };
        // The NATS worker ALWAYS operates on a session. The input is derived
        // before `run_agent_loop_with_nats_inner` attaches the session to the
        // per-session config, so `from_str` sees no session and leaves
        // `with_session=false`. That would make `save_message` a no-op and the
        // turn's assistant barrier would never be persisted. Force it true.
        input.with_session = true;
        // The folded user messages are ALREADY durable in the log (clients
        // append them directly) and loaded into `session.messages`. The worker
        // must not re-append them: doing so duplicates the user message and
        // reorders the assistant barrier past concurrently-arrived messages,
        // burying them so they are never folded into a continuation turn.
        input.skip_user_log_append = true;
        Ok((input, seed_cursor))
    }

    /// A child session may only resume while the parent tool call that
    /// created it is still open. When it is not, interrupt this session so the
    /// log records why it stopped, and run nothing.
    async fn refuse_resume_under_interrupted_parent(
        &self,
        backend: &NatsSessionLogBackend,
    ) -> Result<bool> {
        let AncestorVerdict::Interrupted {
            parent_session,
            cancellation_id,
        } = check_ancestors(
            &self.jetstream,
            &self.session_metadata,
            backend.session_id(),
        )
        .await?
        else {
            return Ok(false);
        };
        log::info!(
            "refusing to resume under interrupted parent: session_id={} parent={parent_session}",
            backend.session_id()
        );
        interrupt_session(
            &self.jetstream,
            &self.client,
            &self.activation_route,
            InterruptRequest {
                session_id: backend.session_id().to_string(),
                cluster: self.cluster.clone(),
                cancellation_id: cancellation_id
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                requested_by: format!("parent:{parent_session}"),
                reason: "parent invocation interrupted".into(),
            },
        )
        .await?;
        Ok(true)
    }

    /// Resumable-turn input: resume from the last user message that kicked
    /// off the still-in-flight turn (orphan repair happens inside
    /// `run_agent_loop_with_nats_inner`).
    fn derive_resumable_turn_input(
        &self,
        ctx: TurnInputCtx<'_>,
        lease: &NatsSessionLease,
        state: ResumableTurnState<'_>,
    ) -> Result<(Input, Option<u64>)> {
        let TurnInputCtx {
            activation,
            per_session,
            ..
        } = ctx;
        log::info!(
            "resume state: session_id={} worker_id={} revision={} mode=resumable",
            activation.session_id,
            lease.worker_id(),
            lease.fence_token()
        );
        let resumable_ctx = state
            .context
            .context("refusing to resume an in-flight tool round without reconstructed context")?;
        // A resumable tool round must retain the user that initiated it. Empty
        // input is otherwise treated as a successful no-op by the shared agent
        // loop, which leaves the same durable tool tail resumable forever.
        let last_user_msg = resumable_ctx
            .last_user
            .as_ref()
            .context("refusing to resume an in-flight tool round without an initiating user")?;
        let input =
            crate::config::input::from_str(per_session, &last_user_msg.content.to_text(), None);
        // Replay includes user messages queued behind the orphan tool call in
        // the model history. The completion boundary must therefore cover the
        // latest of those messages, not merely the initiating user.
        let seed_cursor = resumable_seed_cursor(resumable_ctx, state.next_turn_messages)?;
        Ok((input, Some(seed_cursor)))
    }

    /// Idle-state input: fold queued next-turn messages in log order.
    pub(super) async fn derive_idle_turn_input(
        &self,
        activation: &SessionActivate,
        per_session: &GlobalConfig,
        next_turn_messages: Vec<harnx_core::message::Message>,
    ) -> (Input, Option<u64>) {
        log::debug!(
            "derive_turn_input idle: session_id={} messages_count={}",
            activation.session_id,
            next_turn_messages.len(),
        );
        let seed_cursor = next_turn_messages
            .last()
            .and_then(|message| message.log_seq.and_then(|seq| u64::try_from(seq).ok()));
        let folded = next_turn_messages
            .into_iter()
            .map(|message| message.content.to_text())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (
            crate::config::input::from_str(per_session, &folded, None),
            seed_cursor,
        )
    }
}

fn durable_user_seq(message: &harnx_core::message::Message) -> Result<u64> {
    let seq = message
        .log_seq
        .context("reconstructed NATS user message has no durable sequence")?;
    u64::try_from(seq).context("reconstructed NATS user sequence does not fit u64")
}

fn resumable_seed_cursor(
    resumable_ctx: &harnx_core::session_reconstruct::ResumableCtx,
    next_turn_messages: &[harnx_core::message::Message],
) -> Result<u64> {
    let last_user = resumable_ctx
        .last_user
        .as_ref()
        .context("resumable tool round has no initiating user")?;
    let mut cursor = durable_user_seq(last_user)?;
    for message in next_turn_messages {
        cursor = cursor.max(durable_user_seq(message)?);
    }
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use super::resumable_seed_cursor;
    use harnx_core::message::{Message, MessageContent, MessageRole};
    use harnx_core::session_reconstruct::ResumableCtx;

    fn user(text: &str, seq: usize) -> Message {
        Message::new(MessageRole::User, MessageContent::Text(text.to_string())).with_log_seq(seq)
    }

    #[test]
    fn resume_cursor_covers_users_queued_behind_the_tool_call() {
        let context = ResumableCtx {
            last_user: Some(user("original", 3)),
            last_assistant: None,
            pending_tool_results: Vec::new(),
            fence_token: Some(7),
        };

        assert_eq!(
            resumable_seed_cursor(&context, &[user("queued one", 8), user("queued two", 11)])
                .unwrap(),
            11
        );
    }

    #[test]
    fn resume_cursor_rejects_missing_initiating_user() {
        let context = ResumableCtx {
            last_user: None,
            last_assistant: None,
            pending_tool_results: Vec::new(),
            fence_token: Some(7),
        };

        assert!(resumable_seed_cursor(&context, &[]).is_err());
    }
}
