use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_test_common::{spawn_nats_server, NatsServerHandle};
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};
use harnx_toolset::ToolRequest;
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::Arc;

pub(super) struct Fixture {
    pub(super) server: NatsServerHandle,
    pub(super) js: async_nats::jetstream::Context,
    pub(super) toolset: SubagentToolset,
    pub(super) journal: InvocationJournal,
    pub(super) request: ToolRequest,
    pub(super) log: NatsSessionLog,
}

impl Fixture {
    pub(super) async fn new() -> Result<Self> {
        Self::configured("helper", "observe", "child").await
    }

    pub(super) async fn configured(agent: &str, cluster: &str, session_id: &str) -> Result<Self> {
        let server = spawn_nats_server()
            .await?
            .context("nats-server required for observer tests")?;
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let toolset = SubagentToolset::new(
            agent,
            SubagentSessionRoute::new(cluster, crate::SessionActivationRoute::ClusterShared),
            SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
        );
        let request = ToolRequest {
            operation_id: "operation".into(),
            call_id: "original-prompt".into(),
            tool: "session_send".into(),
            args: json!({"message": "original", "session_id": session_id}),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            parent_session_id: Some("trusted-parent".into()),
            parent_agent: None,
            parent_local_session_id: None,
            replay: None,
            tool_call_id: None,
            tool_round: Some(1),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(&js, 1).await?;
        journal
            .record(&request, ("session_send", agent, "server"))
            .await?;
        // Use the same canonical namespacing helper as production, not a guess
        // about the storage encoding of agent and local session IDs.
        let log = NatsSessionLog::new_with_replicas(
            js.clone(),
            harnx_core::session_identity::session_key(Some(agent), session_id),
            1,
        )
        .with_replicas(1);
        Ok(Self {
            server,
            js,
            toolset,
            journal,
            request,
            log,
        })
    }

    pub(super) fn wait_context(&self) -> ToolInvocationContext {
        ToolInvocationContext {
            call_id: "wait-call".into(),
            invoking_session_id: self.request.parent_session_id.clone(),
            ..Default::default()
        }
    }

    pub(super) fn send_context(&self) -> ToolInvocationContext {
        ToolInvocationContext {
            call_id: self.request.call_id.clone(),
            invoking_session_id: self.request.parent_session_id.clone(),
            run_context: self.request.run_context.clone(),
            checkpoint_store: Some(Arc::new(self.journal.checkpoint_store(&self.request))),
            partial_result_store: Some(Arc::new(self.journal.partial_result_store(&self.request))),
            ..Default::default()
        }
    }

    pub(super) async fn dispatch(&self) -> Result<dispatch::DispatchedPrompt> {
        Ok(dispatch::dispatch_send(
            &self.toolset,
            self.request.args.clone(),
            CancellationToken::new(),
            self.send_context(),
        )
        .await?
        .0)
    }

    pub(super) async fn poll(&self, options: Value) -> Result<Value> {
        Ok(wait(
            &self.toolset,
            self.wait_args(options),
            CancellationToken::new(),
            self.wait_context(),
        )
        .await?)
    }

    pub(super) fn wait_args(&self, options: Value) -> Value {
        json!({"session_id": self.request.args["session_id"], "prompt_id": self.request.call_id, "wait": options})
    }

    pub(super) async fn append(&self, entry: SessionLogEntry) -> Result<u64> {
        self.log.append_event_async(&entry).await
    }

    pub(super) async fn activation_count(&self) -> Result<u64> {
        let mut stream = self.js.get_stream("WORK_NOTIFY_observe").await?;
        Ok(stream.info().await?.state.messages)
    }
}

pub(super) fn make_toolset(
    js: &async_nats::jetstream::Context,
    metadata: SessionMetadataStore,
) -> SubagentToolset {
    SubagentToolset::new(
        "helper",
        SubagentSessionRoute::new("observe", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
    )
}

pub(super) fn message(id: &str, role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.into()),
        role,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

pub(super) fn end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 0,
        timestamp: None,
        usage: None,
    }
}

pub(super) async fn block(f: &Fixture) -> Result<()> {
    f.append(SessionLogEntry::ToolCalls {
        text: "waiting for approval".into(),
        thought: None,
        calls: vec![harnx_core::tool::ToolCall {
            id: Some("approval-tool".into()),
            name: "danger".into(),
            arguments: json!({}),
            thought_signature: None,
            reasoning_provenance: None,
        }],
        timestamp: None,
        fence_token: None,
    })
    .await?;
    f.append(SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "approval-tool".into(),
        summary: "approve?".into(),
        fence_token: 0,
    })
    .await?;
    Ok(())
}

pub(super) fn serialized(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

pub(super) fn assert_pending(value: &Value, wait_expired: bool) {
    assert_eq!(value["status"], "pending");
    assert_eq!(value["terminal"], false);
    assert_eq!(value["wait_expired"], wait_expired);
}

pub(super) async fn assert_initial_dispatch_unchanged(f: &Fixture) -> Result<()> {
    assert_eq!(f.activation_count().await?, 1);
    assert_eq!(f.log.load_events_latest_async().await?.len(), 1);
    Ok(())
}
