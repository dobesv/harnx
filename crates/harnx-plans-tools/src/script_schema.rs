//! Script output fields for plan, task, and note reads, shown by `help`.

use harnx_toolset::{output_field, output_nullable_field, ScriptOutput};
use serde_json::{json, Value};

pub(crate) fn document_output() -> ScriptOutput {
    ScriptOutput {
        summary: Some("the document as JSON".to_string()),
        ..ScriptOutput::default()
    }
}

pub(crate) fn list_output() -> ScriptOutput {
    ScriptOutput {
        summary: Some("one page of matching items as JSON".to_string()),
        ..ScriptOutput::default()
    }
}

pub(crate) fn plan_schema() -> Value {
    let mut properties = plan_metadata_properties();
    properties["body"] = output_field("string", "full Markdown body");
    properties["task_ids"] = string_array("canonical URLs of tasks in the plan");
    properties["note_ids"] = string_array("canonical URLs of notes in the plan");
    json!({"type": "object", "properties": properties})
}

fn plan_metadata_properties() -> Value {
    json!({
        "id": output_field("string", "canonical plan URL"),
        "title": output_nullable_field("string", "plan title"),
        "summary": output_nullable_field("string", "short description of the plan"),
        "author": output_nullable_field("string", "plan author"),
        "assignee": output_nullable_field("string", "who is assigned to the plan"),
        "executor": output_nullable_field("string", "agent designated to execute the plan"),
        "git_branch": output_nullable_field("string", "Git branch associated with the plan"),
        "github_owner_repo": output_nullable_field("string", "GitHub repository as owner/repo"),
        "github_issue": output_nullable_field("integer", "GitHub issue number"),
        "external_task_url": output_nullable_field("string", "URL of the task in an external tracker"),
        "created_at": output_field("string", "creation timestamp"),
        "updated_at": output_nullable_field("string", "last update timestamp"),
    })
}

pub(crate) fn list_plans_schema() -> Value {
    let mut properties = plan_metadata_properties();
    properties["task_count"] = output_field("integer", "number of tasks in the plan");
    properties["note_count"] = output_field("integer", "number of notes in the plan");
    page_schema(
        json!({"type": "object", "properties": properties}),
        "plan metadata and task and note counts",
    )
}

pub(crate) fn list_tasks_schema() -> Value {
    page_schema(
        task_schema(),
        "tasks matching the filters within this page of keys",
    )
}

pub(crate) fn list_notes_schema() -> Value {
    let mut item = note_schema();
    item["properties"]["body"]["description"] = json!("Markdown preview capped at 200 characters");
    page_schema(item, "notes with body previews")
}

fn page_schema(item: Value, description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {"type": "array", "items": item, "description": description},
            "next_cursor": output_nullable_field(
                "string",
                "opaque cursor for the next page with the same scope and filters, or null at the end",
            ),
        },
    })
}

pub(crate) fn note_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": output_field("string", "canonical note URL"),
            "summary": output_nullable_field("string", "short description of the note"),
            "author": output_nullable_field("string", "note author"),
            "created_at": output_field("string", "creation timestamp"),
            "updated_at": output_nullable_field("string", "last update timestamp"),
            "body": output_field("string", "full Markdown body"),
        },
    })
}

pub(crate) fn task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": output_field("string", "canonical task URL"),
            "title": output_field("string", "task title"),
            "summary": output_nullable_field("string", "short description of the task"),
            "author": output_nullable_field("string", "task author"),
            "assignee": output_nullable_field("string", "who is assigned to the task"),
            "executor": output_nullable_field("string", "agent designated to execute the task"),
            "tags": string_array("tags assigned to the task"),
            "plan": output_field("string", "canonical URL of the containing plan"),
            "status": output_field("string", "current task status"),
            "created_at": output_field("string", "creation timestamp"),
            "updated_at": output_nullable_field("string", "last update timestamp"),
            "dependencies": string_array("canonical URLs of prerequisite tasks"),
            "body": output_field("string", "full Markdown body"),
        },
    })
}

fn string_array(description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_output_maps_the_whole_object() {
        assert_eq!(
            serde_json::to_value(document_output()).expect("serialize script output"),
            json!({"summary": "the document as JSON"}),
        );
    }

    #[test]
    fn list_output_maps_the_whole_page() {
        assert_eq!(
            serde_json::to_value(list_output()).expect("serialize script output"),
            json!({"summary": "one page of matching items as JSON"}),
        );
    }

    #[test]
    fn list_schemas_describe_pages_and_their_item_shapes() {
        let plans = list_plans_schema();
        let tasks = list_tasks_schema();
        let notes = list_notes_schema();
        for page in [&plans, &tasks, &notes] {
            assert_eq!(page["type"], "object");
            let properties = &page["properties"];
            assert_eq!(properties["items"]["type"], "array");
            assert_eq!(properties["items"]["items"]["type"], "object");
            assert_eq!(properties["next_cursor"]["type"], json!(["string", "null"]));
        }

        let plan_summary = plans["properties"]["items"]["items"]["properties"]
            .as_object()
            .expect("plan summary properties");
        assert_eq!(plan_summary.len(), 14);
        for field in ["task_count", "note_count"] {
            assert_eq!(plan_summary[field]["type"], "integer");
        }
        for field in ["body", "task_ids", "note_ids"] {
            assert!(!plan_summary.contains_key(field));
        }
        assert_eq!(tasks["properties"]["items"]["items"], task_schema());
        let mut note = notes["properties"]["items"]["items"].clone();
        assert_eq!(
            note["properties"]["body"]["description"],
            "Markdown preview capped at 200 characters",
        );
        note["properties"]["body"]["description"] = json!("full Markdown body");
        assert_eq!(note, note_schema());
    }

    #[test]
    fn document_schemas_describe_nullable_metadata_and_string_arrays() {
        let plan = plan_schema();
        let note = note_schema();
        let task = task_schema();
        for schema in [&plan, &note, &task] {
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["properties"]["body"]["type"], "string");
            assert_eq!(
                schema["properties"]["updated_at"]["type"],
                json!(["string", "null"]),
            );
        }
        assert_eq!(
            plan["properties"]["github_issue"]["type"],
            json!(["integer", "null"])
        );
        assert_eq!(
            note["properties"]["summary"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(task["properties"]["title"]["type"], "string");
        for property in ["task_ids", "note_ids"] {
            assert_string_array(&plan["properties"][property]);
        }
        for property in ["tags", "dependencies"] {
            assert_string_array(&task["properties"][property]);
        }
    }

    fn assert_string_array(property: &Value) {
        assert_eq!(property["type"], "array");
        assert_eq!(property["items"]["type"], "string");
    }
}
