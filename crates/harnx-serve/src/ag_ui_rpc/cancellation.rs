use super::{
    json_rpc_error, json_rpc_response, session_exists, AppResponse, SessionCommand, SessionHandle,
    SessionKey, SessionRegistry, JSON_RPC_UNKNOWN_SESSION_CODE,
};
use http::StatusCode;
use serde_json::{json, Value};

type RpcRequest<'a> = (&'a str, Value, Option<Value>);
type SessionContext<'a> = (
    &'a harnx_runtime::config::Config,
    &'a SessionRegistry,
    SessionKey,
);

#[derive(Clone, Copy)]
enum Mutation {
    Request,
    Abandon,
}

impl Mutation {
    fn from_method(method: &str) -> Self {
        match method {
            "session/abandon_cancellation" => Self::Abandon,
            _ => Self::Request,
        }
    }
}

pub(super) async fn handle(
    request: RpcRequest<'_>,
    session: SessionContext<'_>,
) -> anyhow::Result<AppResponse> {
    let (method, id, params) = request;
    let (config, registry, key) = session;
    if !registry.has_session(&key) && !session_exists(config, &key).await {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(
                id,
                JSON_RPC_UNKNOWN_SESSION_CODE,
                "session not found",
                Some(json!({ "agent": key.agent, "session": key.session })),
            ),
        );
    }

    let request: harnx_execution_control::CancelRequest =
        match serde_json::from_value(params.unwrap_or_else(|| json!({}))) {
            Ok(request) => request,
            Err(error) => {
                return json_rpc_response(
                    StatusCode::BAD_REQUEST,
                    json_rpc_error(id, -32602, &error.to_string(), None),
                )
            }
        };
    let mutation = Mutation::from_method(method);
    let expected_execution_id = match (mutation, request.expected_execution_id) {
        (Mutation::Abandon, None) => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(id, -32602, "expected_execution_id is required", None),
            )
        }
        (_, expected_execution_id) => expected_execution_id,
    };

    let handle = registry.get_or_spawn(key);
    match submit(&handle, mutation, expected_execution_id).await {
        Ok(receipt) => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": receipt }),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, -32003, &message, None),
        ),
    }
}

async fn submit(
    handle: &SessionHandle,
    mutation: Mutation,
    expected_execution_id: Option<String>,
) -> Result<harnx_execution_control::CancelReceipt, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let command = match mutation {
        Mutation::Request => SessionCommand::Cancel {
            reply: reply_tx,
            expected_execution_id,
        },
        Mutation::Abandon => SessionCommand::AbandonCancellation {
            reply: reply_tx,
            expected_execution_id: expected_execution_id
                .expect("abandonment execution ID validated before actor dispatch"),
        },
    };
    handle
        .tx
        .send(command)
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped cancellation reply".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandonment_routes_observed_execution_to_actor() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let receiver = tokio::spawn(async move {
            let command = rx.recv().await.expect("abandonment command");
            let SessionCommand::AbandonCancellation {
                expected_execution_id,
                reply,
            } = command
            else {
                panic!("expected cancellation abandonment command");
            };
            assert_eq!(expected_execution_id, "execution-42");
            let mut receipt = harnx_execution_control::CancelReceipt::idle();
            receipt.cancelled = true;
            receipt.abandoned = true;
            reply
                .send(Ok(receipt))
                .expect("abandonment acknowledgement");
        });

        let receipt = submit(&handle, Mutation::Abandon, Some("execution-42".into()))
            .await
            .expect("route cancellation abandonment");
        assert!(receipt.abandoned);
        receiver.await.expect("command receiver");
    }
}
