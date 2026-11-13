//! session_prompt to a busy child joins the running turn instead of failing
//! with "session busy" (issue #2672), and returns the response that follows
//! the joining parent's message, under the joined invocation.
use super::*;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use crate::nats_worker::tests::{env_lock, seed_remote_config, subagent_test_env};
use anyhow::{Context, Result};
use harnx_core::message::MessageContent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn prompt_params(call_id: &str, message: &str, parent: &str) -> PromptParams {
    PromptParams {
        content: MessageContent::Text(message.into()),
        session_id: Some("busy-child".into()),
        parent_session_id: Some(parent.into()),
        tool_call_id: None,
        timeout_secs: None,
        cancel: CancellationToken::new(),
        context: harnx_toolset::ToolInvocationContext {
            call_id: call_id.into(),
            invoking_session_id: Some(parent.into()),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            ..Default::default()
        },
    }
}

/// Hold the model call until the test confirms the joined message is durable.
fn held_reply_model(entered: Arc<Notify>, release: Arc<Notify>) -> crate::agent_loop::AgentCallFn {
    let first_call = Arc::new(AtomicBool::new(true));
    Arc::new(move |_, _, _| {
        let entered = entered.clone();
        let release = release.clone();
        let first_call = first_call.clone();
        Box::pin(async move {
            if first_call.swap(false, Ordering::SeqCst) {
                entered.notify_one();
                release.notified().await;
            }
            Ok(("child reply".into(), None, vec![], Default::default()))
        })
    })
}

async fn wait_for_durable_join(
    js: &async_nats::jetstream::Context,
    metadata: &SessionMetadataStore,
    storage: &str,
) -> Result<()> {
    let log = crate::nats_session_log::NatsSessionLog::new(js.clone(), storage);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let binding = metadata.prompt_admission(storage, "call-b").await?;
            if binding
                .as_ref()
                .is_some_and(|binding| binding.invocation_id.as_str() == "call-a")
                && log.load_events_async().await?.iter().any(|(_, entry)| {
                    matches!(entry, harnx_core::session::SessionLogEntry::Message { id: Some(id), .. } if id == "call-b")
                })
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("joined prompt was not durably appended to call-a")??;
    Ok(())
}

async fn join_parent_b(
    js: async_nats::jetstream::Context,
    metadata: SessionMetadataStore,
) -> Result<CompletedSubagentTurn> {
    let toolset = make_toolset(&js, &metadata);
    run_prompt(
        &toolset,
        prompt_params("call-b", "do B instead", "parent-b"),
    )
    .await
    .map_err(|error| anyhow::anyhow!("parent B prompt failed: {error}"))
}

async fn checked_turn_b(turn: Result<CompletedSubagentTurn>) -> Result<()> {
    let turn = turn?;
    assert_child_response(&turn, "joined session_prompt returned no response")
}

fn make_toolset(
    js: &async_nats::jetstream::Context,
    metadata: &SessionMetadataStore,
) -> SubagentToolset {
    SubagentToolset::new(
        "metis",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata.clone(), 1),
    )
}

fn assert_child_response(turn: &CompletedSubagentTurn, missing: &'static str) -> Result<()> {
    let response = turn
        .result
        .as_ref()
        .and_then(|result| result.response.as_deref())
        .context(missing)?;
    assert_eq!(response, "child reply");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_prompt_to_busy_child_joins_and_answers_after_its_message() -> Result<()> {
    let _env_guard = env_lock().await;
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let seeded = seed_remote_config(server.url());
    std::fs::write(
        seeded.config_dir().join("agents/metis.md"),
        "---\nmodel: test:test-model\n---\nstub worker prompt\n",
    )?;
    let _env = subagent_test_env(server.url(), &seeded);
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let daemon = crate::nats_worker::tests::spawn_metis_worker_with_call_fn(
        server.url(),
        held_reply_model(entered.clone(), release.clone()),
    );
    let storage = harnx_core::session_identity::session_key(Some("metis"), "busy-child");

    // Parent A starts the child's turn and blocks inside the model call.
    let (js_a, metadata_a) = (js.clone(), metadata.clone());
    let parent_a = tokio::spawn(async move {
        let toolset = make_toolset(&js_a, &metadata_a);
        run_prompt(
            &toolset,
            prompt_params("call-a", "original work", "parent-a"),
        )
        .await
        .map_err(|error| anyhow::anyhow!("parent A prompt failed: {error}"))
    });
    tokio::time::timeout(Duration::from_secs(60), entered.notified())
        .await
        .context("worker never entered the child's first model call")?;
    assert_eq!(
        metadata.admission_head(&storage).await?.as_deref(),
        Some("call-a"),
        "child is busy with parent A's invocation"
    );

    // Parent B joins concurrently. Release the model only after its message is
    // durable and bound to A's invocation, closing the bind-before-append gap.
    let (js_b, metadata_b) = (js.clone(), metadata.clone());
    let parent_b = tokio::spawn(join_parent_b(js_b, metadata_b));
    wait_for_durable_join(&js, &metadata, &storage).await?;
    release.notify_one();
    let turn_b = tokio::time::timeout(Duration::from_secs(60), parent_b)
        .await
        .context("joined session_prompt never completed")?
        .context("parent B prompt task panicked")?;
    checked_turn_b(turn_b).await?;

    let turn_a = tokio::time::timeout(Duration::from_secs(60), parent_a)
        .await
        .context("parent A prompt task panicked")?
        .context("parent A prompt never completed")?
        .context("parent A prompt never completed")?;
    assert_child_response(&turn_a, "original session_prompt returned no response")?;

    // Everything ran under the single invocation parent A started: B's
    // prompt is bound to it and no second admission head was minted.
    assert_eq!(
        metadata.admission_head(&storage).await?.as_deref(),
        Some("call-a")
    );
    let binding = metadata
        .prompt_admission(&storage, "call-b")
        .await?
        .context("joined prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "call-a");
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
