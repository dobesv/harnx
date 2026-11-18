//! Recover only the trusted control decision committed with a closed tool round.
use anyhow::Result;
use harnx_core::{session::SessionLogEntry, tool::SwitchAgentData};

pub(in crate::nats_worker) enum HandoffRecovery {
    FinishTurn(u64),
    AlreadyFinished,
}

pub(super) struct SelectedHandoff {
    pub results_seq: u64,
    pub switch: SwitchAgentData,
}

pub(super) fn selected_handoff(entries: &[(u64, SessionLogEntry)]) -> Option<SelectedHandoff> {
    let (seq, entry) = entries
        .iter()
        .rev()
        .find(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))?;
    let SessionLogEntry::ToolResults {
        results,
        handoff_selection: Some(selected),
        ..
    } = entry
    else {
        return None;
    };
    let result = results.iter().find(|result| {
        result.id.as_ref() == Some(selected) && !crate::handoff_result::is_error(&result.output)
    })?;
    let mut switch = result.switch_agent.clone()?;
    switch.tool_call_id = Some(selected.clone());
    Some(SelectedHandoff {
        results_seq: *seq,
        switch,
    })
}

pub(super) fn committed_after(
    entries: &[(u64, SessionLogEntry)],
    selection: &SelectedHandoff,
) -> bool {
    entries.iter().any(|(seq, entry)| {
        *seq > selection.results_seq
            && matches!(entry, SessionLogEntry::HandoffCommitted { handoff_tool_call_id, .. }
                if handoff_tool_call_id == &selection.switch.tool_call_id)
    })
}

/// Also finish a dispatch that committed before the source TurnEnd. Otherwise
/// redelivery could call the model again in that second crash window.
pub(in crate::nats_worker) async fn reconcile(
    args: &super::handoff_dispatch::HandoffDispatchArgs<'_>,
) -> Result<Option<HandoffRecovery>> {
    let backend = args.backend();
    let entries = backend.load_events_latest_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    let Some(selection) = selected_handoff(&effective) else {
        return Ok(None);
    };
    if round_superseded(&effective, selection.results_seq) {
        return Ok(None);
    }
    let invocation = args.config.read().run_context.clone();
    let Some(invocation) = invocation else {
        anyhow::bail!("handoff recovery requires frozen invocation policy");
    };
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(
        args.jetstream_ctx,
        args.replicas,
    )
    .await?;
    let round = effective
        .iter()
        .rposition(|(seq, entry)| {
            *seq < selection.results_seq && matches!(entry, SessionLogEntry::ToolCalls { .. })
        })
        .unwrap_or(effective.len());
    // Don't consume a user queued while tools ran. Prompt bindings also cover
    // fixed A2A tickets and interactive submissions whose ID differs from run ID.
    let cursor = store
        .invocation_prompt_seq(
            args.source_session_id,
            invocation.invocation_id.as_str(),
            &effective[..round],
        )
        .await?;
    let Some(cursor) = cursor else {
        // An activation for a newer invocation must not consume an older one.
        return Ok(None);
    };
    // TurnEnd is excluded from effective transcript reconstruction. An old
    // activation must settle here, not reconstruct the handoff as model input.
    if entries.iter().any(|(seq, entry)| {
        *seq > selection.results_seq
            && matches!(entry, SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq >= cursor)
    }) {
        return Ok(Some(HandoffRecovery::AlreadyFinished));
    }
    if !committed_after(&effective, &selection) {
        args.dispatch(selection.switch).await?;
    }
    Ok(Some(HandoffRecovery::FinishTurn(cursor)))
}

fn round_superseded(effective: &[(u64, SessionLogEntry)], results_seq: u64) -> bool {
    effective.iter().any(|(seq, entry)| {
        *seq > results_seq
            && matches!(
                entry,
                SessionLogEntry::Cancel { .. }
                    | SessionLogEntry::Clear
                    | SessionLogEntry::ToolCalls { .. }
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_supersedes_only_pre_clear_handoff_rounds() {
        let entries = [(2, SessionLogEntry::Clear)];
        assert!(round_superseded(&entries, 1));
        assert!(!round_superseded(&entries, 3));
    }
}
