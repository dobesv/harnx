use crate::{
    api_types::CompletionTokenUsage,
    event::{SubAgentProgress, SubAgentProgressStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

// Copy of the pre-summary wire reader. Unknown optional fields must remain safe.
#[derive(Debug, Serialize, Deserialize)]
struct OldProgress {
    invocation_id: String,
    agent: String,
    session_id: String,
    status: SubAgentProgressStatus,
    elapsed_ms: u64,
    usage: CompletionTokenUsage,
    tool_call_count: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

fn old_json() -> serde_json::Value {
    json!({"invocation_id":"prompt", "agent":"helper", "session_id":"child", "status":"running", "elapsed_ms":42,
        "usage":CompletionTokenUsage::default(), "tool_call_count":1})
}

#[test]
fn subagent_blocked_old_json_defaults_to_no_pending_interaction() {
    let progress: SubAgentProgress = serde_json::from_value(old_json()).unwrap();
    assert!(progress.pending_interaction.is_none());
    assert!(serde_json::to_value(progress)
        .unwrap()
        .get("pending_interaction")
        .is_none());
}

#[test]
fn subagent_blocked_summary_is_ignored_by_old_reader() {
    let mut value = old_json();
    value["pending_interaction"] = json!({"agent":"helper", "session_id":"child", "kind":"legacy_approval", "cluster":"__local__",
        "request":{"invocation_id":"prompt", "tool_round_seq":2,"request_seq":3,"tool_call_id":"call"},
        "tool_name":"bash_exec", "message":"Allow command?", "pending_count":0});
    let progress: SubAgentProgress = serde_json::from_value(value.clone()).unwrap();
    let old: OldProgress = serde_json::from_value(value).unwrap();
    assert_eq!(old.status, SubAgentProgressStatus::Running);
    assert_eq!(old.invocation_id, progress.invocation_id);
    assert_eq!(
        progress.pending_interaction.unwrap().message,
        "Allow command?"
    );
}
