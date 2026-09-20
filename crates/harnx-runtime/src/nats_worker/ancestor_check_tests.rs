//! A child may only resume while its parent's tool call is still open.
use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{
    ParentLink, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use anyhow::Result;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::ToolOutput;
use harnx_core::tool::ToolCall;

/// The id the PARENT's transcript gave the delegating call. The link is keyed
/// by this one, because it is the only id the parent's log carries.
const CALL: &str = "call-1";
/// The id dispatch minted for the same call on the wire, which is what the
/// invocation marker records. Deliberately different: a link built from it
/// would match nothing in the parent's transcript.
const WIRE_CALL: &str = "wire-call-1";
const PARENT_AGENT: &str = "parent-agent";
const CHILD_AGENT: &str = "child-agent";

struct Fixture {
    js: async_nats::jetstream::Context,
    metadata: SessionMetadataStore,
    parent_key: String,
    child_key: String,
    _server: crate::nats_test_common::NatsServerHandle,
}

impl Fixture {
    /// Parent and child metadata plus a child log with one pending user
    /// message; each test then writes the parent log it wants to check against.
    async fn start(case: &str) -> Result<Option<Self>> {
        let Some(server) = crate::nats_test_common::spawn_nats_server().await? else {
            return Ok(None);
        };
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let parent_id = format!("{case}-parent");
        let child_id = format!("{case}-child");
        let parent_key = harnx_core::session_identity::session_key(Some(PARENT_AGENT), &parent_id);
        let child_key = harnx_core::session_identity::session_key(Some(CHILD_AGENT), &child_id);

        metadata
            .create(&SessionMetadata::new(
                &parent_id,
                SessionInitializer::named(PARENT_AGENT, Default::default()),
            ))
            .await?;
        let mut child_initializer = SessionInitializer::named(CHILD_AGENT, Default::default());
        child_initializer.parent = Some(ParentLink {
            session_id: parent_key.clone(),
            tool_call_id: CALL.into(),
        });
        metadata
            .create(&SessionMetadata::new(&child_id, child_initializer))
            .await?;
        append(&js, &child_key, &user("do the thing")).await?;

        Ok(Some(Self {
            js,
            metadata,
            parent_key,
            child_key,
            _server: server,
        }))
    }

    async fn verdict(&self) -> Result<AncestorVerdict> {
        check_ancestors(&self.js, &self.metadata, &self.child_key).await
    }

    /// The parent transcript up to and including the sub-agent's tool call.
    async fn seed_open_call(&self) -> Result<()> {
        append(&self.js, &self.parent_key, &user("delegate")).await?;
        append(
            &self.js,
            &self.parent_key,
            &SessionLogEntry::ToolCalls {
                text: "delegating".into(),
                thought: None,
                calls: vec![ToolCall::new(
                    "subagent_session_prompt".into(),
                    serde_json::json!({}),
                    Some(CALL.into()),
                    None,
                )],
                timestamp: None,
                fence_token: None,
            },
        )
        .await?;
        append(
            &self.js,
            &self.parent_key,
            &SessionLogEntry::SubAgentStarted {
                agent: CHILD_AGENT.into(),
                session_id: self.child_key.clone(),
                invocation_id: Some(WIRE_CALL.into()),
                tool_call_id: Some(CALL.into()),
                started_at: None,
            },
        )
        .await
    }
}

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

async fn append(
    js: &async_nats::jetstream::Context,
    session: &str,
    entry: &SessionLogEntry,
) -> Result<()> {
    NatsSessionLog::new_with_replicas(js.clone(), session.to_string(), 1)
        .append_event_async(entry)
        .await
        .map(drop)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_parent_turn_forbids_resuming_its_child() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = Fixture::start("interrupted").await? else {
        return Ok(());
    };
    fixture.seed_open_call().await?;
    append(
        &fixture.js,
        &fixture.parent_key,
        &SessionLogEntry::Cancel {
            fence_token: 1,
            cancellation_id: Some("cancel-7".into()),
            requested_by: Some("client:test".into()),
            timestamp: None,
        },
    )
    .await?;

    assert_eq!(
        fixture.verdict().await?,
        AncestorVerdict::Interrupted {
            parent_session: fixture.parent_key.clone(),
            cancellation_id: Some("cancel-7".into()),
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_holding_the_calls_result_forbids_resuming_its_child() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = Fixture::start("answered").await? else {
        return Ok(());
    };
    fixture.seed_open_call().await?;
    append(
        &fixture.js,
        &fixture.parent_key,
        &SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some(CALL.into()),
                name: "subagent_session_prompt".into(),
                output: serde_json::json!({"text": "already answered"}),
                markdown: None,
                content: Vec::new(),
                switch_agent: None,
            }],
            timestamp: None,
        },
    )
    .await?;

    assert_eq!(
        fixture.verdict().await?,
        AncestorVerdict::Interrupted {
            parent_session: fixture.parent_key.clone(),
            cancellation_id: None,
        }
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_still_waiting_on_the_call_lets_its_child_resume() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = Fixture::start("running").await? else {
        return Ok(());
    };
    fixture.seed_open_call().await?;

    assert_eq!(fixture.verdict().await?, AncestorVerdict::Clear);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_parent_log_is_an_error_not_a_clear_verdict() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = Fixture::start("unreadable").await? else {
        return Ok(());
    };
    // A nonstandard API prefix has no JetStream responder. The resulting
    // request error must propagate rather than being treated as an absent log.
    let unreadable = async_nats::jetstream::with_prefix(
        fixture.js.client().clone(),
        "UNREACHABLE_JETSTREAM_API",
    );
    let error = check_ancestors(&unreadable, &fixture.metadata, &fixture.child_key)
        .await
        .expect_err("unreadable parent log");
    assert!(
        format!("{error:#}").contains("parent log unreadable"),
        "expected the unreadable-parent context, got {error:#}"
    );
    Ok(())
}
