//! Joined prompts remain observable across callers in the agent namespace.
//! Wait and prompt-scoped load use the joined content, even when another
//! parent or an External (TUI/web) caller started the turn (issue #2672).
use super::{fixture::*, *};
use crate::nats_session_metadata::{AdmissionAuthority, InvocationAdmission};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};
use harnx_toolset::ToolInvocationContext;
use std::sync::Arc;

mod round_tests;

/// A journaled second-parent send against the fixture's busy child.
async fn joining_send(
    f: &Fixture,
    call: &str,
    message: &str,
) -> Result<dispatch::DispatchedPrompt> {
    let mut request = f.request.clone();
    request.call_id = call.into();
    request.operation_id = format!("{call}-operation");
    request.parent_session_id = Some("parent-b".into());
    request.run_context = Some(crate::nats_session::test_support::inherited_tool_context());
    request.args = json!({"message": message, "session_id": "child"});
    f.journal
        .record(&request, ("session_send", "helper", "server"))
        .await?;
    let context = ToolInvocationContext {
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        run_context: request.run_context.clone(),
        checkpoint_store: Some(Arc::new(f.journal.checkpoint_store(&request))),
        partial_result_store: Some(Arc::new(f.journal.partial_result_store(&request))),
        ..Default::default()
    };
    Ok(
        dispatch::dispatch_send(&f.toolset, request.args, CancellationToken::new(), context)
            .await?
            .0,
    )
}

fn parent_b_context() -> ToolInvocationContext {
    ToolInvocationContext {
        call_id: "wait-parent-b".into(),
        invoking_session_id: Some("parent-b".into()),
        ..Default::default()
    }
}

async fn wait_as(
    f: &Fixture,
    prompt_id: &str,
    seconds: u64,
    context: &ToolInvocationContext,
) -> Result<Value> {
    wait(
        &f.toolset,
        json!({"session_id": "child", "prompt_id": prompt_id, "seconds": seconds}),
        CancellationToken::new(),
        context.clone(),
    )
    .await
    .map_err(anyhow::Error::new)
}

async fn load_status(f: &Fixture, prompt_id: &str) -> Result<Value> {
    f.toolset
        .session_load(json!({"session_id": "child", "prompt_id": prompt_id}))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))
}

#[tokio::test]
async fn joined_prompt_observes_for_sender_starter_and_unrelated_caller() -> Result<()> {
    let f = Fixture::new().await?;
    let first = f.dispatch().await?;
    // A second parent joins the busy turn with different prompt text.
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    assert_eq!(
        joined.prompt.execution_id(),
        Some(first.handle.prompt_id.as_str())
    );
    // Pending while the turn runs, through wait and prompt-scoped load.
    for context in [parent_b_context(), f.wait_context(), unrelated_context()] {
        let pending = wait_as(&f, "join-call", 1, &context).await?;
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["terminal"], false);
        let output = serde_json::to_string(&pending["output"])?;
        assert!(output.contains("joined steering content"), "{output}");
        assert!(!output.contains("original\""), "{output}");
    }
    assert_eq!(load_status(&f, "join-call").await?["status"], "pending");
    // The turn ends covering the joined message.
    f.append(message("reply", MessageRole::Assistant, "joined answer"))
        .await?;
    f.append(end(joined.prompt.user_msg_seq())).await?;
    let completed = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
    assert_eq!(completed["status"], "completed");
    let output = serde_json::to_string(&completed["output"])?;
    assert!(
        output.contains("joined answer"),
        "output starts at the joined message: {output}"
    );
    assert!(
        !output.contains("original\""),
        "the invocation's own prompt stays outside the joined scope: {output}"
    );
    // Ownership doesn't restrict observation. Every caller sees the joined
    // message and answer, not the invocation starter's original prompt.
    for context in [f.wait_context(), unrelated_context()] {
        let observed = wait_as(&f, "join-call", 1, &context).await?;
        assert_eq!(observed["status"], "completed");
        assert_eq!(observed["terminal"], true);
        let output = serde_json::to_string(&observed["output"])?;
        assert!(output.contains("joined steering content"), "{output}");
        assert!(output.contains("joined answer"), "{output}");
        assert!(!output.contains("original\""), "{output}");
    }
    let loaded = load_status(&f, "join-call").await?;
    assert_eq!(loaded["status"], "completed");
    assert_eq!(loaded["terminal"], true);
    let output = serde_json::to_string(&loaded)?;
    assert!(output.contains("joined steering content"), "{output}");
    assert!(output.contains("joined answer"), "{output}");
    assert!(!output.contains("original\""), "{output}");
    // The sender's own original handle still works for the sender.
    let own = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(own["status"], "completed");
    Ok(())
}

#[tokio::test]
async fn joined_prompt_on_external_admission_observes_for_the_joining_parent() -> Result<()> {
    let f = Fixture::new().await?;
    // Create the child session through the toolset so it has canonical
    // metadata, then seed a busy turn the way the TUI/web frontend would:
    // an External-admitted invocation with its prompt appended and bound.
    f.toolset
        .create_session(Some("child".into()), Some("external-parent"), None)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let storage = f.log.storage_key();
    let authority = AdmissionAuthority::External {
        admitted_at: chrono::Utc::now(),
    };
    let mut intent = InvocationAdmission::new(&authority, "external-invocation".into(), None, None);
    intent.prompt_content = Some(MessageContent::Text("external original".into()));
    let entries = f.log.load_events_latest_async().await?;
    let admission = f
        .toolset
        .session_metadata
        .reserve_admission(storage, &intent, &entries)
        .await?;
    assert_eq!(admission.invocation_id.as_str(), "external-invocation");
    f.append(message(
        "external-prompt",
        MessageRole::User,
        "external original",
    ))
    .await?;
    f.toolset
        .session_metadata
        .bind_prompt_admission(storage, "external-prompt", "external-invocation")
        .await?;
    // An inherited parent joins the External turn.
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    assert_eq!(joined.prompt.execution_id(), Some("external-invocation"));
    let pending = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(load_status(&f, "join-call").await?["status"], "pending");
    f.append(message(
        "reply",
        MessageRole::Assistant,
        "external turn answer",
    ))
    .await?;
    f.append(end(joined.prompt.user_msg_seq())).await?;
    let completed = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
    assert_eq!(completed["status"], "completed");
    let output = serde_json::to_string(&completed["output"])?;
    assert!(
        output.contains("external turn answer"),
        "joined scope output: {output}"
    );
    let external = wait_as(&f, "external-prompt", 1, &unrelated_context()).await?;
    assert_eq!(external["status"], "completed");
    assert_eq!(external["terminal"], true);
    let output = serde_json::to_string(&external["output"])?;
    assert!(output.contains("external original"), "{output}");
    assert!(output.contains("external turn answer"), "{output}");
    Ok(())
}

/// The joined message keeps exactly the binding the worker's mid-turn
/// injection check requires, so a joined message is injectable like TUI
/// steering (issue #2672 invariants).
#[tokio::test]
async fn joined_message_binding_matches_its_invocation_for_injection() -> Result<()> {
    let f = Fixture::new().await?;
    let first = f.dispatch().await?;
    let _joined = joining_send(&f, "join-call", "joined steering content").await?;
    let storage = f.log.storage_key();
    let binding = f
        .toolset
        .session_metadata
        .prompt_admission(storage, "join-call")
        .await?
        .context("joined prompt has no admission binding")?;
    assert_eq!(
        binding.invocation_id.as_str(),
        first.handle.prompt_id,
        "verify_injected_prompt_admissions accepts exactly this binding"
    );
    let entries = f.log.load_events_latest_async().await?;
    let joined_row = entries
        .iter()
        .find(|(_, entry)| matches!(entry, SessionLogEntry::Message { id: Some(id), .. } if id == "join-call"))
        .context("joined message not appended")?;
    assert!(
        matches!(&joined_row.1, SessionLogEntry::Message { id: Some(id), role, .. } if id == "join-call" && role.is_user())
    );
    Ok(())
}

fn unrelated_context() -> ToolInvocationContext {
    ToolInvocationContext {
        call_id: "wait-foreign".into(),
        invoking_session_id: Some("foreign-parent".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn unrelated_observer_rejects_invocation_content_substituted_for_joined_content() -> Result<()>
{
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    let activations = f.activation_count().await?;
    let stream =
        f.js.get_stream(crate::nats_session_log::stream_name_for_session(
            f.log.storage_key(),
        ))
        .await?;
    stream.delete_message(joined.prompt.user_msg_seq()).await?;
    // This matches the invocation admission, but not the joined record.
    let forged = f
        .append(message("join-call", MessageRole::User, "original"))
        .await?;
    f.append(end(forged)).await?;
    let observed = wait_as(&f, "join-call", 1, &unrelated_context()).await?;
    assert_eq!(observed["status"], "unknown");
    assert_eq!(observed["terminal"], false);
    assert_eq!(
        observed["output"]["observation"]["error"],
        "prompt_content_mismatch"
    );
    assert!(observed["output"].get("events").is_none());
    assert!(load_status(&f, "join-call").await.is_err());
    assert_eq!(f.activation_count().await?, activations);
    Ok(())
}

/// A joined handle reports Error and Cancel terminals like any other prompt:
/// the shared terminal classification runs on the bound invocation (F9).
#[tokio::test]
async fn joined_handle_reports_error_and_cancel_terminals() -> Result<()> {
    for (call, terminal, expected) in [
        (
            "join-err",
            SessionLogEntry::Error {
                message: "child failed".into(),
                fence_token: 0,
                timestamp: None,
            },
            "failed",
        ),
        (
            "join-cancel",
            SessionLogEntry::Cancel {
                fence_token: 0,
                cancellation_id: None,
                requested_by: None,
                timestamp: None,
            },
            "cancelled",
        ),
    ] {
        let f = Fixture::new().await?;
        let _first = f.dispatch().await?;
        joining_send(&f, call, "joined steering content").await?;
        assert_eq!(
            wait_as(&f, call, 1, &parent_b_context()).await?["status"],
            "pending"
        );
        f.append(terminal).await?;
        let observed = wait_as(&f, call, 1, &parent_b_context()).await?;
        assert_eq!(observed["terminal"], true, "{call}");
        assert_eq!(observed["status"], expected, "{call}");
        let loaded = load_status(&f, call).await?;
        assert_eq!(loaded["terminal"], true, "{call}");
        assert_eq!(loaded["status"], expected, "{call}");
        // The terminal handle stays terminal: repeat waits neither repair nor
        // republish an activation.
        let activations = f.activation_count().await?;
        assert_eq!(
            wait_as(&f, call, 1, &parent_b_context()).await?["status"],
            expected,
            "{call}"
        );
        assert_eq!(f.activation_count().await?, activations, "{call}");
    }
    Ok(())
}
