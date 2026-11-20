use super::*;
use crate::runner_test_support::Logs;
use a2a_lf::{Part, Role, StreamResponse, TaskStatusUpdateEvent};
use futures::{stream, StreamExt};
use harnx_core::abort::create_abort_signal;
use serde_json::{json, Value};

fn capture() -> Logs {
    harnx_core::require_nextest();
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .try_init()
        .unwrap();
    logs
}

fn records(logs: &Logs) -> Vec<Value> {
    logs.text()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["target"] == TARGET)
        .map(|event| event["fields"].clone())
        .collect()
}

fn task(state: TaskState) -> Task {
    let mut task: Task = serde_json::from_value(json!({
        "id":"task", "contextId":"context", "status":{"state":"TASK_STATE_WORKING"},
        "history":[{"messageId":"reply", "role":"ROLE_AGENT", "parts":[{"text":"private response"}]}]
    })).unwrap();
    task.status.state = state;
    task
}

fn event(state: TaskState) -> Result<StreamResponse, A2AError> {
    Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: "task".into(),
        context_id: "context".into(),
        status: task(state).status,
        metadata: None,
    }))
}

#[test]
fn unary_completion_logs_only_metadata_once() {
    let logs = capture();
    let mut message = Message::new(Role::User, vec![Part::text("private prompt")]);
    message.message_id = "send-id".into();
    let result = RequestLog::new("jira", "SendMessage")
        .with_message(&message)
        .complete(
            Ok(SendMessageResponse::Task(task(TaskState::Completed))),
            RequestLog::send_response,
        );
    assert!(result.is_ok());
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["export"], "jira");
    assert_eq!(events[0]["method"], "SendMessage");
    assert_eq!(events[0]["task_id"], "task");
    assert_eq!(events[0]["context_id"], "context");
    assert_eq!(events[0]["message_id"], "send-id");
    assert_eq!(events[0]["state"], "Completed");
    assert!(events[0]["duration_ms"].as_f64().unwrap() >= 0.0);
    assert!(!logs.text().contains("private prompt"));
    assert!(!logs.text().contains("private response"));
}

#[tokio::test]
async fn stream_completion_counts_events_and_defers_log_until_eof() {
    let logs = capture();
    let inner = stream::iter([
        Ok(StreamResponse::Task(task(TaskState::Working))),
        event(TaskState::Completed),
    ])
    .boxed();
    let mut stream = RequestLog::new("jira", "SendStreamingMessage")
        .streaming(create_abort_signal())
        .stream(Ok(inner))
        .unwrap();
    assert!(records(&logs).is_empty());
    assert!(stream.next().await.is_some());
    assert!(records(&logs).is_empty());
    assert!(stream.next().await.is_some());
    assert!(records(&logs).is_empty());
    assert!(stream.next().await.is_none());
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["events_sent"], 2);
    assert_eq!(events[0]["end_reason"], "terminal");
    assert_eq!(events[0]["state"], "Completed");
    assert_eq!(events[0]["task_id"], "task");
    assert_eq!(events[0]["context_id"], "context");
    assert!(events[0]["duration_ms"].is_number());
    assert!(!logs.text().contains("private response"));
}

#[tokio::test]
async fn stream_client_disconnect_logs_latest_state_without_canceling_task() {
    let logs = capture();
    let abort = create_abort_signal();
    let inner = stream::once(async { Ok(StreamResponse::Task(task(TaskState::Working))) })
        .chain(stream::pending())
        .boxed();
    let mut stream = RequestLog::new("jira", "SubscribeToTask")
        .streaming(abort.clone())
        .stream(Ok(inner))
        .unwrap();
    assert!(stream.next().await.is_some());
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["events_sent"], 1);
    assert_eq!(events[0]["end_reason"], "client_disconnect");
    assert_eq!(events[0]["state"], "Working");
    assert!(!abort.aborted());
}

#[tokio::test]
async fn stream_shutdown_wakes_pending_stream_and_logs_shutdown() {
    let logs = capture();
    let abort = create_abort_signal();
    let mut stream = RequestLog::new("jira", "SubscribeToTask")
        .with_task("task")
        .streaming(abort.clone())
        .stream(Ok(stream::pending().boxed()))
        .unwrap();
    let next = stream.next();
    tokio::pin!(next);
    assert!(futures::poll!(&mut next).is_pending());
    abort.set_ctrlc();
    assert!(next.await.is_none());
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["events_sent"], 0);
    assert_eq!(events[0]["end_reason"], "server_shutdown");
}

#[tokio::test]
async fn stream_interrupted_and_terminal_boundary_survive_drop_without_eof_poll() {
    let logs = capture();
    for (state, reason) in [
        (TaskState::InputRequired, "interrupted"),
        (TaskState::AuthRequired, "interrupted"),
        (TaskState::Completed, "terminal"),
    ] {
        let inner = stream::iter([event(state)]).boxed();
        let mut stream = RequestLog::new("jira", "SubscribeToTask")
            .streaming(create_abort_signal())
            .stream(Ok(inner))
            .unwrap();
        stream.next().await.unwrap().unwrap();
        drop(stream);
        let events = records(&logs);
        assert_eq!(events.last().unwrap()["events_sent"], 1);
        assert_eq!(events.last().unwrap()["end_reason"], reason);
    }
    assert_eq!(records(&logs).len(), 3);
}

#[tokio::test]
async fn stream_setup_and_delivery_errors_are_logged() {
    let logs = capture();
    let error = A2AError::new(-32001, "task not found");
    let result = RequestLog::new("jira", "SubscribeToTask")
        .with_task("missing")
        .streaming(create_abort_signal())
        .stream(Err(error));
    assert!(result.is_err());
    let inner = stream::iter([Err(A2AError::internal(
        "task stream interrupted; reconnect",
    ))])
    .boxed();
    let mut stream = RequestLog::new("jira", "SubscribeToTask")
        .streaming(create_abort_signal())
        .stream(Ok(inner))
        .unwrap();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["events_sent"], 0);
    assert_eq!(events[0]["error_code"], -32001);
    assert_eq!(events[0]["error_message"], "task not found");
    assert_eq!(events[1]["events_sent"], 1);
    assert_eq!(events[1]["error_code"], -32603);
    assert_eq!(
        events[1]["error_message"],
        "task stream interrupted; reconnect"
    );
    assert!(events.iter().all(|event| event["end_reason"] == "error"));
}

#[test]
fn stream_drop_during_setup_or_before_first_poll_still_logs_reason() {
    let logs = capture();
    let abort = create_abort_signal();
    let guard = RequestLog::new("jira", "SendStreamingMessage").streaming(abort.clone());
    drop(guard);
    let stream = RequestLog::new("jira", "SubscribeToTask")
        .streaming(abort.clone())
        .stream(Ok(stream::pending().boxed()))
        .unwrap();
    abort.set_ctrlc();
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["end_reason"], "client_disconnect");
    assert_eq!(events[1]["end_reason"], "server_shutdown");
    assert!(events.iter().all(|event| event["events_sent"] == 0));
}

#[tokio::test]
async fn stream_delivery_error_is_not_overwritten_by_previous_boundary() {
    let logs = capture();
    let inner = stream::iter([
        event(TaskState::InputRequired),
        Err(A2AError::internal("task stream interrupted; reconnect")),
    ])
    .boxed();
    let stream = RequestLog::new("jira", "SubscribeToTask")
        .streaming(create_abort_signal())
        .stream(Ok(inner))
        .unwrap();
    assert_eq!(stream.collect::<Vec<_>>().await.len(), 2);
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["end_reason"], "error");
    assert_eq!(events[0]["state"], "InputRequired");
    assert_eq!(events[0]["error_code"], -32603);
}

#[test]
fn completion_bounds_request_and_response_ids_without_changing_protocol_values() {
    let logs = capture();
    let id = "é".repeat(512);
    let mut log = RequestLog::new("jira", "GetTask")
        .with_task(&id)
        .with_context(Some(&id));
    log.error(&A2AError::task_not_cancelable(&id));
    drop(log);
    let mut response = task(TaskState::Completed);
    response.id.clone_from(&id);
    response.context_id.clone_from(&id);
    let result = RequestLog::new("jira", "GetTask")
        .complete(Ok(response), RequestLog::task)
        .unwrap();
    assert_eq!(result.id, id);
    let expected = format!("{}…", "é".repeat(256));
    let events = records(&logs);
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event["task_id"], expected);
        assert_eq!(event["context_id"], expected);
    }
    assert_eq!(events[0]["error_message"], "task cannot be canceled");
}

#[tokio::test]
async fn stream_eof_without_response_boundary_logs_stream_closed_once() {
    let logs = capture();
    let mut stream = RequestLog::new("jira", "SubscribeToTask")
        .streaming(create_abort_signal())
        .stream(Ok(stream::iter([event(TaskState::Working)]).boxed()))
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(records(&logs).is_empty());
    assert!(stream.next().await.is_none());
    drop(stream);
    let events = records(&logs);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["end_reason"], "stream_closed");
    assert_eq!(events[0]["events_sent"], 1);
    assert_eq!(events[0]["state"], "Working");
    assert!(events[0].get("error_code").is_none());
}

#[tokio::test]
async fn debug_caller_event_uses_resolved_identity_through_production_routes() {
    use axum::{body::Body, http::Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    harnx_core::require_nextest();
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .try_init()
        .unwrap();
    let export = crate::exports::Export {
        public_name: "jira".into(),
        agent: "team/triage".into(),
        cluster: None,
        lookup_keys: vec!["alias".into()],
        card_meta: crate::exports::AgentCardMeta {
            name: "jira".into(),
            description: String::new(),
            version: "1.0".into(),
            conversation_starters: Vec::new(),
        },
    };
    let app = crate::routes::router(&[export], None, &["X-User-ID".into()], |_, _| {
        Arc::new(crate::test_support::RoutingHandler)
    })
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/alias")
                .header("Content-Type", "application/json")
                .header("X-User-ID", "  private-forge-account  ")
                .header("Authorization", "Bearer private-token")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"task"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let events: Vec<Value> = logs
        .text()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["target"] == TARGET)
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["level"], "DEBUG");
    assert_eq!(events[0]["fields"]["message"], "A2A caller resolved");
    assert_eq!(events[0]["fields"]["export"], "jira");
    assert_eq!(events[0]["fields"]["user_id"], "private-forge-account");
    assert!(!logs.text().contains("private-token"));
}
