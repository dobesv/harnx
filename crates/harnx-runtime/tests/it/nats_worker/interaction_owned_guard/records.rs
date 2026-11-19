use super::*;
use harnx_core::session::ToolOutput;
use harnx_runtime::interaction_checkpoint::{round_start_key, RoundStart, ROUND_START_BUCKET};
use harnx_toolset_server::invocation_journal::InvocationJournal;

pub(super) fn calls() -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: "unfinished round".into(),
        thought: None,
        calls: [
            ("guard_read", "saved"),
            ("guard_read", "rerun"),
            ("unknown_tool", "unknown"),
        ]
        .into_iter()
        .map(|(tool, id)| ToolCall::new(tool.into(), json!({}), Some(id.into()), None))
        .collect(),
        timestamp: None,
        fence_token: Some(1),
    }
}

fn output(id: &str) -> ToolOutput {
    ToolOutput {
        id: Some(id.into()),
        name: "guard_read".into(),
        output: json!({"ok":true}),
        markdown: None,
        content: vec![],
        switch_agent: None,
    }
}

pub(super) async fn seed_case(
    case: Case,
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    round: u64,
) -> Result<()> {
    match case {
        Case::InteractionRecords => {
            log.append_event_async(&SessionLogEntry::ToolCallCompleted {
                tool_round_seq: round,
                tool_call_id: "saved".into(),
                result: output("saved"),
                private_snapshot: "private-result".into(),
                fence_token: 1,
            })
            .await?;
            log.append_event_async(&SessionLogEntry::InteractionRequested {
                tool_round_seq: round,
                tool_call_id: "rerun".into(),
                interaction_id: "confirm".into(),
                producer: "middleware:guard".into(),
                step_path: vec![],
                key: "confirm".into(),
                kind: "confirm".into(),
                payload: harnx_core::interaction::confirm_surface("s", "Continue?"),
                summary: "Continue?".into(),
                chain_version: "chain".into(),
                checkpoint: "private-state".into(),
                fence_token: 1,
            })
            .await?;
        }
        Case::SuspensionReady => {
            log.append_event_async(&SessionLogEntry::SuspensionReady {
                tool_round_seq: round,
                pending: vec!["confirm".into()],
                fence_token: 1,
            })
            .await?;
        }
        Case::RoundStart | Case::StoreError | Case::MissingKey => {
            let store = js
                .create_key_value(async_nats::jetstream::kv::Config {
                    bucket: ROUND_START_BUCKET.into(),
                    ..Default::default()
                })
                .await?;
            if !matches!(case, Case::MissingKey) {
                let bytes = if matches!(case, Case::RoundStart) {
                    serde_json::to_vec(&RoundStart {
                        version: 1,
                        session_storage_key: log.storage_key().into(),
                        tool_round_seq: round,
                        fence_token: 1,
                    })?
                } else {
                    b"corrupt checkpoint".to_vec()
                };
                store
                    .put(round_start_key(log.storage_key(), round), bytes.into())
                    .await?;
            }
        }
        Case::Handoff | Case::HandoffCommitted | Case::UnmarkedHandoff | Case::ErrorHandoff => {
            let mut result = output("saved");
            result.switch_agent = Some(serde_json::from_value(json!({
                "agent":"never-dispatch", "session_id":"never-target", "prompt":"continue"
            }))?);
            if matches!(case, Case::ErrorHandoff) {
                result.output = json!({"isError":true});
            }
            log.append_event_async(&SessionLogEntry::ToolResults {
                results: vec![result, output("rerun"), output("unknown")],
                timestamp: None,
                handoff_selection: (!matches!(case, Case::UnmarkedHandoff)).then(|| "saved".into()),
            })
            .await?;
            if matches!(case, Case::HandoffCommitted) {
                log.append_event_async(&SessionLogEntry::HandoffCommitted {
                    target_agent: "never-dispatch".into(),
                    target_session_id: "never-target".into(),
                    handoff_tool_call_id: Some("saved".into()),
                })
                .await?;
            }
        }
        Case::InvalidStore => {
            // Stream exists, but isn't a KV bucket. get_key_value must return
            // InvalidStoreName rather than authorizing legacy recovery.
            js.create_stream(async_nats::jetstream::stream::Config {
                name: format!("KV_{ROUND_START_BUCKET}"),
                subjects: vec![format!("$KV.{ROUND_START_BUCKET}.>")],
                max_messages_per_subject: 0,
                ..Default::default()
            })
            .await?;
            assert!(harnx_runtime::interaction_checkpoint::round_started(
                js,
                log.storage_key(),
                round
            )
            .await
            .is_err());
        }
        Case::MissingBucket => {}
    }
    Ok(())
}

pub(super) async fn saved_journal(
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    round: u64,
) -> Result<()> {
    let journal = InvocationJournal::ensure(js, 1).await?;
    let request =
        serde_json::from_value(json!({"operation_id":"guard-read", "call_id":"wire-saved",
        "tool":"read", "args":{}, "parent_session_id":log.storage_key(),
        "tool_call_id":"saved", "tool_round":round}))?;
    journal
        .record(&request, ("guard_read", "guard-scope", "guard"))
        .await?;
    journal
        .complete(
            &request,
            harnx_toolset::ToolReply {
                call_id: "wire-saved".into(),
                result: Ok(json!({"raw":"pre-middleware reply"})),
                final_progress: None,
            },
        )
        .await?;
    Ok(())
}
