use super::*;
use harnx_core::{execution_context::ExecutionContextObservation, tool::ToolCall};
use serde_json::json;

#[test]
fn private_result_metadata_and_status_survive_serialized_call_checkpoint() -> Result<()> {
    let observation = ExecutionContextObservation {
        version: 1,
        observed_at: chrono::Utc::now(),
        workspace_root: "/private/workspace".into(),
        working_directory: "/private/workspace/repo".into(),
        repository: None,
        provenance: None,
    };
    let mut result = harnx_core::tool::ToolResult::new(
        ToolCall::new("echo".into(), json!({}), Some("a".into()), None),
        json!("done"),
    );
    result.execution_context = Some(observation.clone());
    assert!(
        !serde_json::to_string(&result)?.contains("private/workspace"),
        "public ToolResult must stay private"
    );
    for status in [
        InnerToolCallStatus::Ok,
        InnerToolCallStatus::ToolError,
        InnerToolCallStatus::Blocked,
    ] {
        let checkpoint = ToolCallCheckpoint {
            state: json!({"version": 1, "frames": []}),
            snapshots: [(
                "ref".into(),
                MaterializedToolResult::new(result.clone(), status),
            )]
            .into(),
        };
        let bytes = serde_json::to_vec(&CallCheckpoint::new(&checkpoint))?;
        let saved: CallCheckpoint = serde_json::from_slice(&bytes)?;
        let restored = saved.restore()?;
        let snapshot = &restored.snapshots["ref"];
        assert_eq!(snapshot.result.execution_context, Some(observation.clone()));
        assert_eq!(snapshot.status, status);
        assert_eq!(snapshot.result.call, result.call);
        assert_eq!(restored.state, checkpoint.state);
    }
    Ok(())
}
