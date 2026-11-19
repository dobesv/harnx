//! A2UI forms use the same canonical-data fixtures as the other renderers.
use super::*;
use serde_json::json;

pub(crate) fn cases() -> Value {
    serde_json::from_str(include_str!(
        "../../../../harnx-core/tests/fixtures/a2ui-build-data-cases.json"
    ))
    .unwrap()
}

pub(crate) fn surface() -> Value {
    let mut surface = cases()[0]["surface"].clone();
    surface["createSurface"]["surfaceId"] = json!("deploy-form");
    surface["createSurface"]["catalogId"] =
        json!(harnx_core::interaction::surface::BASIC_CATALOG_ID);
    surface
}

pub(super) fn agent(script: Script, dir: &std::path::Path) -> Result<String> {
    let form = surface();
    let mut body = format!("result=$(interact --id first '{form}')\n");
    if script == Script::A2uiSerial {
        let mut second = form.clone();
        second["createSurface"]["surfaceId"] = json!("second-form");
        second["createSurface"]["components"][2]["text"] = json!("Second deployment target");
        body.push_str(&format!("result=$(interact --id second '{second}')\n"));
    }
    body.push_str("if [ \"$(printf '%s' \"$result\" | jq -r .action)\" = deny ]; then deny 'Dismissed'; else next; fi");
    let tool = if script == Script::A2uiSameRound {
        write_same_round_tools_agent(dir)?;
        "time_get_current_time"
    } else {
        "bash_exec"
    };
    let middleware = json!([{"name": "form", "match": format!("^{tool}$"), "bash": body}]);
    Ok(format!("---\nmodel: mock:test\nuse_tools: [{tool}]\nmiddleware: {middleware}\n---\nTest A2UI form\n"))
}

pub(crate) const MEDIA_TYPE: &str = "application/a2ui+json";

pub(crate) fn data_part(task: &Value) -> &Value {
    task["status"]["message"]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| part["mediaType"] == MEDIA_TYPE)
        .expect("A2UI DataPart")
}

pub(crate) fn action_reply(id: &str, task: &Value, action: &str, values: Value) -> Value {
    let surface = &data_part(task)["data"]["createSurface"]["surfaceId"];
    json!({"message": {
        "messageId": id, "contextId": task["contextId"], "taskId": task["id"], "role": "ROLE_USER",
        "parts": [{"data": {"version": "1.0", "action": {
            "name": action, "surfaceId": surface, "sourceComponentId": "submit",
            "timestamp": "2026-06-30T12:00:00Z", "context": {},
            "metadata": {"extensions": {"harnx_response": {"data": values}}}
        }}, "mediaType": MEDIA_TYPE}]
    }})
}
