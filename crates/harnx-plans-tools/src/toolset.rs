use crate::server::*;
use crate::tool_templates;
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorData, Tool};
use rmcp::schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Native toolset for file-backed plan, task, and note management.
#[derive(Clone)]
pub struct PlansToolset {
    server: PlansServer,
}

impl PlansToolset {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            server: PlansServer::new(dir),
        }
    }
}

macro_rules! dispatch_plan_tools {
    (
        $server:expr, $tool:expr, $args:expr;
        $( $name:literal => $mode:ident $handler:ident, $params:ty; )+
    ) => {
        match $tool {
            $(
                $name => dispatch_plan_tools!(
                    @call $server, $args, $mode, $handler, $params
                ),
            )+
            _ => unknown_tool($tool),
        }
    };
    (@call $server:expr, $args:expr, with_args, $handler:ident, $params:ty) => {
        map_result($server.$handler(parse_args::<$params>($args)?).await)
    };
    (@call $server:expr, $args:expr, no_args, $handler:ident, $params:ty) => {{
        let _params = parse_args::<$params>($args)?;
        map_result($server.$handler().await)
    }};
}

fn input_schema<T: JsonSchema + 'static>() -> Value {
    Tool::new("schema", "schema", Map::new())
        .with_input_schema::<T>()
        .schema_as_json_value()
}

/// Build a spec with the tool's call template plus the shared result template.
fn spec<T: JsonSchema + 'static>(
    name: &str,
    description: &str,
    read_only_hint: bool,
    call_template: &str,
) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: input_schema::<T>(),
        idempotent_hint: false,
        read_only_hint,
        timeout_secs: None,
        meta: None,
    }
    .with_call_template(call_template)
    .with_result_template(tool_templates::RESULT)
}

fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|err| ToolInvokeError::Recoverable(format!("invalid tool arguments: {err}")))
}

fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|err| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {err}"))
        }),
        Err(err) => Err(ToolInvokeError::Recoverable(err.message.to_string())),
    }
}

fn unknown_tool(tool: &str) -> Result<Value, ToolInvokeError> {
    Err(ToolInvokeError::Recoverable(format!(
        "unknown plans tool: {tool}"
    )))
}

/// Declares the tool table: params type, tool name, read-only hint, call
/// template, description. A macro because `spec` is generic over the params
/// type, so the rows cannot live in a plain array.
macro_rules! plan_tool_specs {
    (
        $( $params:ty : $name:literal, $read_only:literal, $template:ident, $description:literal; )+
    ) => {
        vec![
            $( spec::<$params>($name, $description, $read_only, tool_templates::$template), )+
        ]
    };
}

#[async_trait]
impl Toolset for PlansToolset {
    fn name(&self) -> &str {
        "plans"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        plan_tool_specs![
            ListPlansParams: "list_plans", true, LIST_PLANS_CALL,
                "List all plans with metadata and task/note counts.";
            AddPlanParams: "add_plan", false, ADD_PLAN_CALL,
                "Create a new plan with optional metadata. Set body with content (or body for compatibility). Keep body content under 1000 words per call; use update_plan with replace_in_content for targeted edits.";
            GetPlanParams: "get_plan", true, GET_PLAN_CALL,
                "Read plan metadata, body, and list task/note IDs.";
            UpdatePlanParams: "update_plan", false, UPDATE_PLAN_CALL,
                "Update plan body and metadata. Creates plan if it doesn't exist. Use content or replace_content to rewrite body, append_content to extend it, or replace_in_content for surgical edits. Provide at most one body-edit parameter. Optionally batch-create tasks. Keep each write under 1000 words.";
            DeletePlanParams: "delete_plan", false, DELETE_PLAN_CALL,
                "Delete an entire plan and all its tasks and notes.";
            ListTasksParams: "list_tasks", true, LIST_TASKS_CALL,
                "List tasks in a plan with optional filters.";
            AddTaskParams: "add_task", false, ADD_TASK_CALL,
                "Create a task in a plan. Keep body under 1000 words; use update_task with replace_in_body for targeted edits.";
            GetTaskParams: "get_task", true, GET_TASK_CALL,
                "Read a task by ID within a plan.";
            UpdateTaskParams: "update_task", false, UPDATE_TASK_CALL,
                "Update a task within its plan. Use replace_body to rewrite body, append_body to extend it, or replace_in_body for surgical edits. Keep each write under 1000 words.";
            DeleteTaskParams: "delete_task", false, DELETE_TASK_CALL,
                "Delete a task by ID.";
            ListNotesParams: "list_notes", true, LIST_NOTES_CALL,
                "List notes for a plan.";
            AddNoteParams: "add_note", false, ADD_NOTE_CALL,
                "Add a note to a plan. Keep body under 1000 words; use update_note with replace_in_body for targeted edits.";
            GetNoteParams: "get_note", true, GET_NOTE_CALL,
                "Read a note from a plan.";
            UpdateNoteParams: "update_note", false, UPDATE_NOTE_CALL,
                "Update a note within its plan. Use replace_body, append_body, or replace_in_body for body edits. Keep each write under 1000 words.";
            DeleteNoteParams: "delete_note", false, DELETE_NOTE_CALL,
                "Delete a note from a plan.";
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        dispatch_plan_tools!(
            self.server, tool, args;
            "list_plans" => no_args handle_list_plans, ListPlansParams;
            "add_plan" => with_args handle_add_plan, AddPlanParams;
            "get_plan" => with_args handle_get_plan, GetPlanParams;
            "update_plan" => with_args handle_update_plan, UpdatePlanParams;
            "delete_plan" => with_args handle_delete_plan, DeletePlanParams;
            "list_tasks" => with_args handle_list_tasks, ListTasksParams;
            "add_task" => with_args handle_add_task, AddTaskParams;
            "get_task" => with_args handle_get_task, GetTaskParams;
            "update_task" => with_args handle_update_task, UpdateTaskParams;
            "delete_task" => with_args handle_delete_task, DeleteTaskParams;
            "list_notes" => with_args handle_list_notes, ListNotesParams;
            "add_note" => with_args handle_add_note, AddNoteParams;
            "get_note" => with_args handle_get_note, GetNoteParams;
            "update_note" => with_args handle_update_note, UpdateNoteParams;
            "delete_note" => with_args handle_delete_note, DeleteNoteParams;
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_all_server_errors_to_recoverable() {
        let internal = map_result(Err(ErrorData::internal_error("server failed", None)));
        assert!(matches!(internal, Err(ToolInvokeError::Recoverable(_))));

        let invalid = map_result(Err(ErrorData::invalid_params("bad input", None)));
        assert!(matches!(invalid, Err(ToolInvokeError::Recoverable(_))));
    }

    #[test]
    fn exposes_all_plan_tools() {
        let dir = tempfile::tempdir().unwrap();
        let toolset = PlansToolset::new(dir.path().to_path_buf());
        assert_eq!(toolset.name(), "plans");
        let tools = toolset.tools();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [
                "list_plans",
                "add_plan",
                "get_plan",
                "update_plan",
                "delete_plan",
                "list_tasks",
                "add_task",
                "get_task",
                "update_task",
                "delete_task",
                "list_notes",
                "add_note",
                "get_note",
                "update_note",
                "delete_note",
            ]
        );
        assert!(tools
            .iter()
            .all(|tool| tool.input_schema["type"] == "object"));
    }

    #[tokio::test]
    async fn add_then_get_plan_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let toolset = PlansToolset::new(dir.path().to_path_buf());

        let added = toolset
            .invoke(
                "add_plan",
                json!({"name": "native-roundtrip", "title": "Native roundtrip", "content": "Stored through PlansToolset."}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_ne!(added["isError"], json!(true));

        let fetched = toolset
            .invoke(
                "get_plan",
                json!({"name": "native-roundtrip"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_ne!(fetched["isError"], json!(true));
        let text = fetched["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Native roundtrip"));
        assert!(text.contains("Stored through PlansToolset."));
    }

    #[tokio::test]
    async fn rejects_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let result = PlansToolset::new(dir.path().to_path_buf())
            .invoke("missing", json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolInvokeError::Recoverable(_))));
    }
}
