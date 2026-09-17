//! Interruption reaches a sub-agent subtree without any handler forwarding it.
//!
//! Nothing walks the tree. Each level's worker, on being activated, follows its
//! own `ParentLink` up one step and reads the parent's log: a parent turn whose
//! tool call for this child is closed — interrupted, or already answered —
//! means nobody is waiting for this child any more, so the worker interrupts it
//! and records the parent that closed it. Repeating that one step per level is
//! what carries a root interruption to the leaf.
use super::*;
use anyhow::Context;
use harnx_runtime::nats_session_metadata::{
    ParentLink, SessionInitializer as Initializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::nats_worker::{publish_session_activate, SessionActivate};

const ROOT: &str = "hierarchy-root";
const MIDDLE: &str = "hierarchy-middle";
const LEAF: &str = "hierarchy-leaf";

/// One session's storage key plus the invocation its parent delegated to it.
struct Level {
    key: String,
    /// Tool call id this session was created for, as the PARENT's transcript
    /// records it. The `ParentLink` names this one: it is the only id that
    /// appears in the parent's log.
    invocation: String,
}

impl Level {
    /// The id dispatch minted for the same call on the wire, which is what the
    /// delegation marker records. Deliberately unrelated to `invocation`, so a
    /// link built from a wire id would match nothing in the parent's log.
    fn wire_invocation(&self) -> String {
        format!("wire-{}", self.invocation)
    }
}

fn log(
    js: &async_nats::jetstream::Context,
    key: &str,
) -> harnx_runtime::nats_session_log::NatsSessionLog {
    harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), key)
}

fn user(text: &str) -> Entry {
    Entry::Message {
        id: Some(format!("{text}-id")),
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

/// A tool round this session is still waiting on, which is what makes it
/// resumable — and so what makes its worker consult its ancestors.
fn tool_calls(id: &str) -> Entry {
    Entry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "subagent_session_prompt".into(),
            json!({}),
            Some(id.into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    }
}

/// Seed root → middle → leaf as a delegation chain leaves them: each level
/// mid tool round, each child's metadata naming the call it was created for.
async fn seed_chain(js: &async_nats::jetstream::Context) -> Result<[Level; 3]> {
    let metadata = SessionMetadataStore::ensure(js, 1).await?;
    let key = |id: &str| harnx_core::session_identity::session_key(None, id);
    let levels = [
        Level {
            key: key(ROOT),
            invocation: String::new(),
        },
        Level {
            key: key(MIDDLE),
            invocation: "invoke-middle".into(),
        },
        Level {
            key: key(LEAF),
            invocation: "invoke-leaf".into(),
        },
    ];

    for (index, level) in levels.iter().enumerate() {
        let id = [ROOT, MIDDLE, LEAF][index];
        let mut initializer =
            Initializer::inline("", Default::default(), SessionOverrides::default());
        initializer.parent = index.checked_sub(1).map(|parent| ParentLink {
            session_id: levels[parent].key.clone(),
            tool_call_id: level.invocation.clone(),
        });
        metadata
            .create(&SessionMetadata::new(id, initializer))
            .await?;
        log(js, &level.key)
            .append_event_async(&user("wait until cancelled"))
            .await?;
        // Every level but the leaf is waiting on the child below it.
        if let Some(child) = levels.get(index + 1) {
            log(js, &level.key)
                .append_event_async(&tool_calls(&child.invocation))
                .await?;
            log(js, &level.key)
                .append_event_async(&Entry::SubAgentStarted {
                    agent: String::new(),
                    session_id: child.key.clone(),
                    invocation_id: Some(child.wire_invocation()),
                    tool_call_id: Some(child.invocation.clone()),
                    started_at: None,
                })
                .await?;
        }
    }
    // The leaf is waiting on a tool of its own, so it is resumable too.
    log(js, &levels[2].key)
        .append_event_async(&tool_calls("leaf-call"))
        .await?;
    Ok(levels)
}

/// Interrupt one session the way a frontend does.
async fn interrupt(
    js: &async_nats::jetstream::Context,
    client: &async_nats::Client,
    session: &str,
) -> Result<()> {
    let outcome = harnx_runtime::nats_session::interrupt_session(
        js,
        client,
        &harnx_runtime::SessionActivationRoute::ClusterShared,
        harnx_runtime::nats_session::InterruptRequest {
            session_id: session.to_string(),
            cluster: "local".into(),
            cancellation_id: format!("cancel-{session}"),
            requested_by: "client:test".into(),
            reason: "user interrupt".into(),
        },
    )
    .await?;
    anyhow::ensure!(
        matches!(
            outcome,
            harnx_runtime::nats_session::InterruptOutcome::Accepted { .. }
        ),
        "seeded session must be interruptible: {outcome:?}"
    );
    Ok(())
}

/// Activate one descendant and wait for the worker to refuse it, reporting the
/// parent its `Cancel` names.
async fn refused_under(js: &async_nats::jetstream::Context, level: &Level) -> Result<String> {
    publish_session_activate(js, "local", &SessionActivate::new(&level.key)).await?;
    let log = log(js, &level.key);
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if let Some(requested_by) = log
                .load_events_latest_async()
                .await?
                .iter()
                .rev()
                .find_map(|(_, entry)| match entry {
                    Entry::Cancel { requested_by, .. } => Some(requested_by.clone()),
                    _ => None,
                })
                .flatten()
            {
                return Ok::<_, anyhow::Error>(requested_by);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

async fn has_cancel(js: &async_nats::jetstream::Context, key: &str) -> Result<bool> {
    Ok(log(js, key)
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, entry)| matches!(entry, Entry::Cancel { .. })))
}

async fn exercise_hierarchy(cancel_root: bool) -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let [root, middle, leaf] = seed_chain(&js).await?;

    // The middle level's worker died mid-turn without releasing its lease, so
    // nothing is left in the middle of the chain to forward anything: the
    // grandchild is only reachable if each level's own wind-up activation
    // closes the invocation the level below it is waiting on.
    let dead_middle = match cancel_root {
        true => {
            let lease = acquire_worker_lease(&js, &middle.key, "dead-intermediate-worker").await?;
            lease.stop_renewal_for_test().await;
            Some(lease)
        }
        false => None,
    };

    let interrupted = if cancel_root { &root } else { &middle };
    interrupt(&js, &client, &interrupted.key).await?;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut daemon_config = WorkerDaemonConfig::managing("local", "hierarchical-worker");
    daemon_config.lease = short_lease_config();
    let daemon = tokio::spawn(run_worker_daemon(
        local_nats_runtime_config(server.url()),
        daemon_config,
        Some(counting_stub_call_fn(calls.clone())),
        None,
    ));

    // One level at a time: each refusal is what closes the call the level
    // below it is waiting on, so the next activation has something to read.
    let descendants: Vec<(&Level, &Level)> = if cancel_root {
        vec![(&middle, &root), (&leaf, &middle)]
    } else {
        vec![(&leaf, &middle)]
    };
    for (child, parent) in descendants {
        assert_eq!(
            refused_under(&js, child).await?,
            format!("parent:{}", parent.key),
            "the child's Cancel names the parent that closed its invocation"
        );
    }

    if !cancel_root {
        assert!(
            !has_cancel(&js, &root.key).await?,
            "interrupting a child must not reach its parent"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused descendant never reaches the model"
    );
    daemon.abort();
    let _ = daemon.await;
    drop(dead_middle);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_level_interrupt_reaches_grandchild_through_wind_up_activation_when_intermediate_worker_is_dead(
) -> Result<()> {
    exercise_hierarchy(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_child_cancellation_stops_only_its_worker_subtree() -> Result<()> {
    exercise_hierarchy(false).await
}

/// A child whose prompt was interrupted before any worker saw it is not
/// replayed when the session is reopened: the `Cancel` already terminated it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_ownerless_child_prompt_is_not_replayed_when_reopened() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let child = session(server.url(), "ownerless-cancel-child").await?;
    child.enqueue_text("wait until cancelled").await?;
    assert!(child.cancel_pending_turn().await?);
    await_cancel_entry(&js, child.storage_key()).await?;

    let reopened = session(server.url(), child.session_id()).await?;
    assert_eq!(reopened.activate_pending_turn().await?, None);

    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "ownerless-cancel-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
