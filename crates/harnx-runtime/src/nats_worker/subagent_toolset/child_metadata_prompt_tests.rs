//! End-to-end broker-backed coverage for the `metadata` argument through the
//! real `session_new` and `session_prompt` handlers, against a running
//! worker: create with metadata, existing-session update, busy join, and
//! invalid input that dispatches nothing (plan #2970, T2).
//!
//! Every test name carries `nats` so the `test(/nats/)` filter keeps it in
//! the broker nextest group.

use super::child_metadata_tests::{assert_patch_applied, patch_json};
use super::termination::busy_join_tests::held_reply_model;
use super::{SubagentNats, SubagentSessionRoute, SubagentToolset};
use crate::nats_test_common::NatsServerHandle;
use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_metis_worker_with_call_fn, subagent_test_env,
};
use crate::test_environment::EnvGuard;
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A stub model that always answers with the same reply.
fn reply_model() -> crate::agent_loop::AgentCallFn {
    Arc::new(|_, _, _| {
        Box::pin(async { Ok(("child reply".into(), None, vec![], Default::default())) })
    })
}

struct PromptFixture {
    server: NatsServerHandle,
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
    journal: InvocationJournal,
}

async fn prompt_fixture() -> Result<PromptFixture> {
    let (server, js, toolset) =
        super::child_metadata_tests::broker_toolset("metis", "local").await?;
    let journal = InvocationJournal::ensure(&js, 1).await?;
    Ok(PromptFixture {
        server,
        js,
        toolset,
        journal,
    })
}

impl PromptFixture {
    fn storage(&self, session_id: &str) -> String {
        harnx_core::session_identity::session_key(Some("metis"), session_id)
    }

    async fn record(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::nats_session_metadata::MetadataRecord>> {
        self.toolset
            .session_metadata
            .get(&self.storage(session_id))
            .await
    }

    async fn events(&self, session_id: &str) -> Result<Vec<(u64, SessionLogEntry)>> {
        crate::nats_session_log::NatsSessionLog::new(self.js.clone(), self.storage(session_id))
            .load_events_latest_async()
            .await
    }

    /// Journal a `session_prompt` request with a fresh call id.
    async fn request(&self, call_id: &str, args: Value) -> Result<ToolRequest> {
        super::child_metadata_tests::journal_tool_request(
            &self.journal,
            call_id,
            super::child_metadata_tests::ToolJournalIdentity {
                tool: "session_prompt",
                agent: "metis",
                parent: "parent-x",
            },
            args,
        )
        .await
    }

    /// The invocation context exactly as the tool server rebuilds it from
    /// the journal, saved checkpoint included.
    async fn context(&self, request: &ToolRequest) -> Result<ToolInvocationContext> {
        super::child_metadata_tests::journaled_invocation_context(&self.journal, request).await
    }

    /// Run the real `session_prompt` handler for a journaled request.
    async fn prompt(&self, request: &ToolRequest) -> Result<Value> {
        self.toolset
            .session_prompt(
                request.args.clone(),
                CancellationToken::new(),
                self.context(request).await?,
            )
            .await
            .map_err(anyhow::Error::new)
    }

    async fn checkpoint(&self, request: &ToolRequest) -> Result<Value> {
        self.journal
            .get(request)
            .await?
            .context("journal row missing")?
            .checkpoint
            .context("checkpoint missing")
    }
}

/// The full metis-worker environment: seeded config, env guards and a
/// spawned daemon with a stub model. The seeded config stays alive for the
/// whole test — it owns the temporary config directory the child's agent
/// loads from.
async fn spawn_worker(
    server: &NatsServerHandle,
    call_fn: crate::agent_loop::AgentCallFn,
) -> Result<(
    tokio::task::JoinHandle<anyhow::Result<()>>,
    crate::nats_worker::tests::SeededRemoteParentConfig,
    (EnvGuard, EnvGuard, EnvGuard),
)> {
    let seeded = seed_remote_config(server.url());
    std::fs::write(
        seeded.config_dir().join("agents/metis.md"),
        "---\nmodel: test:test-model\nuse_tools: \"*\"\n---\nstub worker prompt\n",
    )?;
    let env = subagent_test_env(server.url(), &seeded);
    let daemon = spawn_metis_worker_with_call_fn(server.url(), call_fn);
    Ok((daemon, seeded, env))
}

/// A generated-ID `session_new` with metadata runs the child's first turn and
/// carries the patch in the first canonical record, stamped with the trusted
/// creation identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_session_new_metadata_creates_child_with_first_record_state() -> Result<()> {
    let _env_guard = env_lock().await;
    let fixture = prompt_fixture().await?;
    let (daemon, _seeded, _env) = spawn_worker(&fixture.server, reply_model()).await?;
    let context = ToolInvocationContext {
        call_id: "new-call".into(),
        run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
        ..Default::default()
    };
    let value = fixture
        .toolset
        .session_new(
            json!({"metadata": patch_json()}),
            CancellationToken::new(),
            context,
        )
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(value["response"], "child reply", "child answered");
    let session_id = value["session_id"].as_str().context("session id")?;
    let record = fixture.record(session_id).await?.expect("record exists");
    assert_patch_applied(&record);
    assert_eq!(
        record.metadata.creation_invocation.as_deref(),
        Some("new-call"),
        "reservation stamps the trusted creation identity"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// A patch-bearing `session_prompt` against a missing explicit session
/// creates it: the creation guard lands in the checkpoint, the first record
/// carries the patch, and the child's turn answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_prompt_metadata_creates_explicit_session_end_to_end() -> Result<()> {
    let _env_guard = env_lock().await;
    // qual:allow(dry, fragment) reason: "The e2e tests share the env-lock + fixture + worker + journaled-request preamble by design; the env lock and seeded config must stay owned by each test body, so hoisting them into a helper would drop the lock when the helper returns."
    let fixture = prompt_fixture().await?;
    let (daemon, _seeded, _env) = spawn_worker(&fixture.server, reply_model()).await?;
    let request = fixture
        .request(
            "create-call",
            json!({
                "message": "work",
                "session_id": "prompt-create-meta",
                "metadata": patch_json(),
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let value = fixture.prompt(&request).await?;
    assert_eq!(value["response"], "child reply");
    assert_eq!(value["session_id"], "prompt-create-meta");
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["metadata_creation"], true, "creation guard");
    let record = fixture
        .record("prompt-create-meta")
        .await?
        .expect("record exists");
    assert_patch_applied(&record);
    assert_eq!(
        record.metadata.creation_invocation.as_deref(),
        Some("create-call"),
        "explicit create stamps its own provenance"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// A patch-bearing `session_prompt` against an existing session runs the
/// child's turn and applies title, variables and properties together in one
/// revision-guarded update.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_prompt_metadata_updates_existing_session_end_to_end() -> Result<()> {
    let _env_guard = env_lock().await;
    // qual:allow(dry, fragment) reason: "The e2e tests share the env-lock + fixture + worker + journaled-request preamble by design; the env lock and seeded config must stay owned by each test body, so hoisting them into a helper would drop the lock when the helper returns."
    let fixture = prompt_fixture().await?;
    let (daemon, _seeded, _env) = spawn_worker(&fixture.server, reply_model()).await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({
                "message": "hi",
                "session_id": "prompt-update-meta",
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let value = fixture.prompt(&seed).await?;
    assert_eq!(value["response"], "child reply");
    let created = fixture
        .record("prompt-update-meta")
        .await?
        .expect("record exists");
    assert!(created.metadata.title.value.is_none());

    let request = fixture
        .request(
            "update-call",
            json!({
                "message": "hi",
                "session_id": "prompt-update-meta",
                "metadata": patch_json(),
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let value = fixture.prompt(&request).await?;
    assert_eq!(value["response"], "child reply");
    let record = fixture
        .record("prompt-update-meta")
        .await?
        .expect("record exists");
    // KV revisions are global stream sequences shared by every key in the
    // metadata bucket, so one CAS shows up as one advance, not a fixed
    // revision number.
    assert!(
        record.revision > created.revision,
        "one CAS advanced the record ({} > {})",
        record.revision,
        created.revision
    );
    assert_patch_applied(&record);
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["metadata_revision"], created.revision,
        "pinned revision"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// An invalid `metadata` argument fails through the real handler with no
/// mutation and no dispatch: an existing record is untouched, nothing is
/// appended, and a missing session stays missing.
#[tokio::test]
async fn nats_prompt_invalid_metadata_dispatches_nothing() -> Result<()> {
    let fixture = prompt_fixture().await?;
    fixture
        .toolset
        .create_session(Some("invalid-meta".into()), None, None)
        .await?;
    let before = fixture
        .record("invalid-meta")
        .await?
        .expect("record exists");

    // user_id is a read-only identity property: the pre-flight validation
    // must reject it before anything is resolved or created.
    let request = fixture
        .request(
            "invalid-call",
            json!({
                "message": "work",
                "session_id": "invalid-meta",
                "metadata": {"properties": {"set": [{"name": "user_id", "value": "attacker"}]}},
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let error = match fixture.prompt(&request).await {
        Ok(_) => anyhow::bail!("invalid metadata unexpectedly dispatched"),
        Err(error) => error,
    };
    let ToolInvokeError::Recoverable(message) = error
        .downcast_ref::<ToolInvokeError>()
        .context("tool error")?
    else {
        anyhow::bail!("expected a recoverable tool error, got: {error}");
    };
    assert!(
        message.contains("invalid child session metadata"),
        "got: {message}"
    );
    let record = fixture
        .record("invalid-meta")
        .await?
        .expect("record exists");
    assert_eq!(record.revision, before.revision, "no mutation");
    assert_eq!(record.metadata, before.metadata, "record untouched");
    assert_eq!(
        fixture.events("invalid-meta").await?.len(),
        0,
        "nothing appended"
    );
    Ok(())
}

/// A patch-bearing `session_prompt` to a busy child joins the running turn,
/// applies the patch once, and both callers get the turn's answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_prompt_metadata_joins_busy_child_and_applies_once() -> Result<()> {
    let _env_guard = env_lock().await;
    let fixture = prompt_fixture().await?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (daemon, _seeded, _env) = spawn_worker(
        &fixture.server,
        held_reply_model(entered.clone(), release.clone()),
    )
    .await?;
    let session_id = "busy-prompt-meta";

    // Parent A starts the child's turn and blocks inside the model call.
    let seed = fixture
        .request(
            "hold-call",
            json!({
                "message": "original work",
                "session_id": session_id,
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let seed_context = fixture.context(&seed).await?;
    let js = fixture.js.clone();
    let metadata = fixture.toolset.session_metadata.clone();
    let toolset_a = SubagentToolset::new(
        "metis",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    );
    let seed_args = seed.args.clone();
    let parent_a = tokio::spawn(async move {
        toolset_a
            .session_prompt(seed_args, CancellationToken::new(), seed_context)
            .await
            .map_err(anyhow::Error::new)
    });
    tokio::time::timeout(Duration::from_secs(60), entered.notified())
        .await
        .context("worker never entered the child's first model call")?;
    let created = fixture
        .record(session_id)
        .await?
        .expect("seed created the record before the model call");

    // Parent B joins the busy turn with a metadata patch. The join runs in a
    // spawned task: the durable binding and user-row append happen inside
    // session_prompt, so the poll below can only observe them once it runs.
    let join = fixture
        .request(
            "join-call",
            json!({
                "message": "do B instead",
                "session_id": session_id,
                "metadata": patch_json(),
                "__harnx_parent_session_id": "parent-x",
            }),
        )
        .await?;
    let join_context = fixture.context(&join).await?;
    let js = fixture.js.clone();
    let metadata = fixture.toolset.session_metadata.clone();
    let toolset_b = SubagentToolset::new(
        "metis",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    );
    let join_args = join.args.clone();
    let parent_b = tokio::spawn(async move {
        toolset_b
            .session_prompt(join_args, CancellationToken::new(), join_context)
            .await
            .map_err(anyhow::Error::new)
    });
    wait_for_durable_join(&fixture, session_id, "join-call").await?;
    release.notify_one();
    let value_b = tokio::time::timeout(Duration::from_secs(60), parent_b)
        .await
        .context("joined session_prompt never completed")?
        .context("joined session_prompt task panicked")?
        .context("joined session_prompt failed")?;
    assert_eq!(value_b["response"], "child reply", "join answered");
    let value_a = tokio::time::timeout(Duration::from_secs(60), parent_a)
        .await
        .context("original session_prompt never completed")?
        .context("original session_prompt task panicked")?
        .context("original session_prompt failed")?;
    assert_eq!(value_a["response"], "child reply", "original answered");

    let record = fixture.record(session_id).await?.expect("record exists");
    assert!(
        record.revision > created.revision,
        "patch applied once ({} > {})",
        record.revision,
        created.revision
    );
    assert_patch_applied(&record);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Wait until the joining prompt is durably bound and appended, closing the
/// bind-before-append gap before the join is asserted on.
async fn wait_for_durable_join(
    fixture: &PromptFixture,
    session_id: &str,
    call_id: &str,
) -> Result<()> {
    let storage = fixture.storage(session_id);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let binding = fixture
                .toolset
                .session_metadata
                .prompt_admission(&storage, call_id)
                .await?;
            let committed = fixture
                .events(session_id)
                .await?
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Message { id: Some(id), .. } if id == call_id));
            if binding.is_some() && committed {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("joined prompt was not durably appended")??;
    Ok(())
}
