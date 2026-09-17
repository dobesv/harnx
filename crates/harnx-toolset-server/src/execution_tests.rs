use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn tool_request(session: &str, call_id: &str) -> ToolRequest {
    ToolRequest {
        replay: None,
        operation_id: call_id.into(),
        call_id: call_id.into(),
        tool: "echo".into(),
        args: Value::Null,
        parent_session_id: Some(session.into()),
        tool_call_id: None,
        capabilities: Default::default(),
    }
}

#[tokio::test]
async fn cancellation_before_first_poll_never_starts_cooperative_work_for_cleanup() {
    let execution =
        InvocationExecution::claim(&tool_request("pre-start", "tool")).expect("claim invocation");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = Arc::new(AtomicBool::new(false));
    let work = {
        let started = started.clone();
        async move {
            started.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        }
    };
    let (reply, result) = oneshot::channel();
    execution
        .invoke(cancel, CancellationGuarantee::Cooperative, work, reply)
        .await;
    assert!(matches!(
        result.await,
        Ok(Err(ToolInvokeError::Interrupted(_)))
    ));
    assert!(!started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_cancellation_that_stopped_a_call_names_its_interrupted_reply() {
    let execution =
        InvocationExecution::claim(&tool_request("named", "tool")).expect("claim invocation");
    let cancel = CancellationToken::new();
    let active = execution.active_call(cancel.clone());
    assert_eq!(active.session_id, "named");
    active.set_cancellation_id("first-cancel");
    // A retry of the same stop must not relabel an interruption already decided.
    active.set_cancellation_id("second-cancel");
    active.cancel.cancel();
    let (reply, result) = oneshot::channel();
    execution
        .invoke(
            cancel,
            CancellationGuarantee::Cooperative,
            std::future::pending(),
            reply,
        )
        .await;
    let Ok(Err(ToolInvokeError::Interrupted(interrupted))) = result.await else {
        panic!("a cancelled call replies as interrupted");
    };
    assert_eq!(interrupted.cancellation_id.as_deref(), Some("first-cancel"));
    assert_eq!(interrupted.reason, "tool invocation cancelled");
}

/// The window a biased select cannot close: control accepts the cancellation
/// after `run` has read the token for this poll and before the handler is
/// polled in that same poll, so the handler is free to produce a result the
/// sender of the cancel has already been told will not happen.
#[tokio::test]
async fn a_cancellation_accepted_mid_poll_outranks_the_handler_that_finished_in_it() {
    let execution =
        InvocationExecution::claim(&tool_request("mid-poll", "tool")).expect("claim invocation");
    let cancel = CancellationToken::new();
    let active = execution.active_call(cancel.clone());
    let work = async move {
        active.set_cancellation_id("mid-poll-cancel");
        active.cancel.cancel();
        Ok(Value::Null)
    };
    let (reply, result) = oneshot::channel();
    execution
        .invoke(cancel, CancellationGuarantee::Cooperative, work, reply)
        .await;
    let Ok(Err(ToolInvokeError::Interrupted(interrupted))) = result.await else {
        panic!("an accepted cancellation decides the call, however late the handler is");
    };
    assert_eq!(
        interrupted.cancellation_id.as_deref(),
        Some("mid-poll-cancel")
    );
}
