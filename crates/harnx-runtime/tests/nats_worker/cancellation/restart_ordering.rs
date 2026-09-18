//! A restarted worker must not resume a child whose parent invocation is over.
//!
//! Activations survive a worker's death, so the first thing a replacement sees
//! can be the child of a turn that was interrupted while it was away. The
//! child's own log says nothing about that — only the parent's does — so the
//! worker follows the `ParentLink` upward before it resumes anything.
use super::*;
use anyhow::Context;
use harnx_runtime::nats_session_metadata::{
    ParentLink, SessionInitializer as Initializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::nats_worker::{publish_session_activate, SessionActivate};

const ROOT: &str = "restart-root";
const CHILD: &str = "restart-child";
/// The id the root's transcript gave the delegating call. The child's
/// `ParentLink` names this one, because it is the only id the parent's log
/// carries.
const CALL: &str = "call-1";
/// The id dispatch minted for the same call on the wire, recorded by the
/// delegation marker. Deliberately different from `CALL`: a link built from
/// the wire id would find nothing in the root's transcript.
const WIRE_CALL: &str = "wire-call-1";

fn user(text: &str) -> Entry {
    Entry::Message {
        id: Some(format!("{text}-id")),
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn tool_calls(name: &str, id: &str) -> Entry {
    Entry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(name.into(), json!({}), Some(id.into()), None)],
        timestamp: None,
        fence_token: Some(1),
    }
}

fn log(
    js: &async_nats::jetstream::Context,
    key: &str,
) -> harnx_runtime::nats_session_log::NatsSessionLog {
    harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), key)
}

/// Both sessions as a delegation leaves them: the root mid sub-agent call, the
/// child mid tool round of its own, and the child's metadata naming the call it
/// was created for.
async fn seed(js: &async_nats::jetstream::Context) -> Result<(String, String)> {
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let root_key = harnx_core::session_identity::session_key(None, ROOT);
    let child_key = harnx_core::session_identity::session_key(None, CHILD);
    let inline = || Initializer::inline("", Default::default(), SessionOverrides::default());
    metadata
        .create(&SessionMetadata::new(ROOT, inline()))
        .await?;
    let mut child_initializer = inline();
    child_initializer.parent = Some(ParentLink {
        session_id: root_key.clone(),
        tool_call_id: CALL.into(),
    });
    metadata
        .create(&SessionMetadata::new(CHILD, child_initializer))
        .await?;

    log(js, &root_key)
        .append_event_async(&user("delegate"))
        .await?;
    log(js, &root_key)
        .append_event_async(&tool_calls("subagent_session_prompt", CALL))
        .await?;
    log(js, &root_key)
        .append_event_async(&Entry::SubAgentStarted {
            agent: String::new(),
            session_id: child_key.clone(),
            invocation_id: Some(WIRE_CALL.into()),
            tool_call_id: Some(CALL.into()),
            started_at: None,
        })
        .await?;
    log(js, &child_key)
        .append_event_async(&user("child work"))
        .await?;
    log(js, &child_key)
        .append_event_async(&tool_calls("slow_tool", "child-call"))
        .await?;
    Ok((root_key, child_key))
}

/// Wait for a session's own `Cancel` and report who asked for it.
async fn await_cancel_requester(
    log: &harnx_runtime::nats_session_log::NatsSessionLog,
) -> Result<String> {
    poll_until(async || {
        Ok(log
            .load_events_latest_async()
            .await?
            .iter()
            .any(|(_, entry)| matches!(entry, Entry::Cancel { .. })))
    })
    .await?;
    log.load_events_latest_async()
        .await?
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            Entry::Cancel { requested_by, .. } => Some(requested_by.clone()),
            _ => None,
        })
        .flatten()
        .context("the Cancel records who closed the invocation")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_activation_after_restart_does_not_resume_under_interrupted_parent() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let (root_key, child_key) = seed(&js).await?;

    // The root is interrupted while no worker is alive to hear it.
    let outcome = harnx_runtime::nats_session::interrupt_session(
        &js,
        &client,
        &harnx_runtime::SessionActivationRoute::ClusterShared,
        harnx_runtime::nats_session::InterruptRequest {
            session_id: root_key.clone(),
            cluster: "local".into(),
            cancellation_id: "cancel-root".into(),
            requested_by: "client:test".into(),
            reason: "user interrupt".into(),
        },
    )
    .await?;
    assert!(matches!(
        outcome,
        harnx_runtime::nats_session::InterruptOutcome::Accepted { .. }
    ));
    // The child's activation is queued before the replacement worker exists,
    // so it can be the first thing that worker sees.
    publish_session_activate(&js, "local", &SessionActivate::new(&child_key)).await?;

    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "restart-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;

    // The child refuses, and records the parent that closed its invocation.
    assert_eq!(
        await_cancel_requester(&log(&js, &child_key)).await?,
        format!("parent:{root_key}")
    );

    // The interrupted root owes its sub-agent call a result.
    let root_log = log(&js, &root_key);
    poll_until(async || {
        Ok(root_log
            .load_events_latest_async()
            .await?
            .iter()
            .any(|(_, entry)| {
                matches!(entry, Entry::ToolResults { results, .. }
                if results.iter().any(|r| r.id.as_deref() == Some(CALL)))
            }))
    })
    .await?;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "neither session may reach the model again"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// A message typed after Ctrl+C is still a turn. The activation that finds
/// the interrupted session winds it up first and then runs that turn, rather
/// than acknowledging itself and leaving the input for nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_queued_behind_the_interrupt_runs_after_the_wind_up() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let session = session(server.url(), "steer-after-interrupt").await?;
    let key = session.storage_key().to_string();
    let log = log(&js, &key);
    log.append_event_async(&user("do the thing")).await?;
    log.append_event_async(&tool_calls("slow_tool", "cut-off-call"))
        .await?;
    log.append_event_async(&Entry::cancel_request(
        "cancel-1".into(),
        "client:test".into(),
    ))
    .await?;

    let prompts = Arc::new(AsyncMutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "steer-worker",
        fold_capture_call_fn(calls.clone(), prompts.clone()),
    )
    .await?;
    session.enqueue_text("actually, do this instead").await?;

    poll_until(async || {
        Ok(prompts
            .lock()
            .await
            .iter()
            .any(|prompt: &String| prompt.contains("actually, do this instead")))
    })
    .await?;
    let entries = log.load_events_latest_async().await?;
    assert!(
        entries.iter().any(
            |(_, entry)| matches!(entry, Entry::ToolResults { results, .. }
            if results.iter().any(|r| r.id.as_deref() == Some("cut-off-call")))
        ),
        "the interrupted round is wound up before the queued turn runs: {entries:?}"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
