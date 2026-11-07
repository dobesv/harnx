//! Delivery survives the caller, then a real worker freezes target policy.
use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{CallTimeoutOverride, SessionMetadataStore};
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_metis_worker_with_call_fn, subagent_test_env,
};
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatched_child_runs_after_sender_drop_with_inherited_configured_hard_policy(
) -> Result<()> {
    let _env_guard = env_lock().await;
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let seeded = seed_remote_config(server.url());
    std::fs::write(seeded.config_dir().join("agents/metis.md"), "---\nmodel: test:test-model\nrun_limits:\n  timeout_secs: 172800\n---\nstub worker prompt\n")?;
    let _env = subagent_test_env(server.url(), &seeded);
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = SubagentToolset::new(
        "metis",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata.clone(), 1),
    );
    let parent = RunLimitsRecord::admit_root(
        Default::default(),
        Default::default(),
        chrono::Utc::now(),
        Default::default(),
        None,
        CallTimeoutOverride::from_optional(Some(604800)),
    )?;
    let admitted_at = chrono::Utc::now();
    let args = json!({"message": "continue after sender is gone", "session_id": "live-dispatch", "timeout_secs": -1});
    let context = journaled_context(&js, &parent, admitted_at, &args).await?;
    let cancel = CancellationToken::new();
    let (receipt, _) = dispatch_send(&toolset, args, cancel.clone(), context).await?;
    let handle = receipt.handle;
    let prompt_seq = receipt.prompt.user_msg_seq();
    cancel.cancel();
    drop(receipt.prompt);
    drop(toolset);

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let call_fn = blocking_model(entered.clone(), release.clone());
    let daemon = spawn_metis_worker_with_call_fn(server.url(), call_fn);
    tokio::time::timeout(Duration::from_secs(60), entered.notified())
        .await
        .context("worker never entered model after dispatch")?;
    let storage = harnx_core::session_identity::session_key(Some("metis"), &handle.session_id);
    let limits = metadata
        .get_invocation_limits(&storage, &handle.prompt_id)
        .await?
        .context("worker did not freeze limits")?;
    assert_eq!(limits.run_id, parent.run_id);
    assert_eq!(
        limits.admitted_at.timestamp_millis(),
        admitted_at.timestamp_millis()
    );
    assert_eq!(
        limits.deadline,
        Some(limits.admitted_at + chrono::Duration::seconds(172800)),
        "omitted/nonpositive override must use target policy, not forced 86400 fallback"
    );
    assert!(limits.deadline < parent.deadline);
    release.notify_one();
    let log = NatsSessionLog::new(js, storage);
    let entries = terminal_entries(&log, prompt_seq).await?;
    daemon.abort();
    let _ = daemon.await;
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::Message { content, role, .. } if role.is_assistant() && content.to_transcript_text().contains("child completed without sender"))));
    Ok(())
}

async fn journaled_context(
    js: &async_nats::jetstream::Context,
    parent: &RunLimitsRecord,
    admitted_at: chrono::DateTime<chrono::Utc>,
    args: &Value,
) -> Result<ToolInvocationContext> {
    let context = ToolInvocationContext {
        call_id: "durable-dispatch".into(),
        invoking_session_id: Some("trusted-parent".into()),
        run_context: Some(harnx_toolset::AutonomousRunContext {
            snapshot: serde_json::to_value(parent)?,
            started_at_ms: admitted_at.timestamp_millis().try_into()?,
        }),
        ..Default::default()
    };
    let request = harnx_toolset::ToolRequest {
        operation_id: "live-operation".into(),
        call_id: context.call_id.clone(),
        tool: "session_send".into(),
        args: args.clone(),
        run_context: context.run_context.clone(),
        parent_session_id: context.invoking_session_id.clone(),
        replay: None,
        parent_agent: None,
        parent_local_session_id: None,
        tool_call_id: None,
        tool_round: None,
        capabilities: Default::default(),
    };
    let journal =
        harnx_toolset_server::invocation_journal::InvocationJournal::ensure(js, 1).await?;
    journal
        .record(&request, ("session_send", "metis", "server"))
        .await?;
    Ok(ToolInvocationContext {
        checkpoint_store: Some(Arc::new(journal.checkpoint_store(&request))),
        partial_result_store: Some(Arc::new(journal.partial_result_store(&request))),
        ..context
    })
}

fn blocking_model(
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new({
        let entered = entered.clone();
        let release = release.clone();
        move |_, _, _| {
            let entered = entered.clone();
            let release = release.clone();
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok((
                    "child completed without sender".into(),
                    None,
                    vec![],
                    Default::default(),
                ))
            })
        }
    })
}

async fn terminal_entries(
    log: &NatsSessionLog,
    prompt_seq: u64,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let entries = log.load_events_latest_async().await?;
            if crate::nats_session::invocation_terminal_seq(&entries, prompt_seq).is_some() {
                break Ok::<_, anyhow::Error>(entries);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("worker never completed durable dispatched prompt")?
}
