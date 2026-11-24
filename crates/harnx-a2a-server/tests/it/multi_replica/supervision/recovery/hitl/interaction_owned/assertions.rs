use super::*;

fn same_completed_invocation(current: &ContextSnapshot, before: &ContextSnapshot) -> Result<()> {
    let active = current
        .document
        .state
        .active
        .as_ref()
        .context("completed task missing")?;
    let original = before
        .document
        .state
        .active
        .as_ref()
        .context("original task missing")?;
    assert_eq!(active.snapshot.task.status.state, TaskState::Completed);
    assert_eq!(active.snapshot.task.id, original.snapshot.task.id);
    assert_eq!(
        active.snapshot.task.context_id,
        original.snapshot.task.context_id
    );
    assert_eq!(
        active.admission.invocation_id,
        original.admission.invocation_id
    );
    assert_eq!(active.admission.prompt_id, original.admission.prompt_id);
    assert_eq!(active.admission.closure_id, original.admission.closure_id);
    assert_eq!(
        active.admission.fixed_predecessor,
        original.admission.fixed_predecessor
    );
    assert_eq!(
        active.admission.prompt_sequence,
        original.admission.prompt_sequence
    );
    assert!(active.cancel.is_none());
    Ok(())
}

fn assert_tool_effect(entries: &[(u64, SessionLogEntry)], dispatches: u64) -> Result<()> {
    let batches: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .collect();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), 1);
    let output = &batches[0][0];
    assert_eq!(output.id.as_deref(), Some("call-1"));
    assert_eq!(output.name, "time_get_current_time");
    assert!(
        output.output.get("error").is_none(),
        "tool failed: {:?}",
        output.output
    );
    assert_ne!(output.output["is_error"], true);
    assert_eq!(
        dispatches, 1,
        "one physical NATS tool dispatch is required; output: {:?}",
        output.output
    );
    Ok(())
}

async fn verify_target(
    t: &TwoBackends,
    expected_storage: Option<&str>,
    entries: &[(u64, SessionLogEntry)],
) -> Result<()> {
    let commits: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::HandoffCommitted {
                target_agent,
                target_session_id,
                handoff_tool_call_id,
            } if handoff_tool_call_id.as_deref() == Some("call-1") => {
                Some((target_agent, target_session_id))
            }
            _ => None,
        })
        .collect();
    assert_eq!(commits.len(), 1, "one source commit is required");
    let (agent, session) = commits[0];
    let local = match harnx_core::agent_ref::AgentRef::parse(agent) {
        harnx_core::agent_ref::AgentRef::Local(agent)
        | harnx_core::agent_ref::AgentRef::Remote { agent, .. } => agent,
    };
    let storage = harnx_core::session_identity::session_key(Some(local.as_ref()), session);
    if let Some(expected) = expected_storage {
        assert_eq!(storage, expected);
    }
    let log = NatsSessionLog::new(t.h.jetstream.clone(), &storage);
    tokio::time::timeout(DEADLINE, async {
        loop {
            let events = log.load_events_latest_required_async().await?;
            if events
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
            {
                assert!(!events.iter().any(|(_, entry)| matches!(
                    entry,
                    SessionLogEntry::Error { .. } | SessionLogEntry::Cancel { .. }
                )));
                assert_eq!(prompts(&log).await?.len(), 1);
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("target completion: {}", t.h.logs.text()))??;
    Ok(())
}

pub(super) async fn assert_mixed_version_completed(
    t: &TwoBackends,
    setup: &InteractionOwnedSetup,
    kind: mixed_version::MixedFixtureKind,
    dispatches: u64,
) -> Result<()> {
    let current =
        t.h.store
            .read_context(setup.session.storage_key())
            .await?
            .context("context")?;
    same_completed_invocation(&current, &setup.before)?;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), setup.session.storage_key());
    let entries = log.load_events_latest_required_async().await?;
    assert!(!entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Cancel { .. } | SessionLogEntry::Error { .. }
    )));
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
            .count(),
        1
    );
    assert_eq!(prompts(&log).await?.len(), 1);
    match kind {
        mixed_version::MixedFixtureKind::Fixture2 => assert_tool_effect(&entries, dispatches)?,
        mixed_version::MixedFixtureKind::Fixture4a | mixed_version::MixedFixtureKind::Fixture4b => {
            assert_eq!(dispatches, 0);
            verify_target(t, setup.target_storage.as_deref(), &entries).await?;
        }
    }
    assert_eq!(
        t.h.llm.requests.lock().len(),
        1,
        "only the resumed source or handoff target calls the model"
    );
    Ok(())
}
