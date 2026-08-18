// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

impl ServerHandler for PlansServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-plans-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "File-based plan/task/note management server using markdown + YAML front matter",
            )
    }

    async fn list_tools(
        &self,
        _pagination: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
                Tool::new("list_plans", "List all plans with metadata and task/note counts.", Map::new())
                    .with_input_schema::<ListPlansParams>()
                    .with_meta(tool_meta(tool_templates::LIST_PLANS_CALL)),
                Tool::new("add_plan", "Create a new plan with optional metadata. Set body with content (or body for compatibility). Keep body content under 1000 words per call; use update_plan with replace_in_content for targeted edits.", Map::new())
                    .with_input_schema::<AddPlanParams>()
                    .with_meta(tool_meta(tool_templates::ADD_PLAN_CALL)),
                Tool::new("get_plan", "Read plan metadata, body, and list task/note IDs.", Map::new())
                    .with_input_schema::<GetPlanParams>()
                    .with_meta(tool_meta(tool_templates::GET_PLAN_CALL)),
                Tool::new("update_plan", "Update plan body and metadata. Creates plan if it doesn't exist. Use content or replace_content to rewrite body, append_content to extend it, or replace_in_content for surgical edits. Provide at most one body-edit parameter. Optionally batch-create tasks. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdatePlanParams>()
                    .with_meta(tool_meta(tool_templates::UPDATE_PLAN_CALL)),
                Tool::new("delete_plan", "Delete an entire plan and all its tasks and notes.", Map::new())
                    .with_input_schema::<DeletePlanParams>()
                    .with_meta(tool_meta(tool_templates::DELETE_PLAN_CALL)),
                Tool::new("list_tasks", "List tasks in a plan with optional filters.", Map::new())
                    .with_input_schema::<ListTasksParams>()
                    .with_meta(tool_meta(tool_templates::LIST_TASKS_CALL)),
                Tool::new("add_task", "Create a task in a plan. Keep body under 1000 words; use update_task with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddTaskParams>()
                    .with_meta(tool_meta(tool_templates::ADD_TASK_CALL)),
                Tool::new("get_task", "Read a task by ID within a plan.", Map::new())
                    .with_input_schema::<GetTaskParams>()
                    .with_meta(tool_meta(tool_templates::GET_TASK_CALL)),
                Tool::new("update_task", "Update a task within its plan. Use replace_body to rewrite body, append_body to extend it, or replace_in_body for surgical edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateTaskParams>()
                    .with_meta(tool_meta(tool_templates::UPDATE_TASK_CALL)),
                Tool::new("delete_task", "Delete a task by ID.", Map::new())
                    .with_input_schema::<DeleteTaskParams>()
                    .with_meta(tool_meta(tool_templates::DELETE_TASK_CALL)),
                Tool::new("list_notes", "List notes for a plan.", Map::new())
                    .with_input_schema::<ListNotesParams>()
                    .with_meta(tool_meta(tool_templates::LIST_NOTES_CALL)),
                Tool::new("add_note", "Add a note to a plan. Keep body under 1000 words; use update_note with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddNoteParams>()
                    .with_meta(tool_meta(tool_templates::ADD_NOTE_CALL)),
                Tool::new("get_note", "Read a note from a plan.", Map::new())
                    .with_input_schema::<GetNoteParams>()
                    .with_meta(tool_meta(tool_templates::GET_NOTE_CALL)),
                Tool::new("update_note", "Update a note within a plan. Use replace_body, append_body, or replace_in_body for body edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateNoteParams>()
                    .with_meta(tool_meta(tool_templates::UPDATE_NOTE_CALL)),
                Tool::new("delete_note", "Delete a note from a plan.", Map::new())
                    .with_input_schema::<DeleteNoteParams>()
                    .with_meta(tool_meta(tool_templates::DELETE_NOTE_CALL)),
            ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.dispatch_call_tool(request, _context)
            .await
            .map(Into::into)
    }
}

impl PlansServer {
    /// The tool dispatch itself, which always completes in one step.
    ///
    /// `call_tool` has to return `CallToolResponse`, whose other variants
    /// cover elicitation and long-running tasks that this server does not
    /// use. Keeping the dispatch on its own means every arm still returns a
    /// plain `CallToolResult`.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "list_plans" => {
                let _params = parse_arguments::<ListPlansParams>(request.arguments)?;
                self.handle_list_plans().await
            }
            "add_plan" => {
                let params = parse_arguments::<AddPlanParams>(request.arguments)?;
                self.handle_add_plan(params).await
            }
            "get_plan" => {
                let params = parse_arguments::<GetPlanParams>(request.arguments)?;
                self.handle_get_plan(params).await
            }
            "update_plan" => {
                let params = parse_arguments::<UpdatePlanParams>(request.arguments)?;
                self.handle_update_plan(params).await
            }
            "delete_plan" => {
                let params = parse_arguments::<DeletePlanParams>(request.arguments)?;
                self.handle_delete_plan(params).await
            }
            "list_tasks" => {
                let params = parse_arguments::<ListTasksParams>(request.arguments)?;
                self.handle_list_tasks(params).await
            }
            "add_task" => {
                let params = parse_arguments::<AddTaskParams>(request.arguments)?;
                self.handle_add_task(params).await
            }
            "get_task" => {
                let params = parse_arguments::<GetTaskParams>(request.arguments)?;
                self.handle_get_task(params).await
            }
            "update_task" => {
                let params = parse_arguments::<UpdateTaskParams>(request.arguments)?;
                self.handle_update_task(params).await
            }
            "delete_task" => {
                let params = parse_arguments::<DeleteTaskParams>(request.arguments)?;
                self.handle_delete_task(params).await
            }
            "list_notes" => {
                let params = parse_arguments::<ListNotesParams>(request.arguments)?;
                self.handle_list_notes(params).await
            }
            "add_note" => {
                let params = parse_arguments::<AddNoteParams>(request.arguments)?;
                self.handle_add_note(params).await
            }
            "get_note" => {
                let params = parse_arguments::<GetNoteParams>(request.arguments)?;
                self.handle_get_note(params).await
            }
            "update_note" => {
                let params = parse_arguments::<UpdateNoteParams>(request.arguments)?;
                self.handle_update_note(params).await
            }
            "delete_note" => {
                let params = parse_arguments::<DeleteNoteParams>(request.arguments)?;
                self.handle_delete_note(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}
