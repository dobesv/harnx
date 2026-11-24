//! A handoff target carries the source session's caller-principals snapshot
//! verbatim: no recomputation, no execution context, identical values.

use crate::worker_principals_fixtures as fixtures;

use anyhow::Result;
use harnx_core::require_nextest;
use serde_json::json;

const TARGET_SESSION: &str = "handoff-principals-target";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_target_inherits_the_source_principals_snapshot() -> Result<()> {
    require_nextest();
    // The parent's one turn requests a handoff to `beta`; the target turn
    // completes with plain text and no further tool calls.
    let Some(scenario) = fixtures::scenario_or_skip(fixtures::ScenarioPlan {
        worker_name: "worker-handoff-principals",
        call: fixtures::ScriptedCall {
            tool: "beta_session_handoff",
            arguments: json!({"prompt": "finish delegated work", "session_id": TARGET_SESSION}),
            tool_call_id: "handoff-principals-call-1",
            reply_text: "handoff completed",
        },
        parent_session_id: "handoff-principals-source",
        prompt: "start handoff",
    })
    .await?
    else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let target =
        fixtures::assert_snapshot_inherited(&scenario.harness.js, "beta", TARGET_SESSION).await?;
    // Identity only: the handoff never copies the source execution context.
    assert!(!target
        .extensions
        .contains_key(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE));
    assert!(!target
        .extensions
        .contains_key(harnx_runtime::nats_session_metadata::TOOL_CONTEXT_NAMESPACE));

    scenario.finish().await;
    Ok(())
}
