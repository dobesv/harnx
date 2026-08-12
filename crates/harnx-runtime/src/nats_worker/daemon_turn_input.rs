//! Deriving each turn's input from reconstructed session state: cancel
//! tombstones, resumable in-flight turns, and queued next-turn messages.

use super::agent_loop::fold_new_user_messages_since;
use super::backend::NatsSessionLogBackend;
use super::daemon::SessionActivate;
use super::daemon_runtime::WorkerRuntime;
use crate::config::{GlobalConfig, Input};
use crate::nats_lease::NatsSessionLease;

/// Shared, borrowed context for the `derive_*_turn_input` helpers. Groups the
/// three references they all thread through so each helper stays within the
/// function-argument budget.
#[derive(Clone, Copy)]
pub(super) struct TurnInputCtx<'a> {
    pub(super) activation: &'a SessionActivate,
    pub(super) per_session: &'a GlobalConfig,
    pub(super) backend: &'a NatsSessionLogBackend,
}

impl WorkerRuntime {
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
    ) -> (Input, Option<u64>) {
        if let Some(continuation) = self.derive_continuation_turn_input(ctx, high_water).await {
            return continuation;
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
            harnx_core::session_reconstruct::TurnStatus::InFlightCancelled => {
                // Terminal: Cancel tombstone prevents resume. Do NOT consume pending.
                // The turn is idle; wait for new user input or activation.
                log::info!(
                    "session has cancelled turn tombstone; not resuming (session_id={})",
                    activation.session_id
                );
                (crate::config::input::from_str(per_session, "", None), None)
            }
            harnx_core::session_reconstruct::TurnStatus::InFlightResumable => {
                self.derive_resumable_turn_input(ctx, lease, reconstructed.resumable_ctx)
            }
            harnx_core::session_reconstruct::TurnStatus::Idle => {
                let msg_count = reconstructed.next_turn_messages.len();
                let result = self
                    .derive_idle_turn_input(
                        activation,
                        per_session,
                        reconstructed.next_turn_messages,
                    )
                    .await;
                // DEBUG: log the seed_cursor for this path
                log::debug!(
                    "derive_turn_input idle: session_id={} seed_cursor={:?} messages_count={}",
                    activation.session_id,
                    result.1,
                    msg_count,
                );
                result
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
        (input, seed_cursor)
    }

    /// Resumable-turn input: resume from the last user message that kicked
    /// off the still-in-flight turn (orphan repair happens inside
    /// `run_agent_loop_with_nats_inner`).
    fn derive_resumable_turn_input(
        &self,
        ctx: TurnInputCtx<'_>,
        lease: &NatsSessionLease,
        resumable_ctx: Option<harnx_core::session_reconstruct::ResumableCtx>,
    ) -> (Input, Option<u64>) {
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
        // Extract the last_user Message to get both its text (for Input) and log_seq (for cursor).
        let last_user_msg = resumable_ctx
            .as_ref()
            .and_then(|ctx| ctx.last_user.as_ref());
        let input = if let Some(last_user) = last_user_msg {
            crate::config::input::from_str(per_session, &last_user.content.to_text(), None)
        } else {
            crate::config::input::from_str(per_session, "", None)
        };
        // Cursor: the log_seq of the last user message that kicked off this resumable turn.
        // Any messages appended AFTER this (seq > cursor) will be folded mid-turn.
        let seed_cursor =
            last_user_msg.and_then(|msg| msg.log_seq.and_then(|seq| u64::try_from(seq).ok()));
        (input, seed_cursor)
    }

    /// Idle-state input: fold queued next-turn messages in log order.
    pub(super) async fn derive_idle_turn_input(
        &self,
        _activation: &SessionActivate,
        per_session: &GlobalConfig,
        next_turn_messages: Vec<harnx_core::message::Message>,
    ) -> (Input, Option<u64>) {
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
