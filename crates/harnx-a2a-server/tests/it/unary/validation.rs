use super::fixtures::*;
use anyhow::Result;
use harnx_a2a_server::input_map::InputLimits;
use harnx_a2a_server::store::TaskSeed;
use serde_json::{json, Value};

async fn persist_canonical_completed_task(http: &Http) -> Result<String> {
    let session = http.h.session(None, &crate::support::alice()).await?;
    let id = harnx_a2a_server::store::format_task_id(
        session.session_id(),
        "01234567-89ab-cdef-0123-456789abcdef",
    );
    let task = serde_json::from_value(json!({
        "id":id, "contextId":session.session_id(),
        "status":{"state":"TASK_STATE_COMPLETED"}
    }))?;
    http.h
        .store
        .create_task(
            session.storage_key(),
            TaskSeed {
                task,
                user_msg_id: String::new(),
                user_msg_seq: 0,
                execution_id: String::new(),
            },
        )
        .await?;
    Ok(id)
}

async fn assert_malformed_task_methods(http: &Http, malformed: &str) -> Result<()> {
    for method in ["GetTask", "CancelTask", "SubscribeToTask"] {
        code(&http.rpc(method, json!({"id":malformed})).await?, -32001);
    }
    let mut send = message("noncanonical-task-id", None);
    send["taskId"] = json!(malformed);
    for method in ["SendMessage", "SendStreamingMessage"] {
        code(&http.rpc(method, json!({"message":send})).await?, -32001);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noncanonical_task_ids_return_not_found_for_all_task_methods() -> Result<()> {
    let http = Http::start().await?;
    let id = persist_canonical_completed_task(&http).await?;
    let existing = http.rpc("GetTask", json!({"id":id})).await?;
    assert_eq!(existing["result"]["id"], id);
    let (context, uuid) = harnx_a2a_server::store::parse_task_id(&id)?;
    for noncanonical in [
        uuid.to_ascii_uppercase(),
        uuid.replace('-', ""),
        format!("{{{uuid}}}"),
        format!("urn:uuid:{uuid}"),
    ] {
        let malformed = harnx_a2a_server::store::format_task_id(context, &noncanonical);
        assert_malformed_task_methods(&http, &malformed).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_raw_media_type_returns_content_type_not_supported() -> Result<()> {
    let http = Http::start().await?;
    let response = http
        .rpc(
            "SendMessage",
            json!({
                "message": {
                    "messageId": "unsupported-media",
                    "role": "ROLE_USER",
                    "parts": [{"raw": "AA==", "mediaType": "application/x-unsupported"}]
                }
            }),
        )
        .await?;
    code(&response, -32005);
    assert_eq!(
        response["error"]["message"],
        "unsupported raw mediaType: application/x-unsupported"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_data_returns_same_invalid_params_error() -> Result<()> {
    let http = Http::start().await?;
    let mut message = message("oversized-data", None);
    message["parts"] = json!([{"data": {"value": "x".repeat(65536)}}]);
    let response = http.rpc("SendMessage", json!({"message": message})).await?;
    code(&response, -32602);
    assert_eq!(
        response["error"]["message"],
        "unsupported or oversized message parts"
    );
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

async fn session_keys(http: &Http) -> Result<std::collections::BTreeSet<String>> {
    use futures::TryStreamExt;
    Ok(http
        .h
        .metadata
        .kv_store()
        .keys()
        .await?
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .filter(|key| key.starts_with("sessions/"))
        .collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_first_turn_parts_never_create_session_metadata_or_binding() -> Result<()> {
    let http = Http::start().await?;
    let before = session_keys(&http).await?;
    assert!(
        before.is_empty(),
        "unexpected initial session keys: {before:?}"
    );
    for method in ["SendMessage", "SendStreamingMessage"] {
        for (name, parts, expected) in [
            (
                "oversized-data",
                json!([{"data": {"value": "x".repeat(InputLimits::default().max_data_part_bytes)}}]),
                -32602,
            ),
            (
                "unsupported-media",
                json!([{"raw": "AA==", "mediaType": "application/x-unsupported"}]),
                -32005,
            ),
            (
                "invalid-utf8",
                json!([{"raw": "/w==", "mediaType": "text/plain"}]),
                -32602,
            ),
        ] {
            let mut message = message(&format!("{method}-{name}"), None);
            message["parts"] = parts;
            let response = http.rpc(method, json!({"message": message})).await?;
            code(&response, expected);
            // The A2A binding is stored in sessions/*/meta. Check the whole
            // session prefix so a partial allocation cannot pass this assertion.
            assert_eq!(
                session_keys(&http).await?,
                before,
                "{method} {name} left session keys after rejection"
            );
        }
    }
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

async fn assert_working_validation_precedence(http: &Http, invalid: &Value) -> Result<()> {
    for method in ["SendMessage", "SendStreamingMessage"] {
        code(
            &http
                .rpc_call(HttpRpcCall::new(
                    &http.url,
                    "bob",
                    method,
                    json!({"message": invalid.clone()}),
                ))
                .await?,
            -32001,
        );
        code(
            &http.rpc(method, json!({"message": invalid})).await?,
            -32000,
        );
        let mut mismatch = invalid.clone();
        mismatch["messageId"] = json!("validation-order");
        let response = http.rpc(method, json!({"message": mismatch})).await?;
        code(&response, -32602);
        assert_eq!(
            response["error"]["message"],
            "messageId was already used with different parts"
        );
    }
    Ok(())
}

async fn assert_terminal_validation_precedence(http: &Http, invalid: &Value) -> Result<()> {
    for method in ["SendMessage", "SendStreamingMessage"] {
        let response = http.rpc(method, json!({"message": invalid})).await?;
        code(&response, -32602);
        assert_eq!(
            response["error"]["message"],
            "terminal tasks cannot be continued; send a new message with contextId"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_parts_do_not_override_owner_busy_or_terminal_errors() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("validation-order", None)).await?;
    let mut invalid = message("invalid-new", Some(&first["contextId"]));
    invalid["parts"] = json!([{"raw": "AA==", "mediaType": "application/x-unsupported"}]);
    assert_working_validation_precedence(&http, &invalid).await?;
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    invalid["taskId"] = first["id"].clone();
    assert_terminal_validation_precedence(&http, &invalid).await?;
    Ok(())
}
