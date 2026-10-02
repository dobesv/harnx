use super::*;
use crate::nats_session::invocation_terminal_seq_tests::historical_cases;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use anyhow::{Context, Result};
use harnx_core::event::{NullSink, SubAgentProgressStatus};

#[tokio::test]
async fn historical_terminal_cancel_and_result_matrix() -> Result<()> {
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = SubagentToolset::new(
        "helper",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    );
    for (index, case) in historical_cases().into_iter().enumerate() {
        let session_id = format!("history-{index}");
        let session = toolset
            .create_session(Some(session_id.clone()), None, None)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        let log = NatsSessionLog::new_with_replicas(js.clone(), session.storage_key(), 1);
        for (expected_seq, entry) in &case.entries {
            let actual_seq = log.append_event_async(entry).await?;
            assert_eq!(actual_seq, *expected_seq, "{}: broker sequence", case.name);
        }
        assert_eq!(
            log.load_events_async().await?,
            case.entries,
            "{}: transcript roundtrip",
            case.name
        );
        for expected in case.turns {
            let context = format!("{} prompt={}", case.name, expected.prompt);
            let (response, error) =
                NatsSession::extract_turn_outcome(&case.entries, expected.prompt);
            assert_eq!(response, expected.response, "{context}: response");
            assert_eq!(error, expected.error, "{context}: error");
            let mut result = NatsTurnResult {
                response,
                error,
                session_id: session_id.clone(),
                user_msg_seq: expected.prompt,
                user_msg_id: format!("message-{}", expected.prompt),
                // Historical recovery has no live abort signal. Durable Cancel must decide.
                was_cancelled: false,
            };
            assert_eq!(
                toolset.turn_has_cancel(&result).await,
                expected.cancelled,
                "{context}: cancel consumer"
            );
            if matches!(
                &case.entries[expected.prompt as usize - 1].1,
                harnx_core::session::SessionLogEntry::Message { .. }
            ) {
                let replay = session
                    .clone()
                    .with_execution_parent("parent".into(), result.user_msg_id.clone());
                let recovered = replay
                    .completed_invocation_turn()
                    .await?
                    .context("completed historical turn")?;
                assert_eq!(
                    recovered.response, result.response,
                    "{context}: recovered response"
                );
                assert_eq!(recovered.error, result.error, "{context}: recovered error");
                assert_eq!(
                    recovered.user_msg_seq, result.user_msg_seq,
                    "{context}: recovered prompt"
                );
                result = recovered;
            } else {
                // Maintenance receipts aren't delegated user turns. Only test their raw consumers.
                continue;
            }
            let timeout = expected
                .error
                .as_deref()
                .and_then(crate::parse_timeout_terminal);
            let reporter = toolset
                .start_progress_reporter(super::super::ProgressReporterStart {
                    child_session_id: session_id.clone(),
                    parent_session_id: None,
                    invocation_id: format!("observation-{index}-{}", expected.prompt),
                    tool_call_id: None,
                })
                .await;
            let finished = finish_completed_turn(CompletedTurnParams {
                toolset: &toolset,
                child_session_id: session_id.clone(),
                reporter,
                buffering_sink: Arc::new(InvocationBufferingSink::new(Arc::new(NullSink))),
                result,
            })
            .await;
            if expected.cancelled && timeout.is_none() {
                let Err(ToolInvokeError::Recoverable(message)) = finished else {
                    panic!("{context}: expected ordinary cancellation failure");
                };
                assert_eq!(
                    message,
                    format!("sub-agent turn was cancelled (session_id: {session_id})"),
                    "{context}"
                );
            } else {
                let finished = finished.map_err(|err| anyhow::anyhow!("{context}: {err}"))?;
                let status = if expected.cancelled {
                    SubAgentProgressStatus::Cancelled
                } else if expected.error.is_some() || expected.response.is_none() {
                    SubAgentProgressStatus::Failed
                } else {
                    SubAgentProgressStatus::Done
                };
                assert_eq!(
                    finished.progress.status, status,
                    "{context}: real outcome classification"
                );
                assert_eq!(
                    finished
                        .termination
                        .as_ref()
                        .map(|stop| stop.termination.kind),
                    timeout.as_ref().map(|_| TerminationKind::Timeout),
                    "{context}: timeout classification"
                );
                if let Some(timeout) = timeout {
                    let envelope = result_value(&toolset, &finished)
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                    let advice = envelope["termination"]["retry_hint"]
                        .as_str()
                        .context("retry advice")?;
                    assert!(envelope["response"]
                        .as_str()
                        .context("response")?
                        .contains(advice));
                    assert_eq!(envelope["session_id"], session_id);
                    assert_eq!(envelope["termination"]["session_id"], session_id);
                    assert_eq!(envelope["sub_agent_progress"]["status"], "cancelled");
                    if timeout.scope == crate::TimeoutScope::LocalInvocation {
                        assert!(advice.contains("only while the outer run remains live"));
                        assert!(advice.contains("revise or narrow instructions"));
                    } else {
                        assert!(advice.contains("Do not retry:"));
                        assert!(advice.contains("Return to the user to confirm continuation"));
                    }
                    let stop = finished.termination.unwrap();
                    assert_eq!(
                        stop.termination_json()["scope"],
                        serde_json::to_value(timeout.scope)?,
                        "{context}: original timeout scope"
                    );
                    assert_eq!(
                        stop.termination_json()["run_id"],
                        timeout.run_id,
                        "{context}: original timeout run"
                    );
                    assert_eq!(
                        stop.termination_json()["invocation_id"],
                        timeout.invocation_id,
                        "{context}: original timeout invocation"
                    );
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn scoped_failure_envelopes_preserve_available_durable_output_and_usage() -> Result<()> {
    use harnx_core::{
        event::{AgentEvent, ModelEvent},
        message::MessageRole,
        session::SessionLogEntry,
    };
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let toolset = SubagentToolset::new(
        "helper",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(
            js.client().clone(),
            js.clone(),
            SessionMetadataStore::ensure(&js, 1).await?,
            1,
        ),
    );
    let reference = "cid:plan:pantheon%2Fatlas/armDRA/issue-2222";
    let output = format!("Saved partial results in {reference}");
    for (index, scope) in [
        Some(crate::TimeoutScope::LocalInvocation),
        Some(crate::TimeoutScope::InheritedDeadline),
        Some(crate::TimeoutScope::OuterRun),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let session_id = format!("guidance-{index}");
        let log = NatsSessionLog::new_with_replicas(
            js.clone(),
            harnx_core::session_identity::session_key(Some("helper"), &session_id),
            1,
        );
        for (role, text) in [
            (MessageRole::User, "work"),
            (MessageRole::Assistant, output.as_str()),
        ] {
            log.append_event_async(&SessionLogEntry::Message {
                id: None,
                role,
                content: MessageContent::Text(text.into()),
                timestamp: None,
                fence_token: Some(1),
            })
            .await?;
        }
        let error = if let Some(scope) = scope {
            crate::TimeoutTerminal {
                scope,
                deadline: chrono::Utc::now(),
                run_id: "run".into(),
                invocation_id: "invocation".into(),
            }
            .message()
        } else {
            harnx_core::loop_guard::RepetitionStop(
                harnx_core::loop_guard::RepetitionTerminal::tool_calls("fs_read", 4),
            )
            .to_string()
        };
        let terminal = if scope.is_some() {
            SessionLogEntry::Cancel {
                fence_token: 1,
                cancellation_id: Some("deadline".into()),
                requested_by: Some(error.clone()),
                timestamp: None,
            }
        } else {
            SessionLogEntry::Error {
                message: error.clone(),
                fence_token: 1,
                timestamp: None,
            }
        };
        log.append_event_async(&terminal).await?;
        log.append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("later turn must not leak".into()),
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;
        let reporter = toolset
            .start_progress_reporter(super::super::ProgressReporterStart {
                child_session_id: session_id.clone(),
                parent_session_id: None,
                invocation_id: format!("observe-{index}"),
                tool_call_id: None,
            })
            .await;
        reporter.sink().emit(AgentEvent::Model(ModelEvent::Usage {
            input: 11,
            output: 7,
            cached: 3,
            cache_write: 2,
            session_label: None,
        }));
        let finished = finish_completed_turn(CompletedTurnParams {
            toolset: &toolset,
            child_session_id: session_id.clone(),
            reporter,
            buffering_sink: Arc::new(InvocationBufferingSink::new(Arc::new(NullSink))),
            result: NatsTurnResult {
                response: None,
                error: Some(error),
                session_id: session_id.clone(),
                user_msg_seq: 1,
                user_msg_id: "prompt".into(),
                was_cancelled: scope.is_some(),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let envelope =
            result_value(&toolset, &finished).map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(envelope["session_id"], session_id);
        assert_eq!(envelope["termination"]["session_id"], session_id);
        assert_eq!(envelope["termination"]["usage"]["input_uncached"], 6);
        assert_eq!(envelope["termination"]["usage"]["output"], 7);
        assert_eq!(envelope["termination"]["usage"]["cache_write"], 2);
        assert_eq!(envelope["termination"]["usage"]["budgeted"], 15);
        assert_eq!(envelope["sub_agent_progress"]["usage"]["output_tokens"], 7);
        assert_eq!(
            envelope["termination"]["thinking_excerpt"],
            serde_json::Value::Null
        );
        assert_eq!(
            envelope["termination"]["public_progress"]["available"],
            true
        );
        assert_eq!(
            envelope["termination"]["public_progress"]["references"],
            serde_json::json!([reference])
        );
        let response = envelope["response"].as_str().context("response")?;
        let advice = envelope["termination"]["retry_hint"]
            .as_str()
            .context("advice")?;
        assert!(response.contains(&output));
        assert!(response.contains(advice));
        assert!(!response.contains("later turn must not leak"));
        match scope {
            Some(crate::TimeoutScope::InheritedDeadline | crate::TimeoutScope::OuterRun) => {
                assert!(advice.contains("Do not retry:"));
                assert!(advice.contains("Return to the user to confirm continuation"));
            }
            Some(_) => assert!(advice.contains("only while the outer run remains live")),
            None => {
                assert_eq!(envelope["termination"]["kind"], "repetition");
                assert_eq!(envelope["termination"]["source"], "tool_calls");
                assert_eq!(envelope["termination"]["tool"], "fs_read");
                assert_eq!(envelope["termination"]["count"], 4);
                assert!(advice.contains("change the repeated approach"));
                assert!(advice.contains("Do not retry unchanged"));
                assert!(advice.contains(&session_id));
            }
        }
    }
    Ok(())
}
