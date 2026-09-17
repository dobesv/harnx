use super::*;
use harnx_toolset::ReplayAttempt;

impl NatsToolProvider {
    pub(super) async fn record_invocation(
        &self,
        request: &ToolRequest,
        tool_name: &str,
        server: &str,
    ) -> anyhow::Result<()> {
        let Some(session) = request.parent_session_id.as_deref() else {
            return Ok(());
        };
        let js = async_nats::jetstream::new(self.client.clone());
        // The round is the transcript sequence whose `ToolCalls` made this
        // call, and it keys the journal row. A call dispatched outside a
        // durable round — a direct provider call, a tool a frontend invoked —
        // has none, which is what zero records, exactly as for a call with no
        // id at all. Only a transcript orphan is ever replayed by round, and
        // an orphan by definition has the entry this looks for.
        let round = match request.tool_call_id.as_deref() {
            Some(call_id) => invocation_round(&js, session, call_id).await?.unwrap_or(0),
            None => 0,
        };
        harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
            &js,
            self.journal_replicas,
        )
        .await?
        .record(
            request,
            (tool_name, self.instance_id.as_str(), server),
            round,
        )
        .await
    }

    pub(super) async fn replay_recorded_call(
        &self,
        replay: harnx_core::tool::ToolReplay<'_>,
        abort: &AbortSignal,
    ) -> anyhow::Result<Option<ToolProviderOutput>> {
        let harnx_core::tool::ToolReplay {
            session_id: session,
            tool_round: round,
            call,
            ..
        } = replay;
        let Some(call_id) = call.id.as_deref() else {
            return Ok(None);
        };
        let js = async_nats::jetstream::new(self.client.clone());
        let journal = harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
            &js,
            self.journal_replicas,
        )
        .await?;
        let Some(record) = journal.find(session, round, call_id).await? else {
            return Ok(None);
        };
        anyhow::ensure!(record.tool_name == call.name, "replayed tool name changed");
        if let Some(reply) = journal.completed_reply(&record.request).await? {
            return recovered_output(decode_journaled_reply(&record, reply));
        }
        self.dispatch_replay(record, replay, abort).await
    }

    /// Re-dispatch a durably recorded call that has no completed reply yet.
    /// The journal is the only source of truth for the outcome now: this
    /// worker sends the wire request tagged as a replay attempt and trusts
    /// whatever the tool server returns (its own reply is journaled
    /// first-writer-wins before it ever reaches the wire).
    async fn dispatch_replay(
        &self,
        record: harnx_toolset_server::invocation_journal::RecordedInvocation,
        replay: harnx_core::tool::ToolReplay<'_>,
        abort: &AbortSignal,
    ) -> anyhow::Result<Option<ToolProviderOutput>> {
        let route = self
            .resolve_route(&record.tool_name)
            .context("replayed tool unavailable; keeping the pending invocation for recovery")?;
        // Scope identifies a process lifetime and must change across restart.
        // The logical server identity and raw tool must still be the same.
        validate_replay_route(&route, &record)?;
        let mut request = record.request;
        request.replay = Some(ReplayAttempt {
            attempt: 1,
            requested_by: self.instance_id.to_string(),
        });
        let id = request.call_id.clone();
        let pending =
            self.prepare_recorded_request(request, &route)
                .map_err(|error| match error {
                    ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
                })?;
        if let Some(authorization) = replay.authorization {
            authorization.revalidate().await?;
        }
        anyhow::ensure!(!abort.aborted(), "tool replay aborted before dispatch");
        let message = self
            .await_response(pending, abort)
            .await
            .map_err(|error| match error {
                ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
            })?;
        recovered_output(self.decode_reply(message, id, route))
    }
}

/// Decode a reply the journal kept, attested to the row that recorded it.
///
/// A journaled value is the tool's raw handler output: the server writes it
/// before the reply envelope is finalized, so its `_meta` execution-context
/// block has never been through the worker's own validation. Every reader of
/// a durable reply therefore decodes it here instead of taking the value as
/// it stands, which is what keeps the tool server's private context out of
/// the transcript and stamps the observation with provenance this side
/// vouches for.
pub(crate) fn decode_journaled_reply(
    record: &harnx_toolset_server::invocation_journal::RecordedInvocation,
    reply: ToolReply,
) -> Result<ToolProviderOutput, ToolError> {
    NatsToolProvider::decode_recorded_reply(
        reply,
        ToolObservationProvenance::new(
            record.server_scope.clone(),
            record.server.clone(),
            record.request.tool.clone(),
            record.request.call_id.clone(),
        ),
    )
}

/// The sequence of the `ToolCalls` entry that made `call_id`, or `None` when
/// this session's transcript never recorded one.
///
/// This scans the RAW log, while every caller that hands a round back in —
/// wind-up and the orphan detection a replay comes from — takes it from the
/// effective log, after `apply_log_mutations`. The two agree unless an
/// `EditEntries` replaced the range holding this `ToolCalls` entry, because a
/// replacement inherits the `EditEntries` sequence: a round journaled before
/// such an edit no longer matches the entry that asks for it, and the row
/// reads as absent.
async fn invocation_round(
    js: &async_nats::jetstream::Context,
    session: &str,
    call_id: &str,
) -> anyhow::Result<Option<u64>> {
    let entries = crate::nats_session_log::NatsSessionLog::new(js.clone(), session)
        .load_events_latest_async()
        .await?;
    Ok(entries.iter().rev().find_map(|(seq, entry)| match entry {
        harnx_core::session::SessionLogEntry::ToolCalls { calls, .. }
            if calls.iter().any(|call| call.id.as_deref() == Some(call_id)) =>
        {
            Some(*seq)
        }
        _ => None,
    }))
}

fn validate_replay_route(
    route: &RegisteredTool,
    record: &harnx_toolset_server::invocation_journal::RecordedInvocation,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        route.server == record.server && route.raw_name == record.request.tool,
        "replayed tool server identity changed"
    );
    Ok(())
}

fn recovered_output(
    result: Result<ToolProviderOutput, ToolError>,
) -> anyhow::Result<Option<ToolProviderOutput>> {
    match result {
        Ok(result) => Ok(Some(result)),
        Err(ToolError::Recoverable(error)) => Ok(Some(ToolProviderOutput::new(
            serde_json::json!({"is_error": true, "error": error.to_string()}),
        ))),
        Err(ToolError::Fatal(error)) => Err(error),
    }
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
