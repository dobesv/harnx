//! A sub-agent session created through the sub-agent toolset carries the
//! parent's caller-principals snapshot verbatim (`inherit_from_parent`).

use crate::worker_principals_fixtures as fixtures;

use anyhow::Result;
use harnx_core::require_nextest;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_session_inherits_the_parent_principals_snapshot() -> Result<()> {
    require_nextest();
    // The parent's one turn delegates a prompt to `beta` through
    // `beta_session_prompt`; no explicit session_id, so the tool allocates
    // the child and inherits the parent's identity through
    // `inherit_from_parent`.
    let Some(scenario) = fixtures::scenario_or_skip(fixtures::ScenarioPlan {
        worker_name: "worker-subagent-principals",
        call: fixtures::ScriptedCall {
            tool: "beta_session_prompt",
            arguments: json!({"message": "finish delegated work"}),
            tool_call_id: "subagent-principals-call-1",
            reply_text: "delegation completed",
        },
        parent_session_id: "subagent-principals-parent",
        prompt: "delegate to the child",
    })
    .await?
    else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let child_session_id =
        fixtures::delegated_child_session_id(&scenario.harness.js, "subagent-principals-call-1")
            .await?;

    let child =
        fixtures::assert_snapshot_inherited(&scenario.harness.js, "beta", &child_session_id)
            .await?;
    assert_eq!(
        harnx_runtime::nats_session_metadata::session_properties(&child)?.text("user_id"),
        Some("alice")
    );

    scenario.finish().await;
    Ok(())
}
