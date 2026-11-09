//! Task 5 wiring tests: the registered `session_send`/`session_wait` tools
//! route through the `Toolset` impl into the Task 2 dispatch and Task 3
//! observation entry points, with the transport-attested context the
//! toolset-server supplies. Real NATS fixtures prove the trusted-scope
//! contract: dispatch and observation read caller identity from the context,
//! never from arguments, and fail closed without it.

use super::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::SessionMetadataStore;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use harnx_toolset::{ToolInvokeError, ToolRequest, Toolset};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const SESSION: &str = "wire-child";
const PARENT: &str = "trusted-parent";

struct WiringFixture {
    /// Owns the nats-server child; dropping it kills the broker, so it stays
    /// bound for the whole test.
    _server: crate::nats_test_common::NatsServerHandle,
    js: async_nats::jetstream::Context,
    toolset: SubagentToolset,
}

impl WiringFixture {
    async fn new() -> anyhow::Result<Self> {
        let Some(server) = spawn_nats_server().await? else {
            anyhow::bail!("nats-server required for session wiring tests")
        };
        let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
        let metadata = SessionMetadataStore::ensure(&js, 1).await?;
        let toolset = SubagentToolset::new(
            "helper",
            SubagentSessionRoute::new("observe", crate::SessionActivationRoute::ClusterShared),
            SubagentNats::new(js.client().clone(), js.clone(), metadata, 1),
        );
        Ok(Self {
            _server: server,
            js,
            toolset,
        })
    }

    /// The `ToolInvocationContext` the toolset-server builds for a claimed
    /// call: attested parent scope plus both durable journal stores, which
    /// async dispatch requires before any mutation (plan #1550 Task 2 note:
    /// either missing would silently weaken the lost-reply guarantee).
    async fn trusted_context(&self, call_id: &str) -> anyhow::Result<ToolInvocationContext> {
        let request = ToolRequest {
            operation_id: "operation".into(),
            call_id: call_id.into(),
            tool: SUBAGENT_SESSION_SEND_TOOL.into(),
            args: json!({}),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            parent_session_id: Some(PARENT.into()),
            parent_agent: None,
            parent_local_session_id: None,
            replay: None,
            tool_call_id: None,
            tool_round: Some(1),
            capabilities: Default::default(),
        };
        let journal = InvocationJournal::ensure(&self.js, 1).await?;
        journal
            .record(&request, (SUBAGENT_SESSION_SEND_TOOL, "helper", "server"))
            .await?;
        Ok(ToolInvocationContext {
            call_id: request.call_id.clone(),
            invoking_session_id: request.parent_session_id.clone(),
            run_context: request.run_context.clone(),
            checkpoint_store: Some(Arc::new(journal.checkpoint_store(&request))),
            partial_result_store: Some(Arc::new(journal.partial_result_store(&request))),
            ..Default::default()
        })
    }

    /// Invoke a registered tool the way the toolset-server does.
    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        context: ToolInvocationContext,
    ) -> Result<Value, ToolInvokeError> {
        Toolset::invoke_with_context(
            &self.toolset,
            ToolInvocation {
                tool: tool.to_string(),
                args,
                cancel: CancellationToken::new(),
                context,
            },
        )
        .await
    }

    /// Invoke without any transport context, the way a legacy in-process
    /// caller would.
    async fn invoke_standalone(&self, tool: &str, args: Value) -> Result<Value, ToolInvokeError> {
        Toolset::invoke(&self.toolset, tool, args, CancellationToken::new()).await
    }
}

#[tokio::test]
async fn send_dispatches_and_wait_observes_through_the_toolset() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let fixture = WiringFixture::new().await?;

    // Dispatch-only send: durable append, activation published, no wait.
    let sent = fixture
        .invoke(
            SUBAGENT_SESSION_SEND_TOOL,
            json!({"message": "wire the async tools", "session_id": SESSION}),
            fixture.trusted_context("wiring-send-1").await?,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert_eq!(sent["session_id"], json!(SESSION));
    let prompt_id = sent["prompt_id"].as_str().expect("durable prompt id");
    assert!(!prompt_id.is_empty());
    assert_eq!(sent["dispatch"], json!("confirmed"));
    assert_eq!(sent["status"], json!("pending"));
    assert_eq!(sent["terminal"], json!(false));

    // A zero-second wait takes one bounded snapshot without consuming the
    // result; with no child worker running the prompt stays pending and the
    // soft budget expires, never cancelling or failing the child. Dispatch
    // confirmation is only repaired by a wait that can confirm it, and a
    // zero-budget wait performs no repair, so it reports `null` (unknown).
    let waited = fixture
        .invoke(
            SUBAGENT_SESSION_WAIT_TOOL,
            json!({
                "session_id": SESSION,
                "prompt_id": prompt_id,
                "wait": {"seconds": 0},
            }),
            ToolInvocationContext {
                call_id: "wiring-wait-1".into(),
                invoking_session_id: Some(PARENT.into()),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert_eq!(waited["session_id"], json!(SESSION));
    assert_eq!(waited["prompt_id"], json!(prompt_id));
    assert!(waited["dispatch"].is_null(), "got: {}", waited["dispatch"]);
    assert_eq!(waited["status"], json!("pending"));
    assert_eq!(waited["terminal"], json!(false));
    assert_eq!(waited["wait_expired"], json!(true));

    // The prompt is durably appended in the child namespace, once.
    let storage = harnx_core::session_identity::session_key(Some("helper"), SESSION);
    let log = NatsSessionLog::new_with_replicas(fixture.js.clone(), storage, 1);
    let events = log.load_events_latest_async().await?;
    let user_prompts = events
        .iter()
        .filter(|(_, entry)| matches!(entry, harnx_core::session::SessionLogEntry::Message { role, .. } if role.is_user()))
        .count();
    assert_eq!(user_prompts, 1);

    Ok(())
}

#[tokio::test]
async fn send_and_wait_fail_closed_without_trusted_parent_scope() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let fixture = WiringFixture::new().await?;

    for (tool, args) in [
        (
            SUBAGENT_SESSION_SEND_TOOL,
            json!({"message": "unscoped dispatch"}),
        ),
        (
            SUBAGENT_SESSION_WAIT_TOOL,
            json!({"session_id": SESSION, "prompt_id": "prompt-1"}),
        ),
    ] {
        let error = fixture
            .invoke_standalone(tool, args)
            .await
            .expect_err("untrusted callers must not dispatch or observe");
        assert!(
            error.to_string().contains("trusted parent storage scope"),
            "expected trusted-scope failure for {tool}, got: {error}"
        );
    }

    // A forged hidden parent argument must not change that: send accepts the
    // transport-owned field, and the attested-scope check still fails closed.
    let forged = fixture
        .invoke_standalone(
            SUBAGENT_SESSION_SEND_TOOL,
            json!({"message": "unscoped dispatch", "__harnx_parent_session_id": "forged"}),
        )
        .await
        .expect_err("hidden args must not grant dispatch scope");
    assert!(
        forged.to_string().contains("trusted parent storage scope"),
        "got: {forged}"
    );

    Ok(())
}

#[tokio::test]
async fn send_sanitizes_model_supplied_parent_args_before_dispatch() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let fixture = WiringFixture::new().await?;
    let context = fixture.trusted_context("wiring-send-2").await?;

    // `__harnx_parent_session_id` is transport-owned: the toolset impl
    // overwrites any model-supplied value with the attested scope before the
    // dispatcher sees the arguments, so a forged value can neither poison the
    // child session's parent nor trip the arg/scope disagreement check.
    let sent = fixture
        .invoke(
            SUBAGENT_SESSION_SEND_TOOL,
            json!({
                "message": "sanitize hidden args",
                "__harnx_parent_session_id": "forged-parent",
            }),
            context,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    assert_eq!(sent["dispatch"], json!("confirmed"));
    assert_eq!(sent["status"], json!("pending"));

    Ok(())
}
