use super::*;
use http_body_util::BodyExt;

mod durable;

fn params(action: &str) -> Value {
    json!({ "interaction_id": "first", "request_seq": 3,
        "response": { "action": action, "data": {}, "note": "operator note" } })
}

async fn body(response: AppResponse) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[test]
fn interaction_response_requires_exact_identity_and_confirm_shape() {
    for action in ["approve", "deny"] {
        assert!(parse_params(Some(params(action))).is_ok());
    }
    for invalid in [
        Value::Null,
        json!({}),
        json!({ "interaction_id": "first", "response": { "action": "approve", "data": {} } }),
        json!({ "interaction_id": "first", "request_seq": 0, "response": { "action": "approve", "data": {} } }),
        json!({ "interaction_id": "", "request_seq": 3, "response": { "action": "approve", "data": {} } }),
        json!({ "interaction_id": "first", "request_seq": 3, "response": { "action": "approve", "data": [] } }),
        json!({ "interaction_id": "first", "request_seq": 3, "response": { "action": "approve", "data": {"approved":true} } }),
        params("submit"),
    ] {
        assert!(
            parse_params(Some(invalid.clone())).is_err(),
            "accepted {invalid}"
        );
    }
}

#[tokio::test]
async fn interaction_response_maps_every_durable_outcome_without_bool_only_fallback() {
    use InteractionDecisionOutcome::*;
    for outcome in [Applied, AlreadyApplied] {
        let name = outcome_name(&outcome);
        let response = body(response(json!(7), Ok(outcome), None).unwrap()).await;
        assert_eq!(
            response["result"],
            json!({ "applied": true, "outcome": name })
        );
        assert_eq!(response["id"], 7);
    }
    for outcome in [WrongSequence, Stale, Conflicting, AfterComplete, LeaseLost] {
        let name = outcome_name(&outcome);
        let response = body(response(json!(7), Ok(outcome), None).unwrap()).await;
        assert_eq!(response["error"]["code"], -32004);
        assert_eq!(response["error"]["data"]["outcome"], name);
        assert!(response.get("result").is_none());
    }
}

#[tokio::test]
async fn interaction_response_routes_approve_and_deny_with_sequence_and_note() {
    for action in ["approve", "deny"] {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let task = tokio::spawn(async move {
            let SessionCommand::InteractionResponse {
                interaction_id,
                request_seq,
                response,
                reply,
            } = rx.recv().await.unwrap()
            else {
                panic!("expected InteractionResponse");
            };
            assert_eq!(interaction_id, "first");
            assert_eq!(request_seq, 3);
            assert_eq!(response.action, action);
            assert_eq!(response.note.as_deref(), Some("operator note"));
            reply.send(Ok(InteractionDecisionOutcome::Applied)).unwrap();
        });
        assert_eq!(
            route(&handle, parse_params(Some(params(action))).unwrap())
                .await
                .unwrap(),
            InteractionDecisionOutcome::Applied
        );
        task.await.unwrap();
    }
}
