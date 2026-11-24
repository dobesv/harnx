//! Command-side surface action protocol (plan T5.1, review N-B/F1): prove
//! lease ownership before the early `Pending` acknowledgement, keep a
//! non-holder silent on the wire, and contain handler panics so an acked
//! client always gets an explicit answer instead of waiting out its whole
//! acked-outcome budget in silence.

use super::SessionControlHandler;
use crate::surfaces::{SurfaceActionOutcome, SurfaceActionReply, SurfaceActionSubmission};

impl SessionControlHandler {
    /// Handle one surface action command: stay silent when this worker
    /// cannot prove lease ownership, otherwise acknowledge `Pending` and
    /// run the attempt in a contained task.
    pub(super) async fn handle_surface_action_command(
        &self,
        submission: SurfaceActionSubmission,
        reply: Option<async_nats::Subject>,
    ) {
        // Ownership before the acknowledgement (review N-B): a stale
        // subscriber that acked `Pending` without the lease would park the
        // client for its whole acked-outcome budget and then answer
        // nothing — its handler refuses at the lease check. A non-holder
        // refuses without a wire reply too (F1, like the interaction path
        // in `control/interaction.rs`): control commands use a plain
        // subscribe, so a paused or reconnecting stale listener can still
        // answer resends, and a `lease_lost` from it can land after the
        // real holder's `Pending` — the client treats a post-ack
        // `lease_lost` as terminal, so it would report failure while the
        // holder's action proceeds. Refusing worker-side only is safe: the
        // client ignores nothing it needs here, keeps resending until a
        // worker that proves ownership answers, and still gets its
        // definite answer from the holder or, when none exists, from the
        // recovery deadline's lease-checked idle fallback.
        if !self.holds_session_lease().await {
            log::debug!(
                "surface action on surface '{}' for session '{}' refused: \
                 this worker does not hold the session lease",
                submission.surface_id,
                self.session_id
            );
            return;
        }
        // Acknowledge before the heavy work: a direct tool target runs
        // through the operator RPC, which can take minutes, and the client
        // must never fall back to its own idle-path execution while a
        // worker is processing this command.
        self.publish_control_reply(
            reply.clone(),
            &SurfaceActionReply::rejected(SurfaceActionOutcome::Pending),
            "surface action ack",
        )
        .await;
        // The listener must stay responsive to `Cancel` and other decisions
        // meanwhile, so the accepted attempt runs in its own task,
        // coalesced per surface generation. The attempt is contained
        // (review N-B): after the early ack, silence would stall the client
        // for its whole acked-outcome budget, so a panic is answered
        // explicitly.
        let handler = self.clone();
        let runner = self.clone();
        let panic_reply = reply;
        let attempt_reply = panic_reply.clone();
        tokio::spawn(async move {
            runner
                .answer_if_panicked(panic_reply, async move {
                    handler
                        .apply_surface_action(&submission, attempt_reply)
                        .await;
                })
                .await;
        });
    }

    /// Run one accepted surface-action attempt and guarantee an answer
    /// (review N-B): after the early acknowledgement a panic would leave
    /// the client waiting out its whole acked-outcome budget in silence,
    /// so it is contained and answered with `append_failed`. A direct tool
    /// may already have run, so this outcome does not imply no side effects.
    pub(super) async fn answer_if_panicked<F>(&self, reply: Option<async_nats::Subject>, attempt: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let result =
            futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(attempt)).await;
        if let Err(panic) = result {
            // Extracting a panic message from `Box<dyn Any>` has exactly one
            // mechanism (downcast to &str or String); the type cannot carry
            // anything else.
            // qual:allow(coupling, deh) reason: "panic payloads are `Box<dyn Any>`; downcasting to `&str`/`String` is the only way to recover a message for the log."
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .map(str::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".into());
            log::error!("surface action handler panicked: {message}");
            self.publish_control_reply(
                reply,
                &SurfaceActionReply::rejected(SurfaceActionOutcome::AppendFailed),
                "surface action panic answer",
            )
            .await;
        }
    }
}
