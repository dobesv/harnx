//! T4.3 wire contract: a native toolset returns `input_required` and the
//! server carries it beside a compatibility error, so older callers fail
//! visibly instead of treating a suspended call as complete. The rerun
//! request carries `input_responses` and `request_state` beside the
//! arguments, and the tool sees them through its invocation context only.
use crate::common;
use anyhow::Result;
use common::{request_headers, tool_spec};
use harnx_toolset::{
    ToolErrorPayload, ToolInputRequired, ToolInputResponse, ToolInvocation, ToolInvokeError,
    ToolReply, ToolRequest, Toolset, INPUT_REQUIRED_COMPAT_ERROR,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct Gated {
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait::async_trait]
impl Toolset for Gated {
    fn name(&self) -> &str {
        "gated"
    }

    fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
        vec![tool_spec("ask").with_input_required()]
    }

    async fn invoke(
        &self,
        _tool: &str,
        _args: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        unreachable!("the server always invokes with context")
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let responses = invocation.context.input_responses.clone();
        self.seen.lock().expect("seen").push(json!({
            "args": invocation.args,
            "responses": responses
                .iter()
                .map(|(key, response)| (
                    key.clone(),
                    json!({"action": response.action, "data": response.data}),
                ))
                .collect::<serde_json::Map<String, Value>>(),
            "state": invocation.context.request_state,
        }));
        if responses.is_empty() {
            return Err(ToolInvokeError::InputRequired(Box::new(
                ToolInputRequired::confirm("ask-key", "Allow the gated call?", json!({"round": 1})),
            )));
        }
        Ok(json!({
            "action": responses.get("ask-key").map(|r| r.action.clone()),
            "state": invocation.context.request_state,
        }))
    }
}

async fn call(
    server: &common::ServedToolset,
    client: &async_nats::Client,
    request: &ToolRequest,
) -> Result<ToolReply> {
    let subject = server.tool_subject(&request.tool);
    let payload = serde_json::to_vec(request)?;
    let headers = request_headers(&request.call_id, &request.call_id);
    let message = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        client.send_request(
            subject,
            async_nats::Request::new()
                .headers(headers)
                .payload(payload.into())
                .timeout(None),
        ),
    )
    .await??;
    Ok(serde_json::from_slice(&message.payload)?)
}

fn challenge_request(session: &str, call_id: &str, responses: bool) -> ToolRequest {
    let mut request = ToolRequest {
        tool: "ask".into(),
        args: json!({"value": 42}),
        ..common::request(session, call_id)
    };
    if responses {
        request.input_responses = Some(BTreeMap::from([(
            "ask-key".to_string(),
            ToolInputResponse {
                action: "approve".into(),
                data: Default::default(),
                note: None,
            },
        )]));
        request.request_state = Some(json!({"round": 1}));
    }
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_input_required_wire_challenge_then_rerun_beside_arguments() -> Result<()> {
    let toolset = Gated::default();
    let (server, client, _journal) = common::serve(toolset.clone()).await;

    // First call: the tool challenges instead of finishing.
    let reply = call(
        &server,
        &client,
        &challenge_request("wire-session", "ask-1", false),
    )
    .await?;
    assert_eq!(
        reply.result,
        Err(ToolErrorPayload::Recoverable(
            INPUT_REQUIRED_COMPAT_ERROR.to_string()
        )),
        "older callers must see a visible failure, not a silent success"
    );
    let input = reply
        .input_required
        .as_ref()
        .expect("challenge carried beside the compatibility error");
    assert_eq!(input.key, "ask-key");
    assert_eq!(input.kind, ToolInputRequired::KIND_CONFIRM);
    assert_eq!(input.message.as_deref(), Some("Allow the gated call?"));
    assert_eq!(input.request_state, json!({"round": 1}));

    // Rerun: responses and state ride beside the arguments.
    let reply = call(
        &server,
        &client,
        &challenge_request("wire-session", "ask-2", true),
    )
    .await?;
    assert!(reply.input_required.is_none());
    assert_eq!(
        reply.result,
        Ok(json!({"action": Some("approve".to_string()), "state": json!({"round": 1})}))
    );

    // The tool saw the same untouched arguments both times.
    let seen = toolset.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 2, "challenge then rerun: {seen:?}");
    for entry in &seen {
        assert_eq!(entry["args"], json!({"value": 42}));
    }
    assert!(seen[0]["responses"]
        .as_object()
        .is_some_and(|m| m.is_empty()));
    assert_eq!(seen[0]["state"], Value::Null);
    assert_eq!(seen[1]["responses"]["ask-key"]["action"], "approve");
    assert_eq!(seen[1]["state"], json!({"round": 1}));
    Ok(())
}
