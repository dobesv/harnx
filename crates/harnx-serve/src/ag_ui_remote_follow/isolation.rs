//! Keep one remote run bound to its original prompt through the output queue.
//!
//! A remote follow ends either when the turn completes or when the turn is
//! interrupted. The session log is the only authority for the second: a
//! `Cancel` above this prompt's own sequence ends the follow even while the
//! SSE output queue is full and nothing else can make progress. A `Cancel`
//! below it stopped an earlier turn — this prompt was typed after the
//! interruption, and ending on it would close the stream the moment it opened.
use anyhow::Result;
use harnx_runtime::nats_event_sink::JetstreamContext;
use harnx_runtime::nats_session::interrupt::await_prompt_interrupt;
use harnx_runtime::nats_session_log::NatsSessionLog;

#[derive(Clone)]
pub(super) struct RemoteInterruptWatch {
    log: NatsSessionLog,
    /// Sequence of the user message this follow is watching a turn for.
    prompt_seq: u64,
}

impl RemoteInterruptWatch {
    pub(super) fn bind(jetstream: &JetstreamContext, session: &str, prompt_seq: u64) -> Self {
        Self {
            log: NatsSessionLog::new(jetstream.clone(), session.to_string()),
            prompt_seq,
        }
    }

    /// Resolves with the sequence of the `Cancel` that stopped this prompt's
    /// turn, which is also the fence the reader applies to live output: an
    /// advisory from below it belongs to work the `Cancel` already ended.
    ///
    /// Only the entries above the prompt can carry that `Cancel`, so only
    /// those are read — once per follower per poll, for as long as the turn
    /// runs. A read that fails is retried rather than propagated: it would
    /// otherwise retire a live attachment whose turn is still healthy.
    pub(super) async fn wait_for_stop(&self) -> Result<u64> {
        let log = self.log.clone();
        let prompt_seq = self.prompt_seq;
        await_prompt_interrupt(prompt_seq, move || {
            let log = log.clone();
            async move { log.load_events_after_async(prompt_seq).await }
        })
        .await
    }
}
