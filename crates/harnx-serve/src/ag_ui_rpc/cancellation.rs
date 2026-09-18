use super::{
    json_rpc_error, json_rpc_response, session_exists, AppResponse, SessionCommand, SessionHandle,
    SessionKey, SessionRegistry, JSON_RPC_UNKNOWN_SESSION_CODE,
};
use harnx_runtime::nats_session::InterruptOutcome;
use http::StatusCode;
use serde_json::{json, Value};

type SessionContext<'a> = (
    &'a harnx_runtime::config::Config,
    &'a SessionRegistry,
    SessionKey,
);

/// `session/cancel` takes no parameters: an interrupt always targets the
/// session's current turn, whatever it is, and the result names the `Cancel`
/// the session log accepted.
pub(super) async fn handle(id: Value, session: SessionContext<'_>) -> anyhow::Result<AppResponse> {
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

    let handle = registry.get_or_spawn(key);
    match submit(&handle).await {
        Ok(outcome) => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": outcome }),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, -32003, &message, None),
        ),
    }
}

async fn submit(handle: &SessionHandle) -> Result<InterruptOutcome, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Cancel { reply: reply_tx })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped the interrupt reply".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_returns_the_sequence_the_actor_accepted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let receiver = tokio::spawn(async move {
            let command = rx.recv().await.expect("interrupt command");
            let SessionCommand::Cancel { reply } = command else {
                panic!("expected an interrupt command");
            };
            reply
                .send(Ok(InterruptOutcome::Accepted { cancel_seq: 42 }))
                .expect("interrupt acknowledgement");
        });

        let outcome = submit(&handle).await.expect("route the interrupt");
        assert_eq!(outcome, InterruptOutcome::Accepted { cancel_seq: 42 });
        receiver.await.expect("command receiver");
    }
}
