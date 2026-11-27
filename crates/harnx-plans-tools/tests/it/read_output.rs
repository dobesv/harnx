use super::{assert_linked_resource, cid_from, create_plan, json_result, text, TestContext};
use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset, CAPABILITY_STRUCTURED_RESULT};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn nats_document_getters_return_script_fields_and_keep_direct_links() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let (plan, note, task) = seed_documents(&context).await;
    let specs = context.toolset.tools();
    for (tool, kind, id, args, fields) in [
        (
            "get_plan",
            "plan",
            &plan,
            json!({"plan": plan}),
            json!({"github_issue": 2991, "task_ids": [task], "note_ids": [note]}),
        ),
        (
            "get_note",
            "note",
            &note,
            json!({"plan": plan, "note_id": "design"}),
            json!({"summary": "Design", "author": null}),
        ),
        (
            "get_task",
            "task",
            &task,
            json!({"plan": plan, "id": task}),
            json!({"title": "Review", "tags": ["review"], "status": "open", "plan": plan}),
        ),
    ] {
        let direct = context.invoke(tool, args.clone(), None).await;
        let scripted = invoke_for_script(&context, tool, args).await;
        assert_eq!(direct["isError"], false, "{direct}");
        assert_eq!(scripted["isError"], false, "{scripted}");
        let document = json_result(&direct);
        assert_eq!(document["id"], id.as_str());
        assert_eq!(document["body"], "First line\nSecond line ✓");
        for (field, value) in fields.as_object().expect("document fields") {
            assert_eq!(&document[field], value, "{tool}.{field}");
        }
        assert!(direct.get("structuredContent").is_none(), "{direct}");
        assert_linked_resource(&direct, &plan, "plan");
        assert_linked_resource(&direct, id, kind);
        assert_eq!(scripted["structuredContent"], document, "{tool}");
        let content = scripted["content"].as_array().expect("content blocks");
        assert_eq!(content.len(), 1, "{scripted}");
        assert_eq!(content[0]["type"], "text", "{scripted}");
        assert_eq!(json_result(&scripted), document, "{tool}");

        let spec = specs.iter().find(|spec| spec.name == tool).expect(tool);
        assert_schema_parity(spec.output_schema().expect("output schema"), &document);
    }
}

#[tokio::test]
async fn nats_list_tools_return_script_pages_without_changing_direct_json() {
    let Some(context) = TestContext::start().await else {
        return;
    };
    let (plan, note, task) = seed_documents(&context).await;
    let second_plan = create_plan(&context, "Zeta").await;
    let (second_note, second_task) = seed_more_items(&context, &plan).await;
    let mut tasks = [task, second_task];
    tasks.sort();

    let specs = context.toolset.tools();
    for (tool, mut args, ids) in [
        (
            "list_plans",
            json!({"limit": 1}),
            [plan.clone(), second_plan],
        ),
        ("list_tasks", json!({"plan": plan, "limit": 1}), tasks),
        (
            "list_notes",
            json!({"plan": plan, "limit": 1}),
            [note, second_note],
        ),
    ] {
        let spec = specs.iter().find(|spec| spec.name == tool).expect(tool);
        let schema = spec.output_schema().expect("list output schema");
        let first = assert_list_page(&context, tool, &args, schema, &ids[0]).await;
        let cursor = first["next_cursor"]
            .as_str()
            .expect("non-final page cursor");
        assert!(!cursor.is_empty());
        args["cursor"] = json!(cursor);
        let last = assert_list_page(&context, tool, &args, schema, &ids[1]).await;
        assert!(last["next_cursor"].is_null(), "{tool}: {last}");

        if tool == "list_plans" {
            let item = &first["items"][0];
            assert_eq!(item["task_count"], 2);
            assert_eq!(item["note_count"], 2);
            assert!(item.get("body").is_none());
        } else if tool == "list_notes" {
            assert_eq!(last["items"][0]["body"], format!("{}…", "é".repeat(199)));
        }
    }
}

async fn assert_list_page(
    context: &TestContext,
    tool: &str,
    args: &Value,
    schema: &Value,
    id: &str,
) -> Value {
    let direct = context.invoke(tool, args.clone(), None).await;
    let page = json_result(&direct);
    let script = invoke_for_script(context, tool, args.clone()).await;
    assert_eq!(direct["isError"], false, "{direct}");
    assert_eq!(script["isError"], false, "{script}");
    assert!(direct.get("structuredContent").is_none(), "{direct}");
    assert_eq!(
        direct["content"].as_array().expect("content blocks").len(),
        1
    );
    assert_eq!(direct["content"][0]["type"], "text");
    assert_eq!(
        text(&direct),
        serde_json::to_string_pretty(&page).expect("pretty JSON"),
        "{tool}"
    );
    assert_eq!(script["structuredContent"], page, "{tool}");
    assert_eq!(
        script["content"],
        json!([{"type": "text", "text": page.to_string()}]),
        "{tool}"
    );
    assert_schema_parity(schema, &page);
    let items = page["items"].as_array().expect("page items");
    assert_eq!(items.len(), 1, "{tool}: {page}");
    assert_eq!(items[0]["id"], id, "{tool}");
    page
}

async fn seed_more_items(context: &TestContext, plan: &str) -> (String, String) {
    let note = context
        .invoke(
            "add_note",
            json!({"plan": plan, "id": "guidance", "body": "é".repeat(250)}),
            None,
        )
        .await;
    let task = context
        .invoke("add_task", json!({"plan": plan, "title": "Schedule"}), None)
        .await;
    (cid_from(&note), cid_from(&task))
}

async fn seed_documents(context: &TestContext) -> (String, String, String) {
    let body = "First line\nSecond line ✓";
    let added = context
        .invoke(
            "add_plan",
            json!({"name": "Script output", "content": body, "github_issue": 2991}),
            Some(context.caller.clone()),
        )
        .await;
    let plan = cid_from(&added);
    let note = context
        .invoke(
            "add_note",
            json!({"plan": plan, "id": "design", "body": body, "summary": "Design"}),
            None,
        )
        .await;
    let task = context
        .invoke(
            "add_task",
            json!({"plan": plan, "title": "Review", "body": body, "tags": ["review"]}),
            None,
        )
        .await;
    (plan, cid_from(&note), cid_from(&task))
}

async fn invoke_for_script(context: &TestContext, tool: &str, args: Value) -> Value {
    context
        .toolset
        .invoke_with_context(ToolInvocation {
            tool: tool.to_string(),
            args,
            context: ToolInvocationContext {
                capabilities: [CAPABILITY_STRUCTURED_RESULT.to_string()].into(),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        })
        .await
        .expect("scripted read result")
}

fn assert_schema_parity(schema: &Value, document: &Value) {
    assert_eq!(schema["type"], "object");
    let properties = schema["properties"].as_object().expect("schema fields");
    let fields = document.as_object().expect("document fields");
    assert_eq!(properties.len(), fields.len());
    for (name, value) in fields {
        let property = properties.get(name).expect(name);
        assert!(property["description"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        let kind = json_kind(value);
        let types = &property["type"];
        assert!(
            types == kind
                || types
                    .as_array()
                    .is_some_and(|types| types.contains(&json!(kind))),
            "{name}: {value} does not match {property}"
        );
        if let Value::Array(items) = value {
            assert_array_schema(&property["items"], items);
        }
    }
}

fn assert_array_schema(schema: &Value, items: &[Value]) {
    if schema["type"] == "object" {
        for item in items {
            assert_schema_parity(schema, item);
        }
    } else {
        assert_eq!(schema["type"], "string");
        assert!(items.iter().all(Value::is_string));
    }
}

fn json_kind(value: &Value) -> &str {
    match value {
        Value::String(_) => "string",
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        _ => panic!("unexpected document field: {value}"),
    }
}
