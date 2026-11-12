use super::{fixture::*, *};
use crate::nats_session_metadata::{
    AdmissionAuthority, InvocationAdmission, InvocationEdgeKind, SessionMetadataStore,
};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};

#[tokio::test]
async fn first_terminal_survives_steering_compaction_later_approval_deadline_and_restart(
) -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    let steering = f
        .append(message("steering", MessageRole::User, "later steering"))
        .await?;
    f.toolset
        .session_metadata
        .bind_prompt_admission(f.log.storage_key(), "steering", "original-prompt")
        .await?;
    f.append(message("answer", MessageRole::Assistant, "original answer"))
        .await?;
    f.append(end(prompt.prompt.user_msg_seq())).await?;
    let first = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(first["status"], "completed");
    assert_eq!(first["terminal"], true);
    f.append(SessionLogEntry::Compress {
        prompt: "summary".into(),
    })
    .await?;
    f.append(message("original-prompt", MessageRole::User, "original"))
        .await?;
    f.append(end(steering)).await?;
    f.append(message("new-turn", MessageRole::User, "unrelated"))
        .await?;
    block(&f).await?;
    f.append(SessionLogEntry::Error {
        message: "later failure".into(),
        fence_token: 0,
        timestamp: None,
    })
    .await?;
    let admission = f
        .toolset
        .session_metadata
        .prompt_admission(f.log.storage_key(), "original-prompt")
        .await?
        .unwrap();
    let mut expired = admission.parent.unwrap();
    expired.invocation_id = admission.invocation_id;
    expired.deadline = Some(chrono::Utc::now() - chrono::Duration::hours(1));
    f.toolset
        .session_metadata
        .put_invocation_limits(f.log.storage_key(), &expired)
        .await?;
    let restarted = make_toolset(&f.js, SessionMetadataStore::ensure(&f.js, 1).await?);
    let context = ToolInvocationContext {
        call_id: "later-parent-turn".into(),
        ..f.wait_context()
    };
    let later = wait(
        &restarted,
        f.wait_args(json!({"seconds": 120})),
        CancellationToken::new(),
        context,
    )
    .await?;
    assert_eq!(later["status"], "completed");
    assert_eq!(later["output"]["events"], first["output"]["events"]);
    assert!(!later["output"].to_string().contains("later failure"));
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn success_requires_covering_durable_terminal_not_text_or_noncovering_turn_end() -> Result<()>
{
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    f.append(message(
        "answer",
        MessageRole::Assistant,
        "looks final but no terminal yet",
    ))
    .await?;
    f.append(end(0)).await?;
    let pending = f.status().await?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["terminal"], false);
    f.append(end(prompt.prompt.user_msg_seq())).await?;
    let completed = f.status().await?;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["terminal"], true);
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn durable_error_cancel_and_matching_timeout_are_distinct_and_first_wins() -> Result<()> {
    for expected in ["failed", "cancelled", "timed_out"] {
        let f = Fixture::new().await?;
        f.dispatch().await?;
        let a = f
            .toolset
            .session_metadata
            .prompt_admission(f.log.storage_key(), "original-prompt")
            .await?
            .unwrap();
        let entry = match expected {
            "failed" => SessionLogEntry::Error {
                message: "worker failed".into(),
                fence_token: 0,
                timestamp: None,
            },
            "cancelled" => {
                SessionLogEntry::cancel_request("stop".into(), "explicit user stop".into())
            }
            _ => SessionLogEntry::cancel_request(
                "timeout".into(),
                crate::TimeoutTerminal {
                    scope: crate::TimeoutScope::LocalInvocation,
                    deadline: chrono::Utc::now(),
                    run_id: a.run_id.as_str().into(),
                    invocation_id: a.invocation_id.as_str().into(),
                }
                .message(),
            ),
        };
        f.append(entry).await?;
        f.append(end(1)).await?;
        let result = f
            .poll(json!({"seconds": 1, "include_partial": false}))
            .await?;
        assert_eq!(result["status"], expected);
        assert_eq!(result["terminal"], true);
        assert!(!result["output"]["events"].as_array().unwrap().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn approval_is_blocked_nonterminal_and_partial_off_never_hides_terminal_output() -> Result<()>
{
    let f = Fixture::new().await?;
    f.dispatch().await?;
    block(&f).await?;
    let blocked = f.poll(json!({"include_partial": false})).await?;
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["terminal"], false);
    assert!(blocked["output"].get("events").is_none());
    assert_eq!(f.activation_count().await?, 1);
    f.append(end(1)).await?;
    let completed = f.poll(json!({"include_partial": false})).await?;
    assert_eq!(completed["status"], "completed");
    assert!(completed["output"].get("events").is_some());
    Ok(())
}

#[tokio::test]
async fn prompt_identity_is_not_execution_identity_and_filters_never_classify_lifecycle(
) -> Result<()> {
    let f = Fixture::new().await?;
    let parent = serde_json::from_value(f.request.run_context.as_ref().unwrap().snapshot.clone())?;
    let authority = AdmissionAuthority::Inherited {
        parent,
        edge: InvocationEdgeKind::Delegation,
        admitted_at: chrono::Utc::now(),
    };
    let mut admission =
        InvocationAdmission::new(&authority, "different-execution".into(), None, None);
    admission.parent_storage_key = Some("trusted-parent".into());
    admission.prompt_content = Some(MessageContent::Text("original".into()));
    f.toolset
        .session_metadata
        .reserve_admission(f.log.storage_key(), &admission, &[])
        .await?;
    f.toolset
        .session_metadata
        .bind_prompt_admission(
            f.log.storage_key(),
            "original-prompt",
            "different-execution",
        )
        .await?;
    let seq = f
        .append(message("original-prompt", MessageRole::User, "original"))
        .await?;
    f.append(message("answer", MessageRole::Assistant, "old reply"))
        .await?;
    f.append(end(seq)).await?;
    let result = f
        .poll(
            json!({"seconds": 1, "read": {"types": ["message"], "text_regex": "nothing matches"}}),
        )
        .await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["invocation_id"], "different-execution");
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_eq!(result["output"]["events"], json!([]));
    Ok(())
}

#[tokio::test]
async fn retained_or_missing_original_and_gaps_never_substitute_a_compaction_copy_or_later_result(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(message("answer", MessageRole::Assistant, "old answer"))
        .await?;
    f.append(end(1)).await?;
    f.append(SessionLogEntry::Compress {
        prompt: "summary".into(),
    })
    .await?;
    f.append(message("original-prompt", MessageRole::User, "original"))
        .await?;
    f.append(end(5)).await?;
    let stream =
        f.js.get_stream(crate::nats_session_log::stream_name_for_session(
            f.log.storage_key(),
        ))
        .await?;
    stream.delete_message(1).await?;
    let unknown = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(unknown["status"], "unknown");
    assert_eq!(unknown["terminal"], false);
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn compaction_after_settled_round_keeps_prompt_pending() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    settled_round(&f, "first-call").await?;
    compaction_layout(&f).await?;
    let pending = f.status().await?;
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["terminal"], false);
    f.append(end(prompt.prompt.user_msg_seq())).await?;
    let done = f.status().await?;
    assert_eq!(done["status"], "completed");
    assert_eq!(done["terminal"], true);
    Ok(())
}

#[tokio::test]
async fn compaction_with_parked_approval_stays_unknown() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    block(&f).await?;
    f.append(SessionLogEntry::Compress {
        prompt: "summary".into(),
    })
    .await?;
    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["status"], "unknown");
    assert_eq!(
        result["output"]["observation"]["error"],
        "live_state_unprovable"
    );
    Ok(())
}
