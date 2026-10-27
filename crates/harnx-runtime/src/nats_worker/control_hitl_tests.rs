#[path = "control_hitl_support.rs"]
mod control_hitl_support;
use control_hitl_support::*;

use super::*;

use harnx_core::session::SessionLogEntry;
use serde_json::json;

#[test]
fn hitl_control_request_seq_is_optional_on_the_wire() {
    let legacy = json!({"type": "hitl_approval_decision", "tool_call_id": "call", "approved": true, "note": null});
    let command: ControlCommand = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(
        command,
        ControlCommand::HitlApprovalDecision {
            request_seq: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_value(command).unwrap(), legacy);
    let command = ControlCommand::HitlApprovalDecision {
        tool_call_id: "call".into(),
        approved: false,
        note: None,
        request_seq: Some(42),
    };
    assert_eq!(
        ControlCommand::from_bytes(&command.to_bytes().unwrap()).unwrap(),
        command
    );
    #[derive(Deserialize)]
    struct LegacyDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
    }
    let old: LegacyDecision = serde_json::from_slice(&command.to_bytes().unwrap()).unwrap();
    assert_eq!(old.tool_call_id, "call");
    assert!(!old.approved);
    assert_eq!(old.note, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_control_fences_replaced_requests_and_reused_ids_at_append() -> Result<()> {
    let mut fixture = HitlControlFixture::start().await?;
    let replacement = verify_same_round_replacement(&mut fixture).await?;
    verify_reused_id_next_round(&mut fixture, replacement).await?;
    verify_terminal_precedence_at_append(&mut fixture).await
}
