use super::*;
use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::json;

#[test]
fn replay_route_preserves_logical_identity_across_process_scopes() -> anyhow::Result<()> {
    let record = serde_json::from_value(json!({
        "request": {"call_id": "original", "operation_id": "original", "tool": "echo", "args": {}},
        "tool_name": "echo", "server": "configured-server", "server_scope": "departed-process",
        "tool_round": 1, "started_at_ms": 1, "reply": null
    }))?;
    let route = RegisteredTool {
        server: "configured-server".into(),
        selector_server: "configured-server".into(),
        raw_name: "echo".into(),
        request_timeout: None,
    };
    validate_replay_route(&route, &record)?;
    let mut wrong_server = route.clone();
    wrong_server.server = "new-alias-winner".into();
    assert!(validate_replay_route(&wrong_server, &record).is_err());
    let mut wrong_tool = route;
    wrong_tool.raw_name = "different-tool".into();
    assert!(validate_replay_route(&wrong_tool, &record).is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_reply_recovers_without_a_registered_server() -> anyhow::Result<()> {
    saved_reply_case(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_reply_from_interrupted_pruned_generation_is_typed_and_never_dispatched(
) -> anyhow::Result<()> {
    saved_reply_case(true).await
}

async fn saved_reply_case(interrupted: bool) -> anyhow::Result<()> {
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
    if interrupted {
        let owner = store.get(&operation).await?.unwrap().owner.unwrap();
        store.owner_stopped(&operation, &owner).await?;
        store.status(&parent.reference).await?;
        assert!(store.get(&operation).await?.is_none());
        store
            .cancel_operation(&parent.reference, Some("runtime-stop"), false)
            .await?;
    }
    let provider = saved_reply_provider(client, store.clone(), parent.reference).await?;
    assert_unproved_wire_reply_rejected(&provider)?;
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
        .await;
    if interrupted {
        assert!(result
            .unwrap_err()
            .is::<harnx_execution_control::Interrupted>());
        let _ = nats.kill();
        let _ = nats.wait();
        return Ok(());
    }
    let result = result?.context("recovered reply")?;
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
        execution: Some(
            harnx_toolset_server::invocation_admission::capture(store, &operation).await?,
        ),
        replay_execution: None,
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

async fn saved_reply_provider(
    client: async_nats::Client,
    store: ExecutionStore,
    parent: OperationRef,
) -> anyhow::Result<NatsToolProvider> {
    let instance_id = ServerScope::new();
    let subscription = client.subscribe(instance_id.control_subject()).await?;
    Ok(NatsToolProvider {
        client,
        instance_id,
        parent_session_id: Some("parent".into()),
        execution_control: Some((store, parent)),
        tools: HashMap::new(),
        registrations: Vec::new(),
        active_package: None,
        declarations: Vec::new(),
        registry: None,
        _control_subscription: Mutex::new(subscription),
        in_flight: NatsInFlightCalls::default(),
    })
}

fn assert_unproved_wire_reply_rejected(provider: &NatsToolProvider) -> anyhow::Result<()> {
    let reply = ToolReply {
        call_id: "original".into(),
        result: Ok(json!("unproved wire success")),
    };
    let message = async_nats::Message {
        subject: "reply".into(),
        reply: None,
        payload: serde_json::to_vec(&reply)?.into(),
        headers: None,
        status: None,
        description: None,
        length: 0,
    };
    let route = RegisteredTool {
        server: "retired".into(),
        selector_server: "retired".into(),
        raw_name: "echo".into(),
        request_timeout: None,
    };
    let error = provider
        .decode_reply(message, "original".into(), route)
        .unwrap_err();
    assert!(
        matches!(error, ToolError::Fatal(error) if error.to_string() == "tool reply has no committed proof")
    );
    Ok(())
}
