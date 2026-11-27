//! Broker-backed tests for the `metadata` argument on `session_new`,
//! `session_prompt` and `session_send`: creation provenance, the durable
//! admission gate, first-writer checkpoint guards, and replay that never
//! resets later child edits (plan #2970, T2).
//!
//! Every test name carries `nats` so the `test(/nats/)` filter keeps it in the
//! broker nextest group. The synchronous session_prompt tests live in
//! [`sync`].

// qual:allow(srp, file_length=1200) reason: "cfg(test) module: one store-backed fixture plus the metadata tests that read it. rustqual scores this module root as production code, so the [tests] allowance in rustqual.toml never applies here; splitting the fixture from the tests would hide the seeded setup each one depends on, which is what that allowance exists to avoid."

mod sync;

use super::async_session::dispatch::{self, DispatchedPrompt};
use super::child_metadata::{self, MetadataPlan};
use super::termination::{self, PromptMetadata, PromptParams};
use super::{SubagentNats, SubagentSessionRoute, SubagentToolset};
use crate::nats_session::SessionMetadataClaim;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{
    AdmissionAuthority, ChildPatchOutcome, ChildSessionMetadataPatch, InvocationAdmission,
    InvocationEdgeKind, MetadataRecord, PropertyAssignment, RunLimitsRecord, SessionInitializer,
    SessionMetadata, SessionMetadataStore, SessionPropertiesUpdate, SessionTitlePatch,
};
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use anyhow::{bail, Context, Result};
use harnx_core::agent_config::AgentVariables;
use harnx_core::message::{MessageContent, MessageContentPart, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{CheckpointStore, ToolInvocationContext, ToolInvokeError, ToolRequest};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The patch every test requests: manual title, wholesale variables and one
/// custom property with inheritance.
pub(super) fn patch() -> ChildSessionMetadataPatch {
    let mut variables = AgentVariables::new();
    variables.insert("topic".into(), "rust".into());
    variables.insert("empty".into(), String::new());
    ChildSessionMetadataPatch {
        title: Some(SessionTitlePatch {
            value: Some("  Child plan  ".into()),
            manual: true,
        }),
        variables: Some(variables),
        properties: Some(SessionPropertiesUpdate {
            set: vec![PropertyAssignment {
                name: "customer".into(),
                value: json!("acme"),
                inherit: Some(true),
            }],
            clear: Vec::new(),
            add_labels: Vec::new(),
            remove_labels: Vec::new(),
        }),
    }
}

pub(super) fn patch_json() -> Value {
    json!({
        "title": {"value": "  Child plan  ", "manual": true},
        "variables": {"topic": "rust", "empty": ""},
        "properties": {"set": [{"name": "customer", "value": "acme", "inherit": true}]}
    })
}

/// Assert a record carries the complete requested patch state.
pub(super) fn assert_patch_applied(record: &MetadataRecord) {
    let title = &record.metadata.title;
    assert_eq!(title.value.as_deref(), Some("Child plan"), "trimmed title");
    assert!(title.manual, "manual title flag");
    assert_eq!(
        record.metadata.variables.get("topic").map(String::as_str),
        Some("rust"),
        "stored variables"
    );
    assert_eq!(
        record.metadata.variables.get("empty").map(String::as_str),
        Some(""),
        "wholesale variable replacement keeps empty values"
    );
    let properties = crate::nats_session_metadata::session_properties(&record.metadata)
        .expect("properties decode");
    let customer = properties.get("customer").expect("property set");
    assert_eq!(customer.value, json!("acme"), "property value");
    assert!(customer.inherit, "property inheritance");
    assert!(
        crate::nats_session_metadata::already_holds(&record.metadata, &patch()),
        "record carries the complete normalized patch"
    );
}

fn parent_limits() -> RunLimitsRecord {
    let context = crate::nats_session::test_support::inherited_tool_context();
    serde_json::from_value(context.snapshot).expect("inherited limits record")
}

/// A user row the same shape `dispatch` appends, with the caller's prompt id.
fn user_row(id: &str, content: MessageContent) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.into()),
        role: MessageRole::User,
        content,
        timestamp: None,
        fence_token: None,
    }
}

fn assistant_row(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    }
}

struct Fixture {
    _server: NatsServerHandle,
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
    journal: InvocationJournal,
}

/// The journal identity a fixture journals a tool request under: the tool
/// name, the target agent, and the trusted parent session id.
pub(super) struct ToolJournalIdentity<'a> {
    pub(super) tool: &'a str,
    pub(super) agent: &'a str,
    pub(super) parent: &'a str,
}

/// Journal a tool request exactly as the tool server would receive it.
/// Shared with the `child_metadata_prompt_tests` fixtures so the journaled
/// shape cannot drift between the sync and real-handler paths.
pub(super) async fn journal_tool_request(
    journal: &InvocationJournal,
    call_id: &str,
    identity: ToolJournalIdentity<'_>,
    args: Value,
) -> Result<ToolRequest> {
    let ToolJournalIdentity {
        tool,
        agent,
        parent,
    } = identity;
    let request = ToolRequest {
        user_id: None,
        input_responses: None,
        request_state: None,
        run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
        replay: None,
        operation_id: "operation".into(),
        call_id: call_id.into(),
        tool: tool.into(),
        args,
        parent_session_id: Some(parent.into()),
        parent_agent: None,
        parent_local_session_id: None,
        tool_call_id: Some("transcript-call".into()),
        tool_round: Some(1),
        capabilities: Default::default(),
    };
    journal.record(&request, (tool, agent, "server")).await?;
    Ok(request)
}

/// The invocation context exactly as the tool server rebuilds it from the
/// journal, saved checkpoint included.
pub(super) async fn journaled_invocation_context(
    journal: &InvocationJournal,
    request: &ToolRequest,
) -> Result<ToolInvocationContext> {
    let saved = journal.get(request).await?.context("journal row missing")?;
    Ok(ToolInvocationContext {
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        run_context: request.run_context.clone(),
        checkpoint: saved.checkpoint,
        checkpoint_store: Some(Arc::new(journal.checkpoint_store(request))),
        partial_result_store: Some(Arc::new(journal.partial_result_store(request))),
        ..Default::default()
    })
}

/// Broker connections, metadata store and a live toolset, the base every
/// child-metadata fixture needs. The client bind is hoisted so the setup
/// reads top-down; the journal stays fixture-specific.
pub(super) async fn broker_toolset(
    agent: &str,
    cluster: &str,
) -> Result<(
    NatsServerHandle,
    async_nats::jetstream::Context,
    SubagentToolset,
)> {
    let server = spawn_nats_server()
        .await?
        .context("nats-server required for child-metadata tests")?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&js, 1).await?;
    let toolset = SubagentToolset::new(
        agent,
        SubagentSessionRoute::new(cluster, crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), store, 1),
    );
    Ok((server, js, toolset))
}

async fn fixture() -> Result<Fixture> {
    let (server, js, toolset) = broker_toolset("helper", "dispatch").await?;
    let journal = InvocationJournal::ensure(&js, 1).await?;
    Ok(Fixture {
        _server: server,
        js,
        toolset,
        journal,
    })
}

impl Fixture {
    fn storage(&self, session_id: &str) -> String {
        harnx_core::session_identity::session_key(Some("helper"), session_id)
    }

    async fn record(&self, session_id: &str) -> Result<Option<MetadataRecord>> {
        self.toolset
            .session_metadata
            .get(&self.storage(session_id))
            .await
    }

    async fn events(&self, session_id: &str) -> Result<Vec<(u64, SessionLogEntry)>> {
        NatsSessionLog::new(self.js.clone(), self.storage(session_id))
            .load_events_latest_async()
            .await
    }

    /// Journal a `session_send` request with a fresh call id.
    async fn request(&self, call_id: &str, args: Value) -> Result<ToolRequest> {
        journal_tool_request(
            &self.journal,
            call_id,
            ToolJournalIdentity {
                tool: "session_send",
                agent: "helper",
                parent: "trusted-parent",
            },
            args,
        )
        .await
    }

    /// The invocation context exactly as the tool server rebuilds it from
    /// the journal, saved checkpoint included.
    async fn context(&self, request: &ToolRequest) -> Result<ToolInvocationContext> {
        journaled_invocation_context(&self.journal, request).await
    }

    async fn send(
        &self,
        request: &ToolRequest,
    ) -> Result<(DispatchedPrompt, Option<super::async_session::ResolvedWait>)> {
        dispatch::dispatch_send(
            &self.toolset,
            request.args.clone(),
            CancellationToken::new(),
            self.context(request).await?,
        )
        .await
        .map_err(anyhow::Error::new)
    }

    /// Dispatch expecting a recoverable error; returns its message.
    async fn send_error(&self, request: &ToolRequest) -> Result<String> {
        match self.send(request).await {
            Ok(_) => bail!("dispatch unexpectedly succeeded"),
            Err(error) => match error.downcast_ref::<ToolInvokeError>() {
                Some(ToolInvokeError::Recoverable(message)) => Ok(message.clone()),
                _ => Err(error.context("expected a recoverable tool error")),
            },
        }
    }

    async fn checkpoint(&self, request: &ToolRequest) -> Result<Value> {
        self.journal
            .get(request)
            .await?
            .context("journal row missing")?
            .checkpoint
            .context("checkpoint missing")
    }

    /// Create the child and journal its patch-bearing request: the shared
    /// opening of every guard/scope rejection scenario. Returns the seeded
    /// record and the journaled request.
    async fn seeded_patch_request(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<(MetadataRecord, ToolRequest)> {
        let before = self.seed_session(session_id).await?;
        let request = self
            .request(
                call_id,
                json!({"message": "work", "session_id": session_id, "metadata": patch_json()}),
            )
            .await?;
        Ok((before, request))
    }

    /// Create the child session and return its first canonical record.
    async fn seed_session(&self, session_id: &str) -> Result<MetadataRecord> {
        self.toolset
            .create_session(Some(session_id.into()), None, None)
            .await?;
        self.record(session_id).await?.context("record exists")
    }

    /// `seeded_patch_request` plus the journaled invocation context, exactly
    /// as the tool server rebuilds it: the opening of the review scenarios
    /// that then mutate the context.
    async fn seeded_patch_context(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<(MetadataRecord, ToolRequest, ToolInvocationContext)> {
        let (before, request) = self.seeded_patch_request(session_id, call_id).await?;
        let context = self.context(&request).await?;
        Ok((before, request, context))
    }

    /// A sibling attempt that saved no checkpoint: the journaled context
    /// with its saved-checkpoint view dropped.
    async fn unsaved_context(&self, request: &ToolRequest) -> Result<ToolInvocationContext> {
        let mut context = self.context(request).await?;
        context.checkpoint = None;
        Ok(context)
    }

    /// `prepare_send` through a checkpoint-less sibling attempt.
    async fn unsaved_prepare_send(
        &self,
        request: &ToolRequest,
        session_id: &str,
    ) -> Result<child_metadata::MetadataPlan> {
        let context = self.unsaved_context(request).await?;
        child_metadata::prepare_send(
            &self.toolset,
            &context,
            Some(session_id),
            "trusted-parent",
            patch(),
        )
        .await
        .map_err(anyhow::Error::from)
    }

    /// The error a checkpoint-less sibling attempt's `prepare_send` reports.
    async fn unsaved_prepare_send_error(
        &self,
        request: &ToolRequest,
        session_id: &str,
    ) -> Result<String> {
        match self.unsaved_prepare_send(request, session_id).await {
            Ok(_) => bail!("prepare unexpectedly settled"),
            Err(error) => Ok(error.to_string()),
        }
    }

    /// The applied patch moved the record exactly one revision past `before`.
    async fn assert_applied_once(
        &self,
        session_id: &str,
        before: &MetadataRecord,
    ) -> Result<MetadataRecord> {
        let record = self.record(session_id).await?.expect("record exists");
        assert!(
            record.revision > before.revision,
            "patch applied once ({} > {})",
            record.revision,
            before.revision
        );
        Ok(record)
    }

    /// The deleted child stays deleted across rejected stale proposals.
    async fn assert_stays_deleted(&self, session_id: &str) -> Result<()> {
        assert!(
            self.record(session_id).await?.is_none(),
            "the deleted child stays deleted"
        );
        Ok(())
    }

    /// The Revision pin is still the invocation checkpoint's durable winner.
    async fn assert_revision_pin(&self, request: &ToolRequest, revision: u64) -> Result<()> {
        let checkpoint = self.checkpoint(request).await?;
        assert_eq!(
            checkpoint["metadata_revision"], revision,
            "the Revision pin is the durable first-writer winner"
        );
        assert!(
            checkpoint.get("metadata_creation").is_none(),
            "no creation guard was pinned over the Revision winner"
        );
        Ok(())
    }

    /// Write a pre-T2 dispatch checkpoint without any metadata guard.
    async fn write_legacy_checkpoint(&self, request: &ToolRequest, session_id: &str) -> Result<()> {
        let value = json!({
            "session_id": session_id,
            "storage_key": self.storage(session_id),
            "cluster": "dispatch",
            "prompt_id": request.call_id,
            "parent_storage_key": "trusted-parent",
        });
        self.journal
            .checkpoint_store(request)
            .checkpoint(value)
            .await?;
        Ok(())
    }

    /// A child edit that lands after the parent's patch: only the title moves.
    async fn child_edit_title(&self, session_id: &str, title: &str) -> Result<()> {
        let patch = ChildSessionMetadataPatch {
            title: Some(SessionTitlePatch {
                value: Some(title.into()),
                manual: true,
            }),
            variables: None,
            properties: None,
        };
        let storage = self.storage(session_id);
        let store = &self.toolset.session_metadata;
        match store.apply_child_patch(&storage, &patch, None).await? {
            ChildPatchOutcome::Applied(_) | ChildPatchOutcome::AlreadyHeld(_) => Ok(()),
            ChildPatchOutcome::Conflict { .. } => bail!("child edit conflicted with itself"),
        }
    }

    /// Apply the parent patch at `revision` directly, the state a crashed
    /// attempt leaves between the CAS and admission.
    async fn apply_patch_at(&self, session_id: &str, revision: u64) -> Result<MetadataRecord> {
        let storage = self.storage(session_id);
        let store = &self.toolset.session_metadata;
        match store
            .apply_child_patch(&storage, &patch(), Some(revision))
            .await?
        {
            ChildPatchOutcome::Applied(record) => Ok(record),
            ChildPatchOutcome::AlreadyHeld(record) => Ok(record),
            ChildPatchOutcome::Conflict { current, .. } => {
                bail!("patch conflicted at revision {revision}: {current:?}")
            }
        }
    }

    /// Bind the prompt admission and commit the original turn's rows: the
    /// durable replay proof the metadata gate must honor without re-fetching
    /// a gone attachment.
    async fn commit_replayed_turn(
        &self,
        session_id: &str,
        call_id: &str,
        original: MessageContent,
    ) -> Result<()> {
        let storage = self.storage(session_id);
        self.toolset
            .session_metadata
            .bind_prompt_admission(&storage, call_id, call_id)
            .await?;
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), storage, 1);
        log.append_event_async(&user_row(call_id, original)).await?;
        log.append_event_async(&assistant_row("original reply"))
            .await?;
        log.append_event_async(&turn_end(1)).await?;
        Ok(())
    }

    /// The original patch landed, then the child edited the title; the retry
    /// carries the same call id, the gone attachment and the same patch.
    /// Returns the edited record and the retry arguments.
    async fn edited_child_retry_args(
        &self,
        session_id: &str,
        gone_attachment: &str,
    ) -> Result<(MetadataRecord, Value)> {
        self.apply_patch_at(session_id, 1).await?;
        self.child_edit_title(session_id, "child edit").await?;
        let edited = self.record(session_id).await?.expect("record exists");
        let args = json!({
            "message": "original prompt",
            "session_id": session_id,
            "attachments": [gone_attachment],
            "metadata": patch_json(),
            "__harnx_parent_session_id": "parent-x",
        });
        Ok((edited, args))
    }

    /// Journal a patch-bearing send against `session_id` and run the
    /// metadata gate by hand: the production dispatch path runs the same
    /// steps internally, so a test can pin the durable first-writer state a
    /// crashed attempt leaves behind.
    async fn prepare_patch_send(
        &self,
        call_id: &str,
        session_id: &str,
    ) -> Result<(ToolRequest, MetadataPlan)> {
        let request = self
            .request(
                call_id,
                json!({"message": "hi", "session_id": session_id, "metadata": patch_json()}),
            )
            .await?;
        let context = self.context(&request).await?;
        let plan = child_metadata::prepare_send(
            &self.toolset,
            &context,
            Some(session_id),
            "trusted-parent",
            patch(),
        )
        .await?;
        Ok((request, plan))
    }

    /// Replay an admitted request after the child edited the title: the
    /// retry returns the same durable handle and never re-applies the patch
    /// over the child's newer state.
    async fn assert_replay_keeps_child_edit(
        &self,
        request: &ToolRequest,
        session_id: &str,
        expected: &DispatchedPrompt,
        expected_events: usize,
    ) -> Result<()> {
        self.child_edit_title(session_id, "child edit").await?;
        let edited = self.record(session_id).await?.expect("record exists");
        let (replay, _) = self.send(request).await?;
        assert_eq!(replay.handle, expected.handle, "same durable handle");
        let record = self.record(session_id).await?.expect("record exists");
        assert_eq!(
            record.revision, edited.revision,
            "no re-application on replay"
        );
        assert_eq!(
            record.metadata.title.value.as_deref(),
            Some("child edit"),
            "later child edit survives the replay"
        );
        assert_eq!(
            self.events(session_id).await?.len(),
            expected_events,
            "no new append"
        );
        Ok(())
    }

    /// Run the retry through the real synchronous handler: it returns the
    /// original response, and never resets the child's later edit or
    /// appends again.
    async fn assert_sync_replay_never_resets(
        &self,
        args: Value,
        call_id: &str,
        session_id: &str,
        edited_revision: u64,
    ) -> Result<()> {
        let context = ToolInvocationContext {
            call_id: call_id.into(),
            invoking_session_id: Some("parent-x".into()),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            ..Default::default()
        };
        let value = self
            .toolset
            .session_prompt(args, CancellationToken::new(), context)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(value["response"], "original reply", "durable response");
        assert_eq!(value["session_id"], session_id);
        let record = self.record(session_id).await?.expect("record exists");
        assert_eq!(record.revision, edited_revision, "replay wrote nothing");
        assert_eq!(
            record.metadata.title.value.as_deref(),
            Some("child edit"),
            "later child edit survives the sync replay"
        );
        assert_eq!(self.events(session_id).await?.len(), 3, "no new append");
        Ok(())
    }

    /// Seed an admission for `call_id`, winning the head on an idle session
    /// and leaving a candidate write on a busy one. Returns whether it won.
    async fn seed_admission(
        &self,
        session_id: &str,
        call_id: &str,
        content: MessageContent,
    ) -> Result<bool> {
        let storage = self.storage(session_id);
        let authority = AdmissionAuthority::Inherited {
            parent: parent_limits(),
            edge: InvocationEdgeKind::Delegation,
            detached: false,
            admitted_at: chrono::Utc::now(),
        };
        let mut intent = InvocationAdmission::new(&authority, call_id.into(), None, None);
        intent.prompt_content = Some(content);
        intent.parent_storage_key = Some("trusted-parent".into());
        let entries = self.events(session_id).await?;
        let store = &self.toolset.session_metadata;
        let won = store
            .reserve_admission(&storage, &intent, &entries)
            .await
            .is_ok();
        if !won {
            // A refused reservation still persisted the candidate intent.
            let candidate = store
                .admission(&storage, call_id)
                .await?
                .context("candidate admission persisted")?;
            assert!(
                candidate.prompt_content.is_some(),
                "candidate carries prompt content"
            );
        }
        Ok(won)
    }
}

/// A generated-ID `session_send` create carries the patch in the first
/// canonical record, stamped with the reservation's `creation_invocation`.
#[tokio::test]
async fn nats_send_metadata_creates_generated_session_with_first_record_state() -> Result<()> {
    let fixture = fixture().await?;
    let request = fixture
        .request(
            "gen-call",
            json!({"message": "work", "metadata": patch_json()}),
        )
        .await?;
    let (dispatched, _) = fixture.send(&request).await?;
    let session_id = &dispatched.handle.session_id;

    let record = fixture.record(session_id).await?.expect("record exists");
    assert_eq!(record.revision, 1, "first record");
    assert_patch_applied(&record);
    assert_eq!(
        record.metadata.creation_invocation.as_deref(),
        Some("gen-call"),
        "reservation stamps the trusted creation identity"
    );
    // A generated create guards through the reservation stamp, not a
    // checkpoint guard field.
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["session_id"], *session_id);
    assert!(checkpoint.get("metadata_creation").is_none());
    assert!(checkpoint.get("metadata_revision").is_none());
    Ok(())
}

/// An explicit-ID `session_send` create pins a creation guard in the
/// invocation checkpoint, creates the record with the patch, and stamps the
/// invocation as its creator.
#[tokio::test]
async fn nats_send_metadata_creates_explicit_session_with_guard_and_provenance() -> Result<()> {
    let fixture = fixture().await?;
    let request = fixture
        .request(
            "explicit-call",
            json!({"message": "work", "session_id": "explicit-meta", "metadata": patch_json()}),
        )
        .await?;
    let (dispatched, _) = fixture.send(&request).await?;
    assert_eq!(dispatched.handle.session_id, "explicit-meta");

    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["metadata_creation"], true,
        "creation guard pinned before the create"
    );
    let record = fixture
        .record("explicit-meta")
        .await?
        .expect("record exists");
    assert_patch_applied(&record);
    assert_eq!(
        record.metadata.creation_invocation.as_deref(),
        Some("explicit-call"),
        "explicit create stamps its own provenance"
    );
    assert_eq!(
        fixture.events("explicit-meta").await?.len(),
        1,
        "one prompt"
    );
    Ok(())
}

/// An existing-session `session_send` update applies title, variables and
/// properties together in one revision-guarded CAS, and pins the observed
/// revision in its checkpoint first.
#[tokio::test]
async fn nats_send_metadata_updates_existing_session_in_one_cas() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({"message": "hi", "session_id": "cas-meta"}),
        )
        .await?;
    fixture.send(&seed).await?;
    let created = fixture.record("cas-meta").await?.expect("record exists");

    let request = fixture
        .request(
            "patch-call",
            json!({"message": "hi", "session_id": "cas-meta", "metadata": patch_json()}),
        )
        .await?;
    fixture.send(&request).await?;

    let record = fixture.record("cas-meta").await?.expect("record exists");
    assert!(
        record.revision > created.revision,
        "one CAS, exactly one new revision"
    );
    assert_patch_applied(&record);
    assert_eq!(
        record.metadata.creation_invocation.as_deref(),
        created.metadata.creation_invocation.as_deref(),
        "provenance untouched"
    );
    assert_eq!(
        record.metadata.created_at, created.metadata.created_at,
        "unrelated fields preserved"
    );
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["metadata_revision"], created.revision,
        "guard pinned the pre-patch revision"
    );
    assert!(checkpoint.get("metadata_creation").is_none());
    Ok(())
}

/// A patch-bearing send to a busy session joins the running turn, applies the
/// patch once, and a replay of the joined call never resets later child
/// edits.
#[tokio::test]
async fn nats_send_metadata_to_busy_session_joins_and_replay_never_resets() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "busy-call",
            json!({"message": "work", "session_id": "busy-meta"}),
        )
        .await?;
    let (seeded, _) = fixture.send(&seed).await?;
    assert_eq!(seeded.handle.prompt_id, "busy-call");

    let request = fixture
        .request(
            "join-call",
            json!({"message": "joined", "session_id": "busy-meta", "metadata": patch_json()}),
        )
        .await?;
    let seeded = fixture.record("busy-meta").await?.expect("record exists");
    let (joined, _) = fixture.send(&request).await?;
    assert_eq!(joined.handle.prompt_id, "join-call", "joined the busy turn");
    let record = fixture.record("busy-meta").await?.expect("record exists");
    assert!(
        record.revision > seeded.revision,
        "patch applied once ({} > {})",
        record.revision,
        seeded.revision
    );
    assert_patch_applied(&record);

    // The child edits after the joined dispatch landed; replaying the
    // admitted joined call must never reset that edit.
    fixture
        .assert_replay_keeps_child_edit(&request, "busy-meta", &joined, 2)
        .await?;
    Ok(())
}

/// A crash between the patch CAS and admission resumes safely: the guard
/// checkpoint proves the revision, the record already holds the patch, and
/// the retry proceeds without overwriting anything.
#[tokio::test]
async fn nats_send_metadata_retry_resumes_after_cas_without_overwriting() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({"message": "hi", "session_id": "resume-meta"}),
        )
        .await?;
    fixture.send(&seed).await?;
    let created = fixture.record("resume-meta").await?.expect("record exists");

    // The first attempt crashes after the CAS and before admission: the
    // durable state is the pinned guard plus the applied patch.
    let (request, plan) = fixture
        .prepare_patch_send("patch-call", "resume-meta")
        .await?;
    assert!(matches!(plan, MetadataPlan::Settled), "patch applied");
    let mid = fixture.record("resume-meta").await?.expect("record exists");
    assert!(
        mid.revision > created.revision,
        "the crashed attempt applied its patch ({} > {})",
        mid.revision,
        created.revision
    );

    // The retry replays the same call: AlreadyHeld proceeds without writing.
    let (dispatched, _) = fixture.send(&request).await?;
    assert_eq!(dispatched.handle.prompt_id, "patch-call");
    let record = fixture.record("resume-meta").await?.expect("record exists");
    assert_eq!(record.revision, mid.revision, "retry did not write again");
    assert_patch_applied(&record);
    assert_eq!(
        fixture.events("resume-meta").await?.len(),
        2,
        "one new append"
    );
    Ok(())
}

/// A candidate-only admission is never admission proof: when the record moved
/// past the pinned revision without carrying the patch, the retry fails with
/// a recoverable conflict and appends nothing.
#[tokio::test]
async fn nats_candidate_only_admission_conflicts_without_append() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({"message": "hi", "session_id": "cand-meta"}),
        )
        .await?;
    fixture.send(&seed).await?;

    let (request, _) = fixture
        .prepare_patch_send("patch-call", "cand-meta")
        .await?;
    // Apply the patch through the real gate, then crash before admission.
    // The child overwrites the patch with a newer edit.
    fixture.child_edit_title("cand-meta", "child edit").await?;
    let edited = fixture.record("cand-meta").await?.expect("record exists");

    // A refused reservation leaves only a candidate admission for the call.
    let won = fixture
        .seed_admission("cand-meta", "patch-call", MessageContent::Text("hi".into()))
        .await?;
    assert!(!won, "busy session refuses the reservation");

    let message = fixture.send_error(&request).await?;
    assert!(
        message.contains("child session metadata conflict"),
        "recoverable conflict, got: {message}"
    );
    let record = fixture.record("cand-meta").await?.expect("record exists");
    assert_eq!(record.revision, edited.revision, "no write on conflict");
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("child edit"),
        "child edit kept"
    );
    assert_eq!(
        fixture.events("cand-meta").await?.len(),
        1,
        "nothing appended"
    );
    Ok(())
}

/// A lost explicit-create acknowledgement replays after a child edit: the
/// creation record proves the call's own provenance, the original prompt
/// replays, and the child's newer state is never reset.
#[tokio::test]
async fn nats_lost_explicit_create_ack_replays_after_child_edit_without_reset() -> Result<()> {
    let fixture = fixture().await?;
    let request = fixture
        .request(
            "recover-call",
            json!({"message": "hi", "session_id": "recover-meta", "metadata": patch_json()}),
        )
        .await?;
    let (first, _) = fixture.send(&request).await?;
    assert_eq!(first.handle.session_id, "recover-meta");

    // The acknowledgement is lost; the retry replays the original dispatch
    // without resetting the child's newer state.
    fixture
        .assert_replay_keeps_child_edit(&request, "recover-meta", &first, 1)
        .await?;
    Ok(())
}

/// A competing explicit creator that wins after the creation guard was pinned
/// is a recoverable conflict: the record is not mutated and nothing
/// dispatches.
#[tokio::test]
async fn nats_competing_explicit_creator_conflicts_without_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let (request, plan) = fixture
        .prepare_patch_send("compete-call", "compete-meta")
        .await?;
    // The first attempt pins the creation guard, then crashes before the
    // create.
    assert!(matches!(plan, MetadataPlan::Create(_)), "create planned");
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["metadata_creation"], true);

    // A competing creator wins the explicit id in the meantime.
    let initializer = SessionInitializer::named("helper", AgentVariables::new());
    let competitor = SessionMetadata::new("compete-meta", initializer);
    fixture
        .toolset
        .session_metadata
        .create(&competitor)
        .await
        .context("competing create")?;

    let message = fixture.send_error(&request).await?;
    assert!(
        message.contains("competing invocation"),
        "recoverable competing-creator conflict, got: {message}"
    );
    let record = fixture
        .record("compete-meta")
        .await?
        .expect("record exists");
    assert_eq!(record.revision, 1, "record untouched");
    assert!(
        record.metadata.title.value.is_none(),
        "competing record not mutated"
    );
    assert_eq!(
        fixture.events("compete-meta").await?.len(),
        0,
        "no dispatch"
    );
    Ok(())
}

/// A pre-T2 checkpoint without a metadata guard is never retrofitted: a
/// patch-bearing recovery against an existing session fails safely.
#[tokio::test]
async fn nats_legacy_checkpoint_without_guard_fails_safely() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({"message": "hi", "session_id": "legacy-meta"}),
        )
        .await?;
    fixture.send(&seed).await?;

    let request = fixture
        .request(
            "patch-call",
            json!({"message": "hi", "session_id": "legacy-meta", "metadata": patch_json()}),
        )
        .await?;
    fixture
        .write_legacy_checkpoint(&request, "legacy-meta")
        .await?;

    let message = fixture.send_error(&request).await?;
    assert!(
        message.contains("refusing to retrofit"),
        "legacy checkpoint rejected, got: {message}"
    );
    let record = fixture.record("legacy-meta").await?.expect("record exists");
    assert_eq!(record.revision, 1, "no mutation");
    assert!(record.metadata.title.value.is_none());
    assert_eq!(
        fixture.events("legacy-meta").await?.len(),
        1,
        "nothing appended"
    );
    Ok(())
}

/// The creation claim check: our own creation and a fresh create proceed, a
/// foreign creator is a recoverable conflict.
#[test]
fn verify_creation_claim_accepts_own_creation_and_rejects_competing() {
    let base = SessionInitializer::named("helper", AgentVariables::new());
    let mut own = SessionMetadata::new("s", base.clone());
    own.creation_invocation = Some("call".into());
    let foreign = SessionMetadata::new("s", base);

    assert!(
        child_metadata::verify_creation_claim(
            &SessionMetadataClaim::Created(MetadataRecord {
                metadata: own.clone(),
                revision: 1
            }),
            "call",
            "s"
        )
        .is_ok(),
        "fresh create proceeds"
    );
    assert!(
        child_metadata::verify_creation_claim(
            &SessionMetadataClaim::Existing(MetadataRecord {
                metadata: own,
                revision: 2
            }),
            "call",
            "s"
        )
        .is_ok(),
        "own lost-ack creation proceeds"
    );
    let error = child_metadata::verify_creation_claim(
        &SessionMetadataClaim::Existing(MetadataRecord {
            metadata: foreign,
            revision: 1,
        }),
        "call",
        "s",
    )
    .expect_err("competing creator must fail");
    assert!(
        error.to_string().contains("competing invocation"),
        "got: {error}"
    );
}

/// A fresh patch-bearing `session_send` validates its attachments BEFORE the
/// metadata gate: a missing attachment fails without a guard checkpoint,
/// without mutating an existing record, and without creating an explicit
/// session.
#[tokio::test]
async fn nats_send_missing_attachment_fails_before_metadata_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let seed = fixture
        .request(
            "seed-call",
            json!({"message": "hi", "session_id": "missing-attach-meta"}),
        )
        .await?;
    fixture.send(&seed).await?;
    let before = fixture
        .record("missing-attach-meta")
        .await?
        .expect("record exists");

    let gone =
        "cid:media:parent/source/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let request = fixture
        .request(
            "patch-call",
            json!({
                "message": "hi",
                "session_id": "missing-attach-meta",
                "attachments": [gone],
                "metadata": patch_json(),
            }),
        )
        .await?;
    let message = fixture.send_error(&request).await?;
    assert!(
        message.contains("attachment not found"),
        "missing attachment, got: {message}"
    );
    let after = fixture
        .record("missing-attach-meta")
        .await?
        .expect("record exists");
    assert_eq!(after.revision, before.revision, "record untouched");
    assert_eq!(after.metadata, before.metadata, "no mutation");
    assert_eq!(
        fixture.events("missing-attach-meta").await?.len(),
        1,
        "nothing appended"
    );
    let row = fixture
        .journal
        .get(&request)
        .await?
        .context("journal row missing")?;
    assert!(row.checkpoint.is_none(), "no guard checkpoint written");

    // An explicit-ID create with a missing attachment creates nothing.
    let create = fixture
        .request(
            "create-call",
            json!({
                "message": "hi",
                "session_id": "missing-attach-create",
                "attachments": [gone],
                "metadata": patch_json(),
            }),
        )
        .await?;
    let message = fixture.send_error(&create).await?;
    assert!(
        message.contains("attachment not found"),
        "missing attachment, got: {message}"
    );
    assert!(
        fixture.record("missing-attach-create").await?.is_none(),
        "no record created"
    );
    let row = fixture
        .journal
        .get(&create)
        .await?
        .context("journal row missing")?;
    assert!(row.checkpoint.is_none(), "no creation guard written");
    Ok(())
}

/// A patch-bearing send replays against a saved dispatch checkpoint whose
/// scope no longer matches this worker's route (a same-broker cluster-label
/// change): the call fails BEFORE the metadata gate, leaving the canonical
/// record, the pinned guard and the child log untouched (review F03). The
/// revision guard alone stays valid, so without the saved-scope check this
/// call would CAS and only then be rejected by `checkpointed_session`.
#[tokio::test]
async fn nats_send_saved_checkpoint_scope_mismatch_fails_before_metadata_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let (before, request) = fixture
        .seeded_patch_request("scope-meta", "scope-call")
        .await?;
    // The invocation's checkpoint, pinned under its original route before the
    // cluster label changed: only the scope fields are stale.
    fixture
        .journal
        .checkpoint_store(&request)
        .checkpoint(json!({
            "session_id": "scope-meta",
            "storage_key": fixture.storage("scope-meta"),
            "cluster": "elsewhere",
            "prompt_id": "scope-call",
            "parent_storage_key": "trusted-parent",
            "metadata_revision": before.revision,
        }))
        .await?;

    let message = match fixture.send(&request).await {
        Ok(_) => bail!("stale saved checkpoint scope must fail"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("sub-agent dispatch checkpoint scope mismatch"),
        "got: {message}"
    );

    let record = fixture.record("scope-meta").await?.expect("record exists");
    assert_eq!(record.revision, before.revision, "no CAS on stale scope");
    assert_eq!(record.metadata, before.metadata, "no mutation");
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["cluster"], "elsewhere",
        "the failed call did not rewrite the first writer"
    );
    assert_eq!(
        checkpoint["metadata_revision"], before.revision,
        "pinned guard untouched"
    );
    assert!(
        checkpoint.get("metadata_creation").is_none(),
        "no creation guard"
    );
    assert_eq!(
        fixture.events("scope-meta").await?.len(),
        0,
        "nothing appended"
    );
    Ok(())
}

/// A fresh guard pin that meets a first-writer checkpoint from a different
/// route fails before the CAS: a winner naming the same session under a stale
/// scope is not a valid guard here (review F03, first-writer winner check).
/// The caller's saved-checkpoint view is absent, so only the winner check can
/// catch the stale scope.
#[tokio::test]
async fn nats_send_first_writer_from_stale_route_fails_before_metadata_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let (before, request) = fixture.seeded_patch_request("win-meta", "win-call").await?;
    // A first writer pinned this invocation's checkpoint under a different
    // cluster label (route change between attempts), without a guard yet.
    fixture
        .journal
        .checkpoint_store(&request)
        .checkpoint(json!({
            "session_id": "win-meta",
            "storage_key": fixture.storage("win-meta"),
            "cluster": "elsewhere",
            "prompt_id": "win-call",
            "parent_storage_key": "trusted-parent",
        }))
        .await?;
    let context = ToolInvocationContext {
        call_id: "win-call".into(),
        invoking_session_id: Some("trusted-parent".into()),
        run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
        checkpoint: None,
        checkpoint_store: Some(Arc::new(fixture.journal.checkpoint_store(&request))),
        partial_result_store: Some(Arc::new(fixture.journal.partial_result_store(&request))),
        ..Default::default()
    };

    let error = child_metadata::prepare_send(
        &fixture.toolset,
        &context,
        Some("win-meta"),
        "trusted-parent",
        patch(),
    )
    .await
    .map(|_| ())
    .expect_err("stale first-writer scope must fail");
    let message = error.to_string();
    assert!(
        message.contains("sub-agent metadata checkpoint scope mismatch"),
        "got: {message}"
    );

    let record = fixture.record("win-meta").await?.expect("record exists");
    assert_eq!(record.revision, before.revision, "no CAS on stale winner");
    assert_eq!(record.metadata, before.metadata, "no mutation");
    // The refused pin leaves the first writer untouched: no guard was added.
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["cluster"], "elsewhere", "winner untouched");
    assert!(
        checkpoint.get("metadata_revision").is_none(),
        "no revision guard pinned"
    );
    assert!(
        checkpoint.get("metadata_creation").is_none(),
        "no creation guard pinned"
    );
    assert_eq!(
        fixture.events("win-meta").await?.len(),
        0,
        "nothing appended"
    );
    Ok(())
}

/// The F04 deletion interleave: sibling attempts of one immutable explicit-ID
/// request observe the child differently. The sibling that saw the existing
/// record wins the invocation checkpoint with a Revision pin; authorized
/// deletion then removes the child's record and stream while the parent's
/// journal row and Revision pin survive; the sibling still holding the stale
/// missing-record observation must not recreate the deleted child or
/// dispatch against the Revision winner.
#[tokio::test]
async fn nats_stale_creation_proposal_cannot_recreate_deleted_child() -> Result<()> {
    let fixture = fixture().await?;
    let (created, request) = fixture
        .seeded_patch_request("race-meta", "race-call")
        .await?;

    // Sibling B observed the existing record and, as the invocation's first
    // writer, pinned Revision(rev) — its saved-checkpoint view predates the
    // pin — then applied the patch.
    let plan = fixture.unsaved_prepare_send(&request, "race-meta").await?;
    assert!(matches!(plan, MetadataPlan::Settled), "sibling B applied");
    fixture.assert_applied_once("race-meta", &created).await?;
    fixture
        .assert_revision_pin(&request, created.revision)
        .await?;

    // Authorized deletion removes the child's canonical record and stream.
    crate::nats_admin::delete_session_state(
        &fixture.js,
        None,
        &fixture.storage("race-meta"),
        crate::nats_admin::StreamDeletion::First,
    )
    .await?;
    fixture.assert_stays_deleted("race-meta").await?;
    // The parent invocation's journal row and Revision pin survive deletion.
    fixture
        .assert_revision_pin(&request, created.revision)
        .await?;

    // Sibling A paused before pinning, still holding its stale
    // missing-record observation (its saved-checkpoint view predates B's
    // pin). Its Creation proposal must be rejected against the Revision
    // winner — never rebased into a create.
    let a_context = fixture.unsaved_context(&request).await?;
    let message = fixture
        .unsaved_prepare_send_error(&request, "race-meta")
        .await?;
    assert!(
        message.contains("sub-agent metadata checkpoint guard disagrees with the pinned kind"),
        "got: {message}"
    );
    fixture.assert_stays_deleted("race-meta").await?;
    fixture
        .assert_revision_pin(&request, created.revision)
        .await?;

    // The same stale view through the real dispatch entry point also
    // recreates nothing and dispatches nothing.
    let message = match dispatch::dispatch_send(
        &fixture.toolset,
        request.args.clone(),
        CancellationToken::new(),
        a_context,
    )
    .await
    {
        Ok(_) => bail!("stale Creation dispatch must be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("sub-agent metadata checkpoint guard disagrees with the pinned kind"),
        "got: {message}"
    );
    fixture.assert_stays_deleted("race-meta").await?;

    // An ordinary retry that reads the journal (and so B's Revision pin)
    // fails safely too: the pinned revision names a deleted record.
    let message = match fixture.send(&request).await {
        Ok(_) => bail!("retry against the deleted record must fail"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("no longer has a metadata record"),
        "recoverable guard-without-record, got: {message}"
    );
    fixture.assert_stays_deleted("race-meta").await?;
    Ok(())
}

/// Same-kind convergence: a Revision proposal that observed a different
/// revision than the stored first-writer pin honors the stored pin, and the
/// resulting conflict surfaces as the documented recoverable metadata
/// conflict — never as a guard-kind rejection (review F04).
#[tokio::test]
async fn nats_same_kind_revision_pins_converge_on_stored_revision() -> Result<()> {
    let fixture = fixture().await?;
    let created = fixture.seed_session("conv-meta").await?;

    // The first attempt pins Revision(1) and applies the patch.
    let (request, plan) = fixture.prepare_patch_send("conv-call", "conv-meta").await?;
    assert!(matches!(plan, MetadataPlan::Settled), "patch applied once");
    fixture.assert_applied_once("conv-meta", &created).await?;
    fixture
        .assert_revision_pin(&request, created.revision)
        .await?;

    // The child edits; a later attempt of the same call observes the newer
    // revision and proposes a different Revision number. The stored
    // Revision(1) pin wins — same kind, different number — and the CAS
    // conflict is the documented recoverable conflict, not a rejection.
    fixture.child_edit_title("conv-meta", "child edit").await?;
    let edited = fixture.record("conv-meta").await?.expect("record exists");
    let message = fixture
        .unsaved_prepare_send_error(&request, "conv-meta")
        .await?;
    assert!(
        message.contains("child session metadata conflict"),
        "same-kind convergence kept, got: {message}"
    );
    let record = fixture.record("conv-meta").await?.expect("record exists");
    assert_eq!(record.revision, edited.revision, "no write on the conflict");
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("child edit"),
        "later child edit survives"
    );
    Ok(())
}
