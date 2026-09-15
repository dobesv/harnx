use super::*;

impl NatsToolProvider {
    pub(super) async fn record_invocation(
        &self,
        request: &ToolRequest,
        tool_name: &str,
        server: &str,
    ) -> anyhow::Result<()> {
        if self.execution_control.is_none() {
            return Ok(());
        }
        let session = request
            .parent_session_id
            .as_deref()
            .context("controlled tool session missing")?;
        let js = async_nats::jetstream::new(self.client.clone());
        let round = match request.tool_call_id.as_deref() {
            Some(call_id) => invocation_round(&js, session, call_id).await?,
            None => 0,
        };
        harnx_toolset_server::invocation_journal::InvocationJournal::ensure(&js)
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
        let journal =
            harnx_toolset_server::invocation_journal::InvocationJournal::ensure(&js).await?;
        let Some(record) = journal.find(session, round, call_id).await? else {
            return Ok(None);
        };
        anyhow::ensure!(record.tool_name == call.name, "replayed tool name changed");
        let (store, request) = self.replay_request(&record, replay).await?;
        harnx_toolset_server::reply_fence::admit_recovery(store, &request).await?;
        if let Some(reply) = journal.completed_reply(&request).await? {
            let saved = journal
                .committed_reply(&request)
                .await?
                .context("reply proof missing")?;
            acknowledge_saved_handler(store, &saved).await?;
            return recovered_output(Self::decode_recorded_reply(
                reply,
                ToolObservationProvenance::new(
                    record.server_scope,
                    record.server,
                    record.request.tool,
                    record.request.call_id,
                ),
            ));
        }
        self.dispatch_replay((record, request), replay, abort).await
    }

    async fn dispatch_replay(
        &self,
        invocation: (
            harnx_toolset_server::invocation_journal::RecordedInvocation,
            ToolRequest,
        ),
        replay: harnx_core::tool::ToolReplay<'_>,
        abort: &AbortSignal,
    ) -> anyhow::Result<Option<ToolProviderOutput>> {
        let (record, mut request) = invocation;
        let store = &self
            .execution_control
            .as_ref()
            .context("replay execution missing")?
            .0;
        let journal = harnx_toolset_server::invocation_journal::InvocationJournal::ensure(
            &async_nats::jetstream::new(self.client.clone()),
        )
        .await?;
        let route = self
            .resolve_route(&record.tool_name)
            .context("replayed tool unavailable; keeping the pending invocation for recovery")?;
        // Scope identifies a process lifetime and must change across restart.
        // The logical server identity and raw tool must still be the same.
        validate_replay_route(&route, &record)?;
        let consumer = request
            .replay_execution
            .as_ref()
            .context("replay identity missing")?
            .consumer
            .clone();
        harnx_toolset_server::invocation_admission::prepare_replay(store, &mut request, consumer)
            .await?;
        let id = request.call_id.clone();
        harnx_toolset_server::reply_fence::admit(store, &request).await?;
        let receiving_request = request.clone();
        let pending =
            self.prepare_recorded_request(request, &route)
                .map_err(|error| match error {
                    ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
                })?;
        replay
            .authorization
            .context("replay requires a live session lease")?
            .revalidate()
            .await?;
        anyhow::ensure!(!abort.aborted(), "tool replay aborted before dispatch");
        let message = self
            .await_response(pending, abort)
            .await
            .map_err(|error| match error {
                ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
            })?;
        if let Some(reply) = journal.completed_reply(&receiving_request).await? {
            return recovered_output(Self::decode_recorded_reply(
                reply,
                ToolObservationProvenance::new(
                    self.instance_id.to_string(),
                    route.server,
                    route.raw_name,
                    id,
                ),
            ));
        }
        recovered_output(self.decode_reply(message, id, route))
    }

    async fn replay_request(
        &self,
        record: &harnx_toolset_server::invocation_journal::RecordedInvocation,
        replay: harnx_core::tool::ToolReplay<'_>,
    ) -> anyhow::Result<(&harnx_execution_control::ExecutionStore, ToolRequest)> {
        let (store, parent) = self
            .execution_control
            .as_ref()
            .context("replay requires execution control")?;
        harnx_toolset_server::reply_fence::check_request_stop(store, &record.request).await?;
        let original = record
            .request
            .execution
            .as_ref()
            .context("legacy replay has no retained generation authority")?;
        anyhow::ensure!(
            parent == original.consumer.operation(),
            "replay cannot adopt another generation"
        );
        let (store, owner) = self.replay_owner(replay).await?;
        let consumer = store
            .gate_context(original.consumer.gate_root(), parent)
            .await?;
        anyhow::ensure!(consumer.owner() == &owner, "replay gate owner mismatch");
        let mut request = record.request.clone();
        request.replay = Some(owner);
        request.replay_execution = Some(harnx_toolset::ToolExecution {
            producer: original.producer.clone(),
            consumer: consumer.clone(),
        });
        Ok((store, request))
    }

    async fn replay_owner(
        &self,
        replay: harnx_core::tool::ToolReplay<'_>,
    ) -> anyhow::Result<(
        &harnx_execution_control::ExecutionStore,
        harnx_execution_control::Owner,
    )> {
        let (store, parent) = self
            .execution_control
            .as_ref()
            .context("replay requires an execution owner")?;
        anyhow::ensure!(
            parent.session_id == replay.session_id,
            "replay session mismatch"
        );
        let owner = store
            .get(parent)
            .await?
            .context("replay parent missing")?
            .owner
            .context("replay parent has no owner")?;
        // A stale worker must not borrow its replacement's owner from KV.
        // Lease renewals can raise the caller's fence beyond its graph claim.
        anyhow::ensure!(
            replay.worker_id == Some(owner.instance_id.as_str())
                && replay.fence_token.is_some_and(|fence| owner.fence <= fence),
            "replay requester no longer owns the parent execution"
        );
        Ok((store, owner))
    }
}

async fn invocation_round(
    js: &async_nats::jetstream::Context,
    session: &str,
    call_id: &str,
) -> anyhow::Result<u64> {
    let entries = crate::nats_session_log::NatsSessionLog::new(js.clone(), session)
        .load_events_latest_async()
        .await?;
    let round = entries
        .iter()
        .rev()
        .find_map(|(seq, entry)| match entry {
            harnx_core::session::SessionLogEntry::ToolCalls { calls, .. }
                if calls.iter().any(|call| call.id.as_deref() == Some(call_id)) =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .context("tool invocation has no durable call round")?;
    Ok(round)
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

async fn acknowledge_saved_handler(
    store: &harnx_execution_control::ExecutionStore,
    saved: &harnx_toolset_server::invocation_journal::CommittedReply,
) -> anyhow::Result<()> {
    // Only the committed producer returned. Never acknowledge a replacement
    // handler using an old reply. Descendants remain physical blockers.
    let reference = saved.producer.operation();
    let Some(operation) = store.get(reference).await? else {
        return Ok(());
    };
    if !operation.state.is_terminal() && operation.owner.as_ref() == Some(saved.producer.owner()) {
        store
            .owner_stopped(reference, saved.producer.owner())
            .await?;
    }
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
