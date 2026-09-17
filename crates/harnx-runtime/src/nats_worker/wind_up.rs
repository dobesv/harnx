//! Winding up the turn a `Cancel` interrupted.
//!
//! The `Cancel` entry ends the turn, but it leaves the log owing a result for
//! every tool call the turn had already made: a transcript whose last
//! `ToolCalls` entry is unanswered cannot be replayed to a model. The lease
//! holder closes that round out. It resends an (idempotent) cancel for every
//! call the invocation journal has a row but no reply for — a call with no row
//! at all was never dispatched and there is nothing to cancel — then appends
//! ONE `ToolResults` entry carrying each journal reply where one exists and a
//! placeholder where it does not.
//!
//! A tool that finished while the interrupt was travelling wrote its result
//! to the journal, not to the log: taking replies from there is what keeps a
//! completed call's real output instead of overwriting it with a placeholder.
//!
//! Wind-up is idempotent by construction. The appended `ToolResults` answers
//! the orphans, so a second pass reconstructs `TurnStatus::Idle`, returns
//! [`WindUpOutcome::Nothing`] and writes nothing.

use super::backend::NatsSessionLogBackend;
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::NatsSessionLease;
use crate::nats_tool_provider::{InFlightCancelTarget, NatsInFlightCalls};
use anyhow::Result;
use harnx_core::event::{AgentEvent, TurnEvent};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::session_reconstruct::{OrphanToolCalls, TurnStatus};
use harnx_core::tool::{ToolCall, ToolError};
use harnx_toolset::ToolReply;
use harnx_toolset_server::invocation_journal::{InvocationJournal, RecordedInvocation};
use std::collections::HashMap;

/// Everything one wind-up needs. Only the lease holder may build this.
pub(super) struct WindUpInputs<'a> {
    pub backend: &'a NatsSessionLogBackend,
    pub lease: &'a NatsSessionLease,
    pub client: &'a async_nats::Client,
    pub jetstream: &'a async_nats::jetstream::Context,
    /// Durability for the invocation journal bucket. The lease does not carry
    /// its own replica count, so the worker's configured one is passed in.
    pub replicas: usize,
    pub in_flight: &'a NatsInFlightCalls,
    pub event_sink: Option<&'a NatsEventSink>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WindUpOutcome {
    /// Nothing was owed: no interrupted turn, or one already wound up.
    Nothing,
    Appended {
        seq: u64,
        placeholders: usize,
        real: usize,
    },
}

/// Cancel every incomplete call of the interrupted turn (idempotent), then
/// append one `ToolResults` with journal replies where present and
/// placeholders otherwise. Only the lease holder calls this.
pub(super) async fn wind_up_interrupted_turn(inputs: WindUpInputs<'_>) -> Result<WindUpOutcome> {
    let entries = inputs.backend.load_events_latest_async().await?;
    let TurnStatus::InterruptedPendingWindUp {
        cancel_seq,
        cancellation_id,
        orphans,
    } = harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries).turn_status
    else {
        return Ok(WindUpOutcome::Nothing);
    };
    let wind_up = WindUp {
        journal: InvocationJournal::ensure(inputs.jetstream, inputs.replicas).await?,
        session: inputs.backend.session_id().to_string(),
        // A `Cancel` written without one still has to name itself to the
        // tools it stops, and its sequence is unique within the session.
        cancellation_id: cancellation_id.unwrap_or_else(|| format!("cancel-{cancel_seq}")),
        live: live_targets(&inputs).await,
        inputs,
    };
    let wound = wind_up.close_out(&orphans).await?;
    let entry = SessionLogEntry::ToolResults {
        results: wound.results,
        timestamp: Some(chrono::Utc::now()),
    };
    let seq = wind_up
        .inputs
        .backend
        .append_wind_up_fenced_with_lease(
            &entry,
            wind_up.inputs.lease,
            super::backend::WoundUpRound {
                cancel_seq,
                expected_tail: entries.last().map_or(0, |(seq, _)| *seq),
            },
        )
        .await?;
    if let Some(sink) = wind_up.inputs.event_sink {
        sink.emit_required(AgentEvent::Turn(TurnEvent::Interrupted {
            cancellation_id: wind_up.cancellation_id.clone(),
        }));
    }
    log::info!(
        "interrupted turn wound up: session_id={} cancel_seq={cancel_seq} results_seq={seq} placeholders={} real={}",
        wind_up.session,
        wound.placeholders,
        wound.real,
    );
    Ok(WindUpOutcome::Appended {
        seq,
        placeholders: wound.placeholders,
        real: wound.real,
    })
}

struct WindUp<'a> {
    inputs: WindUpInputs<'a>,
    journal: InvocationJournal,
    /// Storage key of the session being wound up, as the journal keys it.
    session: String,
    cancellation_id: String,
    /// Calls this process still has registered, by the wire call id dispatch
    /// minted. Their recorded control subject is the one the call was
    /// actually dispatched on.
    live: HashMap<String, InFlightCancelTarget>,
}

#[derive(Default)]
struct WoundUp {
    results: Vec<ToolOutput>,
    /// Results recovered from the journal rather than synthesized.
    real: usize,
    placeholders: usize,
}

impl WindUp<'_> {
    /// One result per interrupted call, in the order the turn made them.
    async fn close_out(&self, orphans: &[OrphanToolCalls]) -> Result<WoundUp> {
        // One listing answers every orphan: the journal lists a session by
        // streaming its whole bucket, so reading row by row would pay for
        // that once per call — narrowed to the interrupted rounds, since a
        // long-lived session may hold far more journal rows than this wind-up
        // owes results for. A read that FAILS is not a missing reply —
        // writing a placeholder over a result we simply could not see would
        // lose it — so the error leaves the whole wind-up for a later attempt.
        let rounds: Vec<u64> = orphans.iter().map(|orphan| orphan.seq).collect();
        let records = self
            .journal
            .records_in_rounds(&self.session, &rounds)
            .await?;
        let mut wound = WoundUp::default();
        for orphan in orphans {
            for call in &orphan.calls {
                let recorded = recorded_call(&records, orphan.seq, call);
                self.close_one(call, recorded, &mut wound).await;
            }
        }
        Ok(wound)
    }

    /// Answer one interrupted call: its journal reply where the tool got one
    /// back in time, and otherwise a placeholder plus a cancel resent to
    /// whatever is still running it.
    async fn close_one(
        &self,
        call: &ToolCall,
        recorded: Option<&RecordedInvocation>,
        wound: &mut WoundUp,
    ) {
        if let Some((record, reply)) =
            recorded.and_then(|record| Some((record, record.reply.clone()?)))
        {
            wound.real += 1;
            wound
                .results
                .push(tool_output_from_reply(call, record, reply));
            return;
        }
        wound.placeholders += 1;
        if let Some(id) = call.id.as_deref() {
            self.resend_cancel(id, recorded).await;
        }
        wound
            .results
            .extend(crate::config::session::interrupted_tool_outputs(
                std::slice::from_ref(call),
                Some(&self.cancellation_id),
            ));
    }

    /// Resend a cancel for a call with no reply. The tool server treats a
    /// repeat of the same cancellation as the one it may already have seen,
    /// so this costs nothing when the first cancel arrived.
    async fn resend_cancel(&self, call_id: &str, recorded: Option<&RecordedInvocation>) {
        let Some(target) = self.cancel_target(recorded) else {
            log::debug!("wind-up has nothing to cancel for an unjournaled call: call_id={call_id}");
            return;
        };
        if let Err(error) = crate::nats_tool_provider::publish_tool_cancel(
            self.inputs.client,
            &target,
            &self.session,
            &self.cancellation_id,
        )
        .await
        {
            log::debug!("wind-up cancel not published: call_id={call_id} error={error:#}");
        }
    }

    /// Where a cancel has to be addressed. The journal row names the call: a
    /// tool server knows it by the wire id dispatch minted, never by the id
    /// the transcript gave it, so a call with no row cannot be cancelled at
    /// all. Given the row, this process's own registration wins while it
    /// still holds the call; otherwise the row records the server that took
    /// the call and the scope whose control subject reaches it once the
    /// calling process is gone.
    fn cancel_target(&self, recorded: Option<&RecordedInvocation>) -> Option<InFlightCancelTarget> {
        let record = recorded?;
        let call_id = record.request.call_id.as_str();
        if let Some(target) = self.live.get(call_id) {
            return Some(target.clone());
        }
        Some(InFlightCancelTarget {
            call_id: call_id.to_string(),
            server: record.server.clone(),
            control_subject: harnx_core::instance::ServerScope::from_string(
                record.server_scope.clone(),
            )
            .control_subject(),
        })
    }
}

/// The journal row that answers one interrupted call, out of the session's
/// rows. Dispatch mints its own wire id and keys the row by that, so a
/// transcript id reaches its rows only through the round that made the call —
/// exactly how a replay finds them.
///
/// A call retried inside one round leaves a row per attempt. The attempt that
/// replied is the one that answers the call; with none of them answered the
/// newest is taken, which means only that attempt is cancelled here — an
/// older one is left to the cancel its own dispatcher sends while it still
/// holds the call. `started_at_ms` is a millisecond, so two attempts started
/// inside the same one fall back to the order the journal listed them in.
fn recorded_call<'a>(
    records: &'a [RecordedInvocation],
    round: u64,
    call: &ToolCall,
) -> Option<&'a RecordedInvocation> {
    let call_id = call.id.as_deref()?;
    records
        .iter()
        .filter(|record| record.answers(round, call_id))
        .max_by_key(|record| (record.reply.is_some(), record.started_at_ms))
}

async fn live_targets(inputs: &WindUpInputs<'_>) -> HashMap<String, InFlightCancelTarget> {
    inputs
        .in_flight
        .snapshot_for_session(inputs.backend.session_id())
        .await
        .into_iter()
        .map(|target| (target.call_id.clone(), target))
        .collect()
}

/// Persist a journal reply as the call's transcript result. A failed call is
/// still an answer: it lands as an error output rather than a placeholder,
/// because the tool did run and did report.
///
/// The reply goes through the same decode as a replayed one rather than being
/// written as it stands: the journal holds the tool's raw handler value, whose
/// `_meta` execution context is private to the tool server and would otherwise
/// reach the transcript unattested. Wind-up's only sink is the unfenced
/// `NatsSessionLogBackend` — it does implement `persist_execution_contexts`,
/// but going through it would bypass the lease every other wind-up write is
/// fenced under, so the observation the decode validates is dropped with the
/// envelope instead of persisted unfenced.
fn tool_output_from_reply(
    call: &ToolCall,
    record: &RecordedInvocation,
    reply: ToolReply,
) -> ToolOutput {
    let output = match crate::nats_tool_provider::decode_journaled_reply(record, reply) {
        Ok(decoded) => decoded.value,
        Err(ToolError::Recoverable(error) | ToolError::Fatal(error)) => {
            serde_json::json!({ "error": error.to_string() })
        }
    };
    ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        output,
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}
