//! Real bridge/native results must stay bounded in the next HTTP inference, not only NATS.
use super::result_fixture::{
    BridgeScript, CapturedProvider, McpBridge, ProviderSession, ResultEnvironment, SessionSpec,
    BRIDGE_CAP,
};
use super::*;
use anyhow::Context;
use harnx_toolset::ToolErrorPayload;
use harnx_toolset_server::invocation_journal::InvocationJournal;
use serde_json::Value;

#[path = "bounded_results/assertions.rs"]
mod assertions;
#[path = "bounded_results/fixture.rs"]
mod fixture;
#[path = "bounded_results/provider.rs"]
mod provider;
use assertions::*;
use fixture::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_bridge_and_native_preserve_sibling_and_bound_next_inference() -> Result<()> {
    require_nextest();
    let fixture = Fixture::start().await?;
    let provider = provider::capture_round().await?;
    let session = fixture.session(&provider.base).await?;
    session.run_turn(None).await?;
    let requests = provider.finish().await?;
    assert_eq!(
        requests.len(),
        2,
        "initial inference and successful same-turn continuation"
    );
    assert_eq!(
        session
            .config
            .read()
            .session
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .role,
        MessageRole::Assistant
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);

    let next = &requests[1];
    let bridge = tool_output(next, "bridge-large")?;
    assert_bridge_result(&fixture.env.js, &bridge).await?;
    let native = tool_output(next, "native-large")?;
    assert_native_result(&fixture.env.js, &native).await?;
    assert_eq!(
        tool_output(next, "native-sibling")?,
        native_output("native_sibling")
    );
    assert_provider_context(next, &bridge, &native)?;
    assert_durable_completions(&fixture.env.js, &session.key).await?;
    assert_eq!(fixture.bridge.call_count()?, 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuine_attachment_storage_failure_aborts_round_before_next_inference() -> Result<()> {
    require_nextest();
    let fixture = Fixture::start().await?;
    harnx_blob_store::create_or_open_attachments_bucket(&fixture.env.js, 1).await?;
    let stream = fixture.env.js.get_stream("OBJ_harnx_attachments").await?;
    let mut sealed = stream.cached_info().config.clone();
    sealed.sealed = true;
    fixture.env.js.update_stream(sealed).await?;
    let provider = provider::capture_storage_failure().await?;
    let session = fixture.session(&provider.base).await?;
    let error = session.run_turn(None).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("retain oversized tool reply"),
        "{error:#}"
    );
    assert_eq!(
        provider.finish().await?.len(),
        1,
        "infrastructure failure must not become model-visible size recovery"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let rows = InvocationJournal::ensure(&fixture.env.js, 1)
        .await?
        .records_for_session(&session.key)
        .await?;
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].reply.is_none(),
        "failed attachment retention must not claim durable completion"
    );
    fixture.stop().await
}
