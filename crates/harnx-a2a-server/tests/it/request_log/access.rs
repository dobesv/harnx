use super::*;
use crate::support::{Harness, Script};
use harnx_core::access_rules::AccessRules;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_access_rejections_and_protected_cards_are_not_duplicated() -> Result<()> {
    let rules = Arc::new(AccessRules::from_yaml(&format!(
        "rules:\n  - agents: [runner@runner]\n    scopes: [admin]\n    users: [\"{CALLER}\"]\n"
    ))?);
    let h = Harness::start_with_access_rules(Script::Text, Some(rules.clone())).await?;
    let http = Http::with_access_rules(h, Some(rules)).await?;
    let result = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            CALLER,
            "SendMessage",
            json!({"message":message("no-prompt-scope")}),
        ))
        .await?;
    assert_eq!(result["error"]["code"], -32010);
    let hidden = http.url.replace("/runner", "/other");
    let result = http
        .rpc_call(HttpRpcCall::new(
            &hidden,
            CALLER,
            "GetTask",
            json!({"id":"hidden-task"}),
        ))
        .await?;
    assert_eq!(result["error"]["code"], -32001);
    let response = http
        .client
        .get(format!("{}/.well-known/agent-card.json", http.url))
        .send()
        .await?;
    assert_eq!(response.status(), 401);
    let response = http
        .client
        .get(format!("{hidden}/.well-known/agent-card.json"))
        .header("X-User-ID", CALLER)
        .send()
        .await?;
    assert_eq!(response.status(), 404);
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 4);
    assert_eq!(records[0]["method"], "SendMessage");
    assert_eq!(records[0]["error_code"], -32010);
    assert_eq!(
        records[0]["error_message"],
        "session creation requires prompt scope"
    );
    assert_eq!(records[1]["export"], "other");
    assert_eq!(records[1]["method"], "GetTask");
    assert_eq!(records[1]["task_id"], "hidden-task");
    assert_eq!(records[1]["error_code"], -32001);
    assert_eq!(records[2]["method"], "AgentCard");
    assert_eq!(records[2]["error_code"], -32000);
    assert_eq!(records[2]["http_status"], 401);
    assert_eq!(records[3]["method"], "AgentCard");
    assert_eq!(records[3]["export"], "other");
    assert_eq!(records[3]["http_status"], 404);
    assert_private(&http.h.logs);
    Ok(())
}
