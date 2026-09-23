use super::{
    json_rpc_error, json_rpc_response, session_exists, AppResponse, SessionCommand, SessionHandle,
    SessionKey, SessionRegistry, JSON_RPC_UNKNOWN_SESSION_CODE,
};
use harnx_runtime::nats_session::CompactSubmit;
use http::StatusCode;
use serde_json::{json, Value};

type SessionContext<'a> = (
    &'a harnx_runtime::config::Config,
    &'a SessionRegistry,
    SessionKey,
);

/// `session/compact` takes no parameters: a compaction request always targets the
/// session's current transcript, whatever it is, and the result names the
/// `compaction_id` the session log accepted.
pub(super) async fn handle(id: Value, session: SessionContext<'_>) -> anyhow::Result<AppResponse> {
    let (config, registry, key) = session;
    if !registry.has_session(&key) && !session_exists(config, &key).await? {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(
                id,
                JSON_RPC_UNKNOWN_SESSION_CODE,
                "session not found",
                Some(json!({ "agent": key.agent(), "session": key.session })),
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

async fn submit(handle: &SessionHandle) -> Result<CompactSubmit, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Compact { reply: reply_tx })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped the compaction reply".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compact_returns_the_compaction_id_the_actor_accepted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let receiver = tokio::spawn(async move {
            let command = rx.recv().await.expect("compact command");
            let SessionCommand::Compact { reply } = command else {
                panic!("expected a compact command");
            };
            reply
                .send(Ok(CompactSubmit::Submitted {
                    compaction_id: "comp-42".to_string(),
                }))
                .expect("compact acknowledgement");
        });

        let outcome = submit(&handle).await.expect("route the compact");
        assert_eq!(
            outcome,
            CompactSubmit::Submitted {
                compaction_id: "comp-42".to_string()
            }
        );
        receiver.await.expect("command receiver");
    }
}
