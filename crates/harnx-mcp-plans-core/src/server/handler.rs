use super::*;

pub fn serve_plans_server<S: PlanStore>(store: Arc<S>) -> PlansServer<S> {
    PlansServer::new(store)
}

pub fn serve_plans_server_with_meta<S: PlanStore>(
    store: Arc<S>,
    meta: ServerMeta,
) -> PlansServer<S> {
    PlansServer::with_meta(store, meta)
}

impl<S: PlanStore + 'static> ServerHandler for PlansServer<S> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                self.meta.name,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.meta.instructions)
    }

    async fn list_tools(
        &self,
        _pagination: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            tools: vec![
                Tool::new("list_plans", "List all plans with metadata and task/note counts.", Map::new())
                    .with_input_schema::<ListPlansParams>()
                    .with_meta(Meta(json!({"call_template": "list plans", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_plan", "Create a new plan with optional metadata. Keep body content under 1000 words per call; use update_plan with replace_in_content for targeted edits.", Map::new())
                    .with_input_schema::<AddPlanParams>()
                    .with_meta(Meta(json!({"call_template": "create plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_plan", "Read plan metadata, body, and list task/note IDs.", Map::new())
                    .with_input_schema::<GetPlanParams>()
                    .with_meta(Meta(json!({"call_template": "read plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_plan", "Update plan body and metadata. Creates plan if it doesn't exist. Use replace_content to rewrite body, append_content to extend it, or replace_in_content for surgical edits. Optionally batch-create tasks. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdatePlanParams>()
                    .with_meta(Meta(json!({"call_template": "update plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.tasks %} [{{ args.tasks | length }} tasks]{% endif %}{% if args.replace_content %}\n{{ args.replace_content | truncate(80) }}{% endif %}{% if args.append_content %}\n+{{ args.append_content | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_plan", "Delete an entire plan and all its tasks and notes.", Map::new())
                    .with_input_schema::<DeletePlanParams>()
                    .with_meta(Meta(json!({"call_template": "delete plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_tasks", "List tasks in a plan with optional filters.", Map::new())
                    .with_input_schema::<ListTasksParams>()
                    .with_meta(Meta(json!({"call_template": "list tasks {{ args.plan }}{% if args.filter and args.filter != 'open' %} [{{ args.filter }}]{% endif %}{% if args.tag %} #{{ args.tag }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_task", "Create a task in a plan. Keep body under 1000 words; use update_task with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddTaskParams>()
                    .with_meta(Meta(json!({"call_template": "create task {{ args.plan }}/{{ args.title }}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_task", "Read a task by ID within a plan.", Map::new())
                    .with_input_schema::<GetTaskParams>()
                    .with_meta(Meta(json!({"call_template": "read task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_task", "Update a task within its plan. Use replace_body to rewrite body, append_body to extend it, or replace_in_body for surgical edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateTaskParams>()
                    .with_meta(Meta(json!({"call_template": "update task {{ args.plan }}/{{ args.id }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.replace_body %}\n{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}\n+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_task", "Delete a task by ID.", Map::new())
                    .with_input_schema::<DeleteTaskParams>()
                    .with_meta(Meta(json!({"call_template": "delete task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_notes", "List notes for a plan.", Map::new())
                    .with_input_schema::<ListNotesParams>()
                    .with_meta(Meta(json!({"call_template": "list notes {{ args.plan }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_note", "Add a note to a plan. Keep body under 1000 words; use update_note with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddNoteParams>()
                    .with_meta(Meta(json!({"call_template": "add note {{ args.plan }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.author %} by {{ args.author }}{% endif %}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_note", "Read a note from a plan.", Map::new())
                    .with_input_schema::<GetNoteParams>()
                    .with_meta(Meta(json!({"call_template": "read note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_note", "Update a note within a plan. Use replace_body, append_body, or replace_in_body for body edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateNoteParams>()
                    .with_meta(Meta(json!({"call_template": "update note {{ args.plan }}/{{ args.note_id }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.replace_body %}\n{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}\n+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_note", "Delete a note from a plan.", Map::new())
                    .with_input_schema::<DeleteNoteParams>()
                    .with_meta(Meta(json!({"call_template": "delete note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
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
