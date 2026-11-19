use super::pending::attach_pending_progress;
use harnx_core::pending_interaction::{
    ApprovalRef, PendingInteractionRequest, PendingInteractionSummary,
};
use serde_json::json;

fn summary() -> PendingInteractionSummary {
    PendingInteractionSummary {
        agent: "helper".into(),
        cluster: "__local__".into(),
        session_id: "child".into(),
        request: PendingInteractionRequest::LegacyApproval {
            request: ApprovalRef {
                invocation_id: "prompt".into(),
                tool_round_seq: 2,
                request_seq: 3,
                tool_call_id: "call".into(),
            },
        },
        tool_name: "bash_exec".into(),
        message: "🦀".repeat(128),
        pending_count: 0,
    }
}

#[test]
fn subagent_blocked_overflow_preserves_exact_summary_and_page_restart() {
    let mut value = json!({"session_id":"child", "prompt_id":"prompt", "status":"blocked", "terminal":false,
        "output":{"events":[{"index":7,"text":"x".repeat(3000)}],"next_index":8,"omitted_ranges":[],"continuation":null}});
    attach_pending_progress(&mut value, summary(), None, 1024);
    assert!(serde_json::to_vec(&value).unwrap().len() <= 1024);
    assert_eq!(value["pending_interaction"]["request"]["request_seq"], 3);
    assert_eq!(value["pending_interaction"]["tool_name"], "bash_exec");
    assert!(value.get("sub_agent_progress").is_none());
    assert_eq!(value["output"]["events"], json!([]));
    assert_eq!(value["output"]["next_index"], 7);
    assert_eq!(value["output"]["truncated"], true);
    assert!(
        value["pending_interaction"]["message"]
            .as_str()
            .unwrap()
            .len()
            <= 512
    );
}

#[test]
fn subagent_blocked_drops_events_before_progress() {
    let mut value = json!({"session_id":"child", "prompt_id":"prompt", "status":"blocked", "terminal":false,
        "output":{"events":[{"index":7,"text":"x".repeat(3000)}]}});
    attach_pending_progress(&mut value, summary(), None, 2200);
    assert!(value.get("sub_agent_progress").is_some());
    assert_eq!(
        value["pending_interaction"],
        value["sub_agent_progress"]["pending_interaction"]
    );
    assert_eq!(value["output"]["events"], json!([]));
    assert!(serde_json::to_vec(&value).unwrap().len() <= 2200);
}
