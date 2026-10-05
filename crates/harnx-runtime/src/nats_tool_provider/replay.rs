use super::partial_result::attach_partial_result;
use super::*;
use harnx_core::partial_result::{output_with_partial_result, partial_result_of};
use harnx_toolset::ReplayAttempt;

impl NatsToolProvider {
    /// The invocation journal, opened by the first call that needs it and
    /// kept. Opening sends the server a request to create the bucket, which
    /// the cluster's meta leader answers, so a provider that opened it per
    /// call made every call wait on that. Opening also raises the bucket's
    /// replicas to the configured count when it finds fewer. Discovery builds
    /// a fresh provider whenever it refreshes, and every tool server keeps
    /// reconciling the bucket while it runs.
    pub(super) async fn journal(
        &self,
    ) -> anyhow::Result<&harnx_toolset_server::invocation_journal::InvocationJournal> {
        self.journal
            .get_or_try_init(|| async {
                let js = async_nats::jetstream::new(self.client.clone());
                harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
                    &js,
                    self.journal_replicas,
                )
                .await
            })
            .await
    }

    pub(super) async fn record_invocation(
        &self,
        request: &ToolRequest,
        (tool_name, server): (&str, &str),
    ) -> anyhow::Result<()> {
        if request.parent_session_id.is_none() {
            return Ok(());
        }
        // The request's round is the sequence of the `ToolCalls` entry that
        // made this call, and it keys the journal row. The dispatcher hands
        // it in: a fresh round's is the sequence its append returned, and a
        // resumed round's is the one its orphan detection read. A call
        // dispatched outside a durable round, such as a direct provider call
        // or a tool an operator invoked, has none, which is what zero
        // records. Nothing looks a zero row up by round: only a transcript
        // orphan is replayed or wound up, and whatever dispatched its calls
        // had the orphan's `ToolCalls` entry to take the round from.
        //
        // Wind-up and replay look the row up by the sequence they read from
        // the effective log, after `apply_log_mutations`. That agrees with
        // the round recorded here unless an `EditEntries` replaced the range
        // holding the `ToolCalls` entry since the dispatcher took its
        // sequence, because a replacement inherits the `EditEntries`
        // sequence: the row then no longer matches the entry that asks for
        // it, and reads as absent.
        self.journal()
            .await?
            .record(request, (tool_name, self.instance_id.as_str(), server))
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
        let journal = self.journal().await?;
        let Some(record) = journal.find(session, round, call_id).await? else {
            return Ok(None);
        };
        anyhow::ensure!(record.tool_name == call.name, "replayed tool name changed");
        if let Some(reply) = journal.completed_reply(&record.request).await? {
            return recovered_output(attach_partial_result(
                decode_journaled_reply(&record, reply),
                record.partial_result.as_ref(),
            ));
        }
        if let Some(context) = &record.request.run_context {
            let frozen: crate::nats_session_metadata::RunLimitsRecord =
                serde_json::from_value(context.snapshot.clone())?;
            if frozen.is_expired_at(chrono::Utc::now()) {
                return Err(
                    crate::nats_session_metadata::run_limits::DeadlineExpired::before_dispatch(
                        &frozen,
                    ),
                );
            }
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
        let pending = self
            .prepare_recorded_request(request.clone(), &route)
            .map_err(|error| match error {
                ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
            })?;
        if let Some(authorization) = replay.authorization {
            authorization.revalidate().await?;
        }
        anyhow::ensure!(!abort.aborted(), "tool replay aborted before dispatch");
        if let Some(context) = &request.run_context {
            let frozen: crate::nats_session_metadata::RunLimitsRecord =
                serde_json::from_value(context.snapshot.clone())?;
            if frozen.is_expired_at(chrono::Utc::now()) {
                return Err(
                    crate::nats_session_metadata::run_limits::DeadlineExpired::before_dispatch(
                        &frozen,
                    ),
                );
            }
        }
        let message = self
            .await_response(pending, abort)
            .await
            .map_err(|error| match error {
                ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
            })?;
        let decoded = self.decode_reply(message, id, route);
        recovered_output(self.with_recorded_partial_result(&request, decoded).await)
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
        Err(ToolError::Recoverable(error)) => {
            Ok(Some(ToolProviderOutput::new(output_with_partial_result(
                serde_json::json!({"is_error": true, "error": error.to_string()}),
                partial_result_of(&error),
            ))))
        }
        Err(ToolError::Fatal(error)) => Err(error),
    }
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
