use crate::server::handlers::{self, OperationContext};
use crate::server::params::*;
use crate::tool_templates;
use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_nats_common::connect::NatsEndpoint;
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolProgressKind, ToolSpec, Toolset,
};
use rmcp::model::Tool;
use rmcp::schemars::JsonSchema;
use serde_json::{Map, Value};
use std::any::TypeId;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct PlansToolset {
    nats_url: Option<String>,
}

impl PlansToolset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_nats_url(url: impl Into<String>) -> Self {
        Self {
            nats_url: Some(url.into()),
        }
    }
}

async fn connect_nats(custom_url: Option<&str>) -> Result<(async_nats::Client, usize)> {
    match custom_url {
        Some(url) => Ok((
            async_nats::connect(url)
                .await
                .context("connect to NATS server")?,
            1,
        )),
        None => {
            let endpoint = NatsEndpoint::from_env()?;
            let replicas = endpoint.resolved_replicas();
            Ok((endpoint.connect().await?, replicas))
        }
    }
}

fn input_schema<T: JsonSchema + 'static>() -> Value {
    if TypeId::of::<T>() == TypeId::of::<()>() {
        return serde_json::json!({"type": "object", "properties": {}});
    }
    Tool::new("schema", "schema", Map::new())
        .with_input_schema::<T>()
        .schema_as_json_value()
}

fn tool_kind(name: &str) -> ToolProgressKind {
    if name.starts_with("get_") || name.starts_with("list_") {
        ToolProgressKind::Read
    } else if name.starts_with("add_") || name.starts_with("update_") {
        ToolProgressKind::Edit
    } else if name.starts_with("delete_") {
        ToolProgressKind::Delete
    } else {
        ToolProgressKind::Other
    }
}

struct SpecDef<'a> {
    name: &'a str,
    description: &'a str,
    read_only: bool,
    template: &'a str,
}

fn spec<T: JsonSchema + 'static>(definition: SpecDef<'_>) -> ToolSpec {
    let SpecDef {
        name,
        description,
        read_only,
        template,
    } = definition;
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_string(),
        description: description.to_string(),
        input_schema: input_schema::<T>(),
        idempotent_hint: false,
        read_only_hint: read_only,
        timeout_secs: None,
        meta: None,
    }
    .with_call_template(template)
    .with_result_template(tool_templates::RESULT)
    .with_kind(tool_kind(name))
}

macro_rules! plan_tool_specs {
    ($( $params:ty : $name:literal, $read_only:literal, $template:ident, $description:literal; )+) => {
        vec![$(
            spec::<$params>(SpecDef {
                name: $name,
                description: $description,
                read_only: $read_only,
                template: tool_templates::$template,
            }),
        )+]
    };
}

#[async_trait]
impl Toolset for PlansToolset {
    fn name(&self) -> &str {
        "plans"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3000
    }

    fn tools(&self) -> Vec<ToolSpec> {
        plan_tool_specs![
            ListPlansParams: "list_plans", true, LIST_PLANS_CALL,
                "List NATS-backed plans, optionally filtered by session owner key.";
            AddPlanParams: "add_plan", false, ADD_PLAN_CALL,
                "Create a plan owned by the caller session. Returns its cid:plan URL.";
            GetPlanParams: "get_plan", true, GET_PLAN_CALL,
                "Read a plan by full cid:plan URL.";
            UpdatePlanParams: "update_plan", false, UPDATE_PLAN_CALL,
                "Update a plan by full cid:plan URL. Body edits use revision CAS.";
            DeletePlanParams: "delete_plan", false, DELETE_PLAN_CALL,
                "Delete a plan and all task/note documents by full cid:plan URL.";
            ListTasksParams: "list_tasks", true, LIST_TASKS_CALL,
                "List tasks for a full cid:plan URL.";
            AddTaskParams: "add_task", false, ADD_TASK_CALL,
                "Create a task in a plan. Dependencies must be task URLs.";
            GetTaskParams: "get_task", true, GET_TASK_CALL,
                "Read a task. Plan and task ID parameters are full cid:plan URLs.";
            UpdateTaskParams: "update_task", false, UPDATE_TASK_CALL,
                "Update a task by full item URL using revision CAS.";
            DeleteTaskParams: "delete_task", false, DELETE_TASK_CALL,
                "Delete a task by full item URL.";
            ListNotesParams: "list_notes", true, LIST_NOTES_CALL,
                "List notes for a full cid:plan URL.";
            AddNoteParams: "add_note", false, ADD_NOTE_CALL,
                "Create a note in a plan and return its full item URL.";
            GetNoteParams: "get_note", true, GET_NOTE_CALL,
                "Read a note by full item URL.";
            UpdateNoteParams: "update_note", false, UPDATE_NOTE_CALL,
                "Update a note by full item URL using revision CAS.";
            DeleteNoteParams: "delete_note", false, DELETE_NOTE_CALL,
                "Delete a note by full item URL.";
        ]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.invoke_with_context(ToolInvocation {
            tool: tool.to_string(),
            args,
            context: ToolInvocationContext::default(),
            cancel,
        })
        .await
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        if invocation.tool == "add_plan" && invocation.context.invoking_session.is_none() {
            let result = rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(
                "caller session identity required to create plan",
            )]);
            return serde_json::to_value(result).map_err(|error| {
                ToolInvokeError::Fatal(format!("serialize plans result: {error}"))
            });
        }
        let (client, replicas) = connect_nats(self.nats_url.as_deref())
            .await
            .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        let jetstream = async_nats::jetstream::new(client);
        let store = harnx_blob_store::plans::ensure_plans_bucket(&jetstream, replicas)
            .await
            .map_err(|error| ToolInvokeError::Recoverable(error.to_string()))?;
        handlers::invoke(
            OperationContext {
                store: &store,
                jetstream: &jetstream,
                caller: invocation.context.invoking_session.as_ref(),
            },
            &invocation.tool,
            invocation.args,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_all_plan_tools_with_kinds() {
        let tools = PlansToolset::new().tools();
        assert_eq!(tools.len(), 15);
        assert!(tools
            .iter()
            .all(|tool| tool.input_schema["type"] == "object"));
        assert_eq!(tools[0].kind(), Some(ToolProgressKind::Read));
        assert_eq!(tools[1].kind(), Some(ToolProgressKind::Edit));
        assert_eq!(tools[4].kind(), Some(ToolProgressKind::Delete));
    }

    #[test]
    fn plan_schemas_expose_one_parameter_per_body_edit() {
        let tools = PlansToolset::new().tools();
        let properties = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name == name)
                .expect("tool exists")
                .input_schema["properties"]
                .clone()
        };
        let add = properties("add_plan");
        assert!(add.get("content").is_some());
        assert!(add.get("body").is_none());
        let update = properties("update_plan");
        assert!(update.get("replace_content").is_some());
        assert!(update.get("content").is_none());
    }
}
