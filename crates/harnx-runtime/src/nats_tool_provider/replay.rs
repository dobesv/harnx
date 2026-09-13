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
        let (Some(session), Some(call_id)) = (&request.parent_session_id, &request.tool_call_id)
        else {
            return Ok(());
        };
        let js = async_nats::jetstream::new(self.client.clone());
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
            worker_id,
            fence_token,
            authorization,
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
        let (store, parent) = self
            .execution_control
            .as_ref()
            .context("replay requires an execution owner")?;
        anyhow::ensure!(parent.session_id == session, "replay session mismatch");
        let owner = store
            .get(parent)
            .await?
            .context("replay parent missing")?
            .owner
            .context("replay parent has no owner")?;
        // A stale worker must not borrow its replacement's owner from KV.
        // Lease renewals can raise the caller's fence beyond its graph claim.
        anyhow::ensure!(
            worker_id == Some(owner.instance_id.as_str())
                && fence_token.is_some_and(|fence| owner.fence <= fence),
            "replay requester no longer owns the parent execution"
        );
        if let Some(reply) = record.reply {
            let reference =
                harnx_execution_control::OperationRef::new(session, &record.request.call_id);
            acknowledge_saved_handler(store, &reference).await?;
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
        let route = self
            .resolve_route(&record.tool_name)
            .context("replayed tool unavailable; keeping the pending invocation for recovery")?;
        let mut request = record.request;
        request.replay = Some(owner);
        let id = request.call_id.clone();
        // Registration can be interrupted after the durable request is saved.
        // Recreate only an absent operation, retaining existing terminal work.
        let reference = harnx_execution_control::OperationRef::new(session, &id);
        if store.get(&reference).await?.is_none() {
            self.register_operation(&id).await?;
        }
        let pending =
            self.prepare_recorded_request(request, &route)
                .map_err(|error| match error {
                    ToolError::Fatal(error) | ToolError::Recoverable(error) => error,
                })?;
        authorization
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
        recovered_output(self.decode_reply(message, id, route))
    }
}

async fn acknowledge_saved_handler(
    store: &harnx_execution_control::ExecutionStore,
    reference: &harnx_execution_control::OperationRef,
) -> anyhow::Result<()> {
    // The durable reply proves the handler returned. Descendants remain blockers.
    let Some(operation) = store.get(reference).await? else {
        return Ok(());
    };
    if !operation.state.is_terminal() {
        if let Some(owner) = &operation.owner {
            store.owner_stopped(reference, owner).await?;
        }
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
mod tests {
    use super::*;
    use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};
    use harnx_toolset_server::invocation_journal::InvocationJournal;
    use serde_json::json;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_reply_recovers_without_a_registered_server() -> anyhow::Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let client = async_nats::connect(&url).await?;
        let js = async_nats::jetstream::new(client.clone());
        let store = ExecutionStore::ensure(&js, 1).await?;
        let parent = store.session("parent", None, None).await?;
        let owner = Owner {
            instance_id: "worker".into(),
            fence: 1,
        };
        store.claim(&parent.reference, owner.clone()).await?;
        let operation = save_reply(&js, &store, &parent.reference).await?;
        let instance_id = ServerScope::new();
        let subscription = client.subscribe(instance_id.control_subject()).await?;
        let provider = NatsToolProvider {
            client,
            instance_id,
            parent_session_id: Some("parent".into()),
            execution_control: Some((store.clone(), parent.reference)),
            tools: HashMap::new(),
            registrations: Vec::new(),
            active_package: None,
            declarations: Vec::new(),
            registry: None,
            _control_subscription: Mutex::new(subscription),
            in_flight: NatsInFlightCalls::default(),
        };
        let call = harnx_core::tool::ToolCall::new(
            "retired_echo".into(),
            json!({}),
            Some("model-call".into()),
            None,
        );
        assert!(!provider.has_tool(&call.name));
        let result = provider
            .replay_recorded_call(
                harnx_core::tool::ToolReplay {
                    session_id: "parent",
                    tool_round: 5,
                    call: &call,
                    worker_id: Some("worker"),
                    fence_token: Some(1),
                    authorization: None,
                },
                &harnx_core::abort::create_abort_signal(),
            )
            .await?
            .context("recovered reply")?;
        assert_eq!(result.value, json!({"answer": "saved"}));
        let provenance = result.execution_context.unwrap().provenance.unwrap();
        assert_eq!(provenance.server_scope, "original-scope");
        assert_eq!(provenance.server_identity, "retired");
        assert_eq!(
            store.get(&operation).await?.unwrap().state,
            OperationState::Completed
        );
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }
    async fn save_reply(
        js: &async_nats::jetstream::Context,
        store: &ExecutionStore,
        parent: &OperationRef,
    ) -> anyhow::Result<OperationRef> {
        let operation = OperationRef::new("parent", "original");
        store.child(operation.clone(), parent.clone()).await?;
        store
            .claim(&operation, Owner::invocation("retired"))
            .await?;
        let request = ToolRequest {
            replay: None,
            call_id: "original".into(),
            operation_id: "original".into(),
            tool: "echo".into(),
            args: json!({}),
            parent_session_id: Some("parent".into()),
            tool_call_id: Some("model-call".into()),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(js).await?;
        journal
            .record(&request, ("retired_echo", "original-scope", "retired"), 5)
            .await?;
        journal
            .complete(
                &request,
                ToolReply {
                    call_id: "original".into(),
                    result: Ok(json!({"answer": "saved", "_meta": {
                        EXECUTION_CONTEXT_NAMESPACE: harnx_core::execution_context::ExecutionContextObservation::observe(
                            std::path::Path::new("/original/workspace"), std::path::Path::new("/original/workspace"))
                    }})),
                },
            )
            .await?;
        Ok(operation)
    }
}
