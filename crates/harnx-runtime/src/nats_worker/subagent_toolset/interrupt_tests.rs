//! `Toolset::cancel` interrupts a sub-agent's child session directly through
//! the log, without a live worker or blocking model: create the child
//! through the toolset's real `create_session`/`session_config` path (the
//! code under test, so the parent link is production's doing rather than a
//! hand-seeded field), seed its transcript with a pending turn, then cancel
//! by checkpoint alone.
use super::*;
use crate::nats_session_metadata::{ParentLink, SessionMetadataStore};
use crate::nats_worker::ancestor_check::{check_ancestors, AncestorVerdict};
use anyhow::{Context, Result};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::tool::ToolCall;

const AGENT: &str = "helper";
const PARENT: &str = "parent-session";
/// The id dispatch minted for the call on the wire. A uuid in production; the
/// tool server puts it on `ToolInvocationContext::call_id` and the tool server
/// is the only side that ever sees it.
const WIRE_CALL: &str = "call-42";
/// The id the PARENT's transcript gave the same call, delivered to the tool as
/// `__harnx_tool_call_id`. Only this one appears in the parent's log.
const TRANSCRIPT_CALL: &str = "parent-transcript-call";

fn toolset(js: &async_nats::jetstream::Context, metadata: SessionMetadataStore) -> SubagentToolset {
    SubagentToolset::new(
        AGENT,
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    )
}

/// Create a child through `SubagentToolset::create_session`/`session_config`
/// — the same private entry point `run_prompt` uses for a delegated
/// `session_new`/`session_prompt` call — rather than hand-seeding its
/// metadata, so the parent link asserted below is production's doing.
/// Session creation alone needs no live model or worker.
async fn create_child(toolset: &SubagentToolset, parent: &str) -> Result<NatsSession> {
    toolset
        .create_session(None, Some(parent), Some(TRANSCRIPT_CALL))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Seed a pending user message so the log looks like an actively running
/// turn (`interrupt_session` only appends a `Cancel` when it finds one).
async fn seed_active_turn(js: &async_nats::jetstream::Context, storage_key: &str) -> Result<()> {
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key.to_string(), 1);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("user-1".into()),
        role: MessageRole::User,
        content: MessageContent::Text("do the thing".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    Ok(())
}

fn cancel_invocation(child: &str) -> ToolInvocation {
    ToolInvocation {
        tool: SUBAGENT_SESSION_PROMPT_TOOL.to_string(),
        args: json!({}),
        context: ToolInvocationContext {
            call_id: WIRE_CALL.to_string(),
            invoking_session_id: Some(PARENT.to_string()),
            checkpoint: Some(json!({"session_id": child})),
            ..Default::default()
        },
        cancel: CancellationToken::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_interrupts_the_child_and_records_the_parent_link() -> Result<()> {
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = toolset(&js, metadata.clone());

    let session = create_child(&toolset, PARENT).await?;
    let child = session.session_id().to_string();
    let storage_key = session.storage_key().to_string();
    seed_active_turn(&js, &storage_key).await?;

    toolset
        .cancel(cancel_invocation(&child))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key.clone(), 1);
    let entries = log.load_events_latest_async().await?;
    let (_, last) = entries.last().context("child log has no entries")?;
    let SessionLogEntry::Cancel { requested_by, .. } = last else {
        panic!("expected the child log's last entry to be a Cancel, got {last:?}");
    };
    assert!(
        requested_by
            .as_deref()
            .is_some_and(|by| by.starts_with("parent:")),
        "requested_by should be parent-tagged, got {requested_by:?}"
    );

    let stored = metadata
        .get(&storage_key)
        .await?
        .context("child metadata missing")?;
    assert_eq!(
        stored.metadata.parent,
        Some(ParentLink {
            session_id: PARENT.to_string(),
            tool_call_id: TRANSCRIPT_CALL.to_string(),
        }),
        "the link names the call by the id the parent's transcript gave it, \
         which is the only id the ancestor check can look up"
    );

    // A second cancel is idempotent: no additional Cancel is appended.
    toolset
        .cancel(cancel_invocation(&child))
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let entries_after = log.load_events_latest_async().await?;
    assert_eq!(
        entries_after.len(),
        entries.len(),
        "second cancel must not append another entry"
    );

    Ok(())
}

#[tokio::test]
async fn cancel_is_a_no_op_without_a_checkpoint() -> Result<()> {
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = toolset(&js, metadata);

    let invocation = ToolInvocation {
        tool: SUBAGENT_SESSION_PROMPT_TOOL.to_string(),
        args: json!({}),
        context: ToolInvocationContext {
            call_id: WIRE_CALL.to_string(),
            invoking_session_id: Some(PARENT.to_string()),
            checkpoint: None,
            ..Default::default()
        },
        cancel: CancellationToken::new(),
    };
    toolset
        .cancel(invocation)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(())
}

/// A parent transcript that delegated `TRANSCRIPT_CALL` and was then
/// interrupted before the call came back.
async fn seed_interrupted_parent(
    js: &async_nats::jetstream::Context,
    parent_key: &str,
) -> Result<()> {
    let log = NatsSessionLog::new_with_replicas(js.clone(), parent_key.to_string(), 1);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("parent-user".into()),
        role: MessageRole::User,
        content: MessageContent::Text("delegate".into()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            SUBAGENT_SESSION_PROMPT_TOOL.into(),
            json!({}),
            Some(TRANSCRIPT_CALL.into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::cancel_request(
        "cancel-parent".into(),
        "client:test".into(),
    ))
    .await?;
    Ok(())
}

/// End to end for the link the toolset writes: a child created the way a
/// delegated `session_prompt` creates one, under a parent whose turn is then
/// interrupted, must be refused. The two ids differ exactly as production's
/// do, so a link carrying the wire id would leave `check_ancestors` reading
/// "the parent is still waiting" and the child would resume into a turn
/// nobody is listening to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_the_toolset_created_is_refused_under_an_interrupted_parent() -> Result<()> {
    harnx_core::require_nextest();
    let server = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = toolset(&js, metadata.clone());
    let parent_key = harnx_core::session_identity::session_key(None, "refused-parent");

    let session = create_child(&toolset, &parent_key).await?;
    seed_interrupted_parent(&js, &parent_key).await?;

    assert_eq!(
        check_ancestors(&js, &metadata, session.storage_key()).await?,
        AncestorVerdict::Interrupted {
            parent_session: parent_key,
            cancellation_id: Some("cancel-parent".into()),
        },
        "the child of an interrupted parent call must not resume"
    );
    Ok(())
}
