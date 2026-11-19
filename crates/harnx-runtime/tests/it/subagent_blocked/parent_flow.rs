use super::fixture::*;
use anyhow::{Context, Result};
use harnx_core::{
    event::{NullSink, SubAgentProgressStatus},
    pending_interaction::PendingInteractionRequest,
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    client::CompletionTokenUsage,
    nats_session::{completed_subagent_progress, NatsSession, NatsSessionConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionInitializer,
};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const PARENT: &str = "blocked-parent";
const ROOT: &str = "tree-root";

fn parent_child_model() -> AgentCallFn {
    let parent_calls = AtomicUsize::new(0);
    let child_calls = AtomicUsize::new(0);
    let root_calls = AtomicUsize::new(0);
    Arc::new(move |_input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let parent = agent == PARENT;
        let root = agent == ROOT;
        let counter = if root {
            &root_calls
        } else if parent {
            &parent_calls
        } else {
            &child_calls
        };
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            let calls = if root {
                if first {
                    vec![ToolCall::new(
                        format!("{PARENT}_session_prompt"),
                        json!({"session_id": "parent-flow", "message": "delegate below"}),
                        Some("root-delegate".into()),
                        None,
                    )]
                } else {
                    vec![]
                }
            } else {
                match (parent, first) {
                    (true, true) => vec![ToolCall::new(
                        format!("{AGENT_NAME}_session_prompt"),
                        json!({"session_id": SESSION_ID, "message": "run child"}),
                        Some("delegate".into()),
                        None,
                    )],
                    (false, true) => vec![ToolCall::new(
                        "probe_run".into(),
                        json!({}),
                        Some("inner-tool".into()),
                        None,
                    )],
                    _ => vec![],
                }
            };
            let response = if root {
                "root finished"
            } else if parent {
                "parent kept running"
            } else {
                "child finished"
            };
            Ok((
                response.into(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_real_parent_persists_summary_and_finishes_while_child_waits() -> Result<()>
{
    require_nextest();
    let harness = Harness::required_start(BASH_CONFIRM, parent_child_model()).await?;
    std::fs::write(harness.config_dir.join(format!("agents/{PARENT}.md")), format!(
        "---\nmodel: openai:test-model\nuse_tools: [{AGENT_NAME}_session_prompt]\n---\nDelegate to child.\n"
    ))?;
    let parent = tree_session(&harness, PARENT, "parent-flow").await?;
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        parent
            .with_external_admission()
            .run_turn("delegate now", Arc::new(NullSink), None),
    )
    .await??;
    assert_eq!(result.response.as_deref(), Some("parent kept running"));
    assert!(result.error.is_none(), "{:?}", result.error);
    let parent_key = harnx_core::session_identity::session_key(Some(PARENT), "parent-flow");
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), &parent_key, 1);
    let entries = log.load_events_async().await?;
    assert!(
        !entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::InteractionRequested { .. } | SessionLogEntry::SuspensionReady { .. }
        )),
        "child must never suspend parent"
    );
    let output = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.id.as_deref() == Some("delegate"))
                .map(|r| &r.output),
            _ => None,
        })
        .context("parent persisted actual delegation result")?;
    assert_eq!(output["status"], "blocked");
    assert_eq!(output["terminal"], false);
    let caught = completed_subagent_progress(&entries, 0);
    let progress = caught.first().context("blocked progress catch-up")?;
    assert_eq!(progress.status, SubAgentProgressStatus::Running);
    let pending = progress
        .pending_interaction
        .as_ref()
        .context("pending summary")?;
    assert_eq!(
        serde_json::to_value(pending)?,
        output["pending_interaction"]
    );
    assert_eq!(pending.agent, AGENT_NAME);
    assert_eq!(pending.session_id, SESSION_ID);
    assert_eq!(pending.tool_name, "probe_run");
    let PendingInteractionRequest::Interaction {
        interaction_id,
        request_seq,
        ..
    } = &pending.request
    else {
        anyhow::bail!("expected child confirm");
    };
    assert!(harness
        .approve(interaction_id, *request_seq)
        .await?
        .is_applied());
    harness.wait_for_turn_end().await?;
    Ok(())
}

async fn tree_session(harness: &Harness, agent: &str, id: &str) -> Result<NatsSession> {
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named(agent, Default::default()),
            session_id: Some(id.into()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        harness.client.clone(),
        harness.js.clone(),
        harnx_core::abort::create_abort_signal(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_blocked_real_grandchild_is_not_advertised_to_root() -> Result<()> {
    require_nextest();
    let mut harness = Harness::required_start(BASH_CONFIRM, parent_child_model()).await?;
    for (agent, child) in [(PARENT, AGENT_NAME), (ROOT, PARENT)] {
        std::fs::write(harness.config_dir.join(format!("agents/{agent}.md")), format!(
            "---\nmodel: openai:test-model\nuse_tools: [{child}_session_prompt]\n---\nDelegate below.\n"
        ))?;
    }
    // Register the newly created intermediate agent's delegation tool.
    harness.restart_worker_with_identity("tree-worker").await?;
    let root = tree_session(&harness, ROOT, "root-flow").await?;
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        root.with_external_admission()
            .run_turn("delegate twice", Arc::new(NullSink), None),
    )
    .await??;
    assert_eq!(result.response.as_deref(), Some("root finished"));
    assert!(result.error.is_none(), "{:?}", result.error);
    let root_key = harnx_core::session_identity::session_key(Some(ROOT), "root-flow");
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), &root_key, 1);
    let entries = log.load_events_async().await?;
    let root_progress = completed_subagent_progress(&entries, 0);
    let progress = root_progress
        .first()
        .context("root received direct child result")?;
    assert_eq!(progress.agent, PARENT);
    assert_eq!(progress.status, SubAgentProgressStatus::Done);
    assert!(
        progress.pending_interaction.is_none(),
        "grandchild must not be relayed"
    );
    let parent_key = harnx_core::session_identity::session_key(Some(PARENT), "parent-flow");
    let log = NatsSessionLog::new_with_replicas(harness.js.clone(), &parent_key, 1);
    let parent_entries = log.load_events_async().await?;
    let parent_progress = completed_subagent_progress(&parent_entries, 0);
    let pending = parent_progress
        .first()
        .and_then(|p| p.pending_interaction.as_ref())
        .context("intermediate parent sees its direct child's request")?;
    assert_eq!(pending.agent, AGENT_NAME);
    let (seq, id, _) = harness.wait_for_suspension().await?;
    assert!(harness.approve(&id, seq).await?.is_applied());
    harness.wait_for_turn_end().await?;
    Ok(())
}
