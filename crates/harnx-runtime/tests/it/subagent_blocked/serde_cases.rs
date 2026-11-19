use anyhow::Result;
use harnx_core::{
    event::{SubAgentProgress, SubAgentProgressStatus},
    pending_interaction::{PendingInteractionRequest, PendingInteractionSummary},
};
use serde_json::json;

#[test]
fn subagent_blocked_serde_compatibility() -> Result<()> {
    // 1. Deserializing older JSON without pending_interaction yields None
    let old_json = json!({
        "invocation_id": "inv-1",
        "agent": "helper",
        "session_id": "session-1",
        "status": "running",
        "elapsed_ms": 100,
        "usage": { "input_tokens": 0, "output_tokens": 0, "cached_tokens": 0 },
        "tool_call_count": 0
    });
    let decoded: SubAgentProgress = serde_json::from_value(old_json)?;
    assert!(decoded.pending_interaction.is_none());

    // 2. Round-trip with pending_interaction
    let summary = PendingInteractionSummary {
        agent: "helper".into(),
        cluster: "local".into(),
        session_id: "session-1".into(),
        request: PendingInteractionRequest::Interaction {
            interaction_id: "int-123".into(),
            request_seq: 42,
            interaction_kind: "confirm".into(),
        },
        tool_name: "probe_run".into(),
        message: "Confirm action?".into(),
        pending_count: 0,
    };
    let progress = SubAgentProgress {
        invocation_id: "inv-1".into(),
        agent: "helper".into(),
        session_id: "session-1".into(),
        status: SubAgentProgressStatus::Running,
        elapsed_ms: 100,
        usage: Default::default(),
        tool_call_count: 0,
        title: None,
        tool_call_id: None,
        pending_interaction: Some(summary.clone()),
    };
    let encoded = serde_json::to_value(&progress)?;
    let roundtripped: SubAgentProgress = serde_json::from_value(encoded.clone())?;
    assert_eq!(roundtripped.pending_interaction, Some(summary));

    // 3. Older reader struct without pending_interaction field can deserialize new JSON
    #[derive(serde::Deserialize)]
    struct LegacyProgressReader {
        invocation_id: String,
        agent: String,
        session_id: String,
        status: SubAgentProgressStatus,
    }
    let legacy: LegacyProgressReader = serde_json::from_value(encoded)?;
    assert_eq!(legacy.invocation_id, "inv-1");
    assert_eq!(legacy.agent, "helper");
    assert_eq!(legacy.session_id, "session-1");
    assert_eq!(legacy.status, SubAgentProgressStatus::Running);
    Ok(())
}
