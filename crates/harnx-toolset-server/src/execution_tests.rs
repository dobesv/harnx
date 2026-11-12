use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn tool_request(session: &str, call_id: &str) -> ToolRequest {
    ToolRequest {
        run_context: None,
        replay: None,
        operation_id: call_id.into(),
        call_id: call_id.into(),
        tool: "echo".into(),
        args: Value::Null,
        parent_session_id: Some(session.into()),
        parent_agent: None,
        parent_local_session_id: None,
        tool_call_id: None,
        capabilities: Default::default(),
        tool_round: None,
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

fn new_message_control(session: &str) -> harnx_toolset::ControlMessage {
    harnx_toolset::ControlMessage::new_message("srv".into(), session.into(), "tool".into())
}

/// A new-message signal ends only the call's wait: the handler answers with
/// its own result, and nothing records a cancellation that would outrank it.
#[tokio::test]
async fn a_new_message_signal_keeps_the_handlers_own_reply() {
    let execution =
        InvocationExecution::claim(&tool_request("new-message", "tool")).expect("claim invocation");
    let cancel = CancellationToken::new();
    let active = execution.active_call(cancel.clone());
    let acceptance = crate::control::signal_call(&active, &new_message_control("new-message"));
    assert_eq!(acceptance, harnx_toolset::CancelAcceptance::Accepted);
    assert!(
        !cancel.is_cancelled(),
        "a new message must not cancel the call"
    );
    let new_message = active.new_message.clone();
    let work = async move {
        new_message.cancelled().await;
        Ok(Value::from("still running"))
    };
    let (reply, result) = oneshot::channel();
    execution
        .invoke(cancel, CancellationGuarantee::Cooperative, work, reply)
        .await;
    assert_eq!(result.await.unwrap(), Ok(Value::from("still running")));
}

#[tokio::test]
async fn a_cancel_after_a_new_message_signal_still_interrupts() {
    let session = "new-message-then-cancel";
    let execution =
        InvocationExecution::claim(&tool_request(session, "tool")).expect("claim invocation");
    let cancel = CancellationToken::new();
    let active = execution.active_call(cancel.clone());
    crate::control::signal_call(&active, &new_message_control(session));
    let later_cancel = harnx_toolset::ControlMessage::cancel(
        "srv".into(),
        session.into(),
        "tool".into(),
        "later-cancel".into(),
    );
    crate::control::signal_call(&active, &later_cancel);
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
        panic!("a cancel still interrupts a call that saw a new message");
    };
    assert_eq!(interrupted.cancellation_id.as_deref(), Some("later-cancel"));
}

#[test]
fn a_new_message_pending_at_dispatch_starts_the_signal_fired() {
    let mut request = tool_request("pending-new-message", "tool");
    let quiet = InvocationExecution::claim(&request).expect("claim invocation");
    request
        .capabilities
        .insert(harnx_toolset::CAPABILITY_NEW_MESSAGE_PENDING.into());
    let pending = InvocationExecution::claim(&request).expect("claim invocation");
    let signalled = |execution: &InvocationExecution| {
        execution
            .active_call(CancellationToken::new())
            .new_message
            .is_cancelled()
    };
    assert_eq!((signalled(&quiet), signalled(&pending)), (false, true));
}
