use super::params::*;
use super::store::map_internal;
use async_nats::jetstream::{self, kv};
use harnx_blob_store::plans::get_document;
use harnx_blob_store::touch_activity;
use harnx_core::cid_url::CidUrl;
use harnx_toolset::SessionRef;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

mod note;
mod plan;
mod task;

pub(crate) struct OperationContext<'a> {
    pub(crate) store: &'a kv::Store,
    pub(crate) jetstream: &'a jetstream::Context,
    pub(crate) caller: Option<&'a SessionRef>,
}

impl OperationContext<'_> {
    pub(crate) async fn touch(&self, url: &CidUrl) {
        if let Err(error) = touch_activity(self.jetstream, &url.owner()).await {
            log::debug!("plan activity touch failed for {}: {error:#}", url.owner());
        }
    }
}

fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ErrorData> {
    serde_json::from_value(args).map_err(|error| {
        ErrorData::invalid_params(format!("invalid tool arguments: {error}"), None)
    })
}

fn domain_result(result: Result<CallToolResult, ErrorData>) -> CallToolResult {
    match result {
        Ok(result) => result,
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.message)]),
    }
}

type HandlerResult = Result<CallToolResult, ErrorData>;
type HandlerFuture<'a> = Pin<Box<dyn Future<Output = HandlerResult> + Send + 'a>>;
type Handler = for<'a> fn(&'a OperationContext<'a>, Value) -> HandlerFuture<'a>;

struct HandlerEntry {
    name: &'static str,
    invoke: Handler,
}

macro_rules! define_handlers {
    ($( $wrapper:ident: $name:literal, $params:ty => $handler:path; )+) => {
        $(
            fn $wrapper<'a>(
                context: &'a OperationContext<'a>,
                args: Value,
            ) -> HandlerFuture<'a> {
                Box::pin(async move {
                    $handler(context, parse_args::<$params>(args)?).await
                })
            }
        )+

        const HANDLERS: &[HandlerEntry] = &[
            $(HandlerEntry {
                name: $name,
                invoke: $wrapper,
            }),+
        ];
    };
}

define_handlers! {
    list_plans: "list_plans", ListPlansParams => plan::list;
    add_plan: "add_plan", AddPlanParams => plan::add;
    get_plan: "get_plan", GetPlanParams => plan::get;
    update_plan: "update_plan", UpdatePlanParams => plan::update;
    delete_plan: "delete_plan", DeletePlanParams => plan::delete;
    list_tasks: "list_tasks", ListTasksParams => task::list;
    add_task: "add_task", AddTaskParams => task::add;
    get_task: "get_task", GetTaskParams => task::get;
    update_task: "update_task", UpdateTaskParams => task::update;
    delete_task: "delete_task", DeleteTaskParams => task::delete;
    list_notes: "list_notes", ListNotesParams => note::list;
    add_note: "add_note", AddNoteParams => note::add;
    get_note: "get_note", GetNoteParams => note::get;
    update_note: "update_note", UpdateNoteParams => note::update;
    delete_note: "delete_note", DeleteNoteParams => note::delete;
}

pub(crate) async fn invoke(
    context: OperationContext<'_>,
    tool: &str,
    args: Value,
) -> Result<Value, harnx_toolset::ToolInvokeError> {
    let handler = HANDLERS
        .iter()
        .find(|handler| handler.name == tool)
        .ok_or_else(|| unknown_tool(tool))?;
    let result = (handler.invoke)(&context, args).await;
    serde_json::to_value(domain_result(result)).map_err(|error| {
        harnx_toolset::ToolInvokeError::Fatal(format!("serialize plans result: {error}"))
    })
}

async fn require_plan(context: &OperationContext<'_>, plan: &CidUrl) -> Result<(), ErrorData> {
    if get_document(context.store, plan)
        .await
        .map_err(map_internal)?
        .is_none()
    {
        return Err(ErrorData::invalid_params(
            format!("plan not found: {plan}"),
            None,
        ));
    }
    Ok(())
}

fn unknown_tool(tool: &str) -> harnx_toolset::ToolInvokeError {
    harnx_toolset::ToolInvokeError::Recoverable(format!("unknown plans tool: {tool}"))
}
