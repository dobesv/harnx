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

fn metric_tool_name(tool: &str) -> &str {
    if matches!(
        tool,
        "list_plans"
            | "add_plan"
            | "get_plan"
            | "update_plan"
            | "delete_plan"
            | "list_tasks"
            | "add_task"
            | "get_task"
            | "update_task"
            | "delete_task"
            | "list_notes"
            | "add_note"
            | "get_note"
            | "update_note"
            | "delete_note"
    ) {
        tool
    } else {
        "unknown"
    }
}

fn tool_call_succeeded(result: &Result<CallToolResult, ErrorData>) -> bool {
    result
        .as_ref()
        .is_ok_and(|result| result.is_error != Some(true))
}

/// Converts a domain/inner error into an isError result.
///
/// This wrapper maps `Err(ErrorData)` to `Ok(CallToolResult::error(...))`,
/// ensuring recoverable failures are returned as `is_error: Some(true)`
/// instead of JSON-RPC error frames. Used at the `dispatch_call_tool`
/// boundary for known-tool arms.
fn domain_result(result: Result<CallToolResult, ErrorData>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(ok) => Ok(ok),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e.message)])),
    }
}

impl<S: PlanStore + 'static> ServerHandler for PlansServer<S> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                self.meta.name.clone(),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.meta.instructions.clone())
    }

    async fn list_tools(
        &self,
        _pagination: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![
                Tool::new("list_plans", "List all plans with metadata and task/note counts.", Map::new())
                    .with_input_schema::<ListPlansParams>()
                    .with_meta(MetaObject(json!({"call_template": "list plans", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_plan", "Create a new plan with optional metadata. Set body with content (or body for compatibility). Keep body content under 1000 words per call; use update_plan with replace_in_content for targeted edits.", Map::new())
                    .with_input_schema::<AddPlanParams>()
                    .with_meta(MetaObject(json!({"call_template": "create plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.content %}\n{{ args.content | truncate(80) }}{% elif args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_plan", "Read plan metadata, body, and list task/note IDs.", Map::new())
                    .with_input_schema::<GetPlanParams>()
                    .with_meta(MetaObject(json!({"call_template": "read plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_plan", "Update plan body and metadata. Creates plan if it doesn't exist. Use content or replace_content to rewrite body, append_content to extend it, or replace_in_content for surgical edits. Provide at most one body-edit parameter. Optionally batch-create tasks. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdatePlanParams>()
                    .with_meta(MetaObject(json!({"call_template": "update plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.tasks %} [{{ args.tasks | length }} tasks]{% endif %}{% if args.content %}\n{{ args.content | truncate(80) }}{% elif args.replace_content %}\n{{ args.replace_content | truncate(80) }}{% endif %}{% if args.append_content %}\n+{{ args.append_content | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_plan", "Delete an entire plan and all its tasks and notes.", Map::new())
                    .with_input_schema::<DeletePlanParams>()
                    .with_meta(MetaObject(json!({"call_template": "delete plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_tasks", "List tasks in a plan with optional filters.", Map::new())
                    .with_input_schema::<ListTasksParams>()
                    .with_meta(MetaObject(json!({"call_template": "list tasks {{ args.plan }}{% if args.filter and args.filter != 'open' %} [{{ args.filter }}]{% endif %}{% if args.tag %} #{{ args.tag }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_task", "Create a task in a plan. Keep body under 1000 words; use update_task with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddTaskParams>()
                    .with_meta(MetaObject(json!({"call_template": "create task {{ args.plan }}/{{ args.title }}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_task", "Read a task by ID within a plan.", Map::new())
                    .with_input_schema::<GetTaskParams>()
                    .with_meta(MetaObject(json!({"call_template": "read task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_task", "Update a task within its plan. Use replace_body to rewrite body, append_body to extend it, or replace_in_body for surgical edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateTaskParams>()
                    .with_meta(MetaObject(json!({"call_template": "update task {{ args.plan }}/{{ args.id }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.replace_body %}\n{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}\n+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_task", "Delete a task by ID.", Map::new())
                    .with_input_schema::<DeleteTaskParams>()
                    .with_meta(MetaObject(json!({"call_template": "delete task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_notes", "List notes for a plan.", Map::new())
                    .with_input_schema::<ListNotesParams>()
                    .with_meta(MetaObject(json!({"call_template": "list notes {{ args.plan }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_note", "Add a note to a plan. Keep body under 1000 words; use update_note with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddNoteParams>()
                    .with_meta(MetaObject(json!({"call_template": "add note {{ args.plan }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.author %} by {{ args.author }}{% endif %}{% if args.body %}\n{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_note", "Read a note from a plan.", Map::new())
                    .with_input_schema::<GetNoteParams>()
                    .with_meta(MetaObject(json!({"call_template": "read note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_note", "Update a note within a plan. Use replace_body, append_body, or replace_in_body for body edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateNoteParams>()
                    .with_meta(MetaObject(json!({"call_template": "update note {{ args.plan }}/{{ args.note_id }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.replace_body %}\n{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}\n+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_note", "Delete a note from a plan.", Map::new())
                    .with_input_schema::<DeleteNoteParams>()
                    .with_meta(MetaObject(json!({"call_template": "delete note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
            ];
        for tool in &mut tools {
            self.meta.target_policy.apply_to_tool_schema(tool);
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.clone();
        let metric_tool = metric_tool_name(&tool);
        let start = std::time::Instant::now();
        let result = self.dispatch_call_tool(request, _context).await;
        let elapsed = start.elapsed();
        let is_ok = tool_call_succeeded(&result);
        harnx_metrics::record_tool_call(metric_tool, is_ok, elapsed);
        result.map(Into::into)
    }
}

impl<S: PlanStore + 'static> PlansServer<S> {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "list_plans" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<ListPlansParams>(request.arguments)?;
                        self.handle_list_plans(params).await
                    }
                    .await,
                )
            }
            "add_plan" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<AddPlanParams>(request.arguments)?;
                        self.handle_add_plan(params).await
                    }
                    .await,
                )
            }
            "get_plan" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<GetPlanParams>(request.arguments)?;
                        self.handle_get_plan(params).await
                    }
                    .await,
                )
            }
            "update_plan" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<UpdatePlanParams>(request.arguments)?;
                        self.handle_update_plan(params).await
                    }
                    .await,
                )
            }
            "delete_plan" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<DeletePlanParams>(request.arguments)?;
                        self.handle_delete_plan(params).await
                    }
                    .await,
                )
            }
            "list_tasks" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<ListTasksParams>(request.arguments)?;
                        self.handle_list_tasks(params).await
                    }
                    .await,
                )
            }
            "add_task" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<AddTaskParams>(request.arguments)?;
                        self.handle_add_task(params).await
                    }
                    .await,
                )
            }
            "get_task" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<GetTaskParams>(request.arguments)?;
                        self.handle_get_task(params).await
                    }
                    .await,
                )
            }
            "update_task" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<UpdateTaskParams>(request.arguments)?;
                        self.handle_update_task(params).await
                    }
                    .await,
                )
            }
            "delete_task" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<DeleteTaskParams>(request.arguments)?;
                        self.handle_delete_task(params).await
                    }
                    .await,
                )
            }
            "list_notes" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<ListNotesParams>(request.arguments)?;
                        self.handle_list_notes(params).await
                    }
                    .await,
                )
            }
            "add_note" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<AddNoteParams>(request.arguments)?;
                        self.handle_add_note(params).await
                    }
                    .await,
                )
            }
            "get_note" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<GetNoteParams>(request.arguments)?;
                        self.handle_get_note(params).await
                    }
                    .await,
                )
            }
            "update_note" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<UpdateNoteParams>(request.arguments)?;
                        self.handle_update_note(params).await
                    }
                    .await,
                )
            }
            "delete_note" => {
                let request = request;
                domain_result(
                    async {
                        let params = parse_arguments::<DeleteNoteParams>(request.arguments)?;
                        self.handle_delete_note(params).await
                    }
                    .await,
                )
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Page, PageToken, RepoTarget};
    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams};
    use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
    use tokio::io::duplex;

    #[derive(Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                Implementation::new("test", "0.1"),
            )
        }
    }

    struct EmptyStore;

    #[async_trait::async_trait]
    impl PlanStore for EmptyStore {
        async fn list_plans(
            &self,
            _target: &Target,
            _page: Option<PageToken>,
        ) -> Result<Page<Plan>, StoreError> {
            unreachable!()
        }
        async fn get_plan(&self, _target: &Target, _plan: &PlanId) -> Result<Plan, StoreError> {
            unreachable!()
        }
        async fn add_plan(&self, _target: &Target, _new_plan: NewPlan) -> Result<Plan, StoreError> {
            unreachable!()
        }
        async fn update_plan_meta(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _update: PlanMetaUpdate,
        ) -> Result<Plan, StoreError> {
            unreachable!()
        }
        async fn delete_plan(&self, _target: &Target, _plan: &PlanId) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn read_plan_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
        ) -> Result<String, StoreError> {
            unreachable!()
        }
        async fn write_plan_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _body: &str,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn list_tasks(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _filter: TaskFilter,
            _page: Option<PageToken>,
        ) -> Result<Page<Task>, StoreError> {
            unreachable!()
        }
        async fn get_task(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _task: &crate::model::TaskId,
        ) -> Result<Task, StoreError> {
            unreachable!()
        }
        async fn add_task(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _new_task: NewTask,
        ) -> Result<Task, StoreError> {
            unreachable!()
        }
        async fn update_task_meta(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _task: &crate::model::TaskId,
            _update: TaskMetaUpdate,
        ) -> Result<Task, StoreError> {
            unreachable!()
        }
        async fn delete_task(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _task: &crate::model::TaskId,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn read_task_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _task: &crate::model::TaskId,
        ) -> Result<String, StoreError> {
            unreachable!()
        }
        async fn write_task_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _task: &crate::model::TaskId,
            _body: &str,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn list_notes(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _page: Option<PageToken>,
        ) -> Result<Page<Note>, StoreError> {
            unreachable!()
        }
        async fn get_note(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _note: &crate::model::NoteId,
        ) -> Result<Note, StoreError> {
            unreachable!()
        }
        async fn add_note(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _new_note: NewNote,
        ) -> Result<Note, StoreError> {
            unreachable!()
        }
        async fn update_note_meta(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _note: &crate::model::NoteId,
            _update: NoteMetaUpdate,
        ) -> Result<Note, StoreError> {
            unreachable!()
        }
        async fn delete_note(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _note: &crate::model::NoteId,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
        async fn read_note_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _note: &crate::model::NoteId,
        ) -> Result<String, StoreError> {
            unreachable!()
        }
        async fn write_note_body(
            &self,
            _target: &Target,
            _plan: &PlanId,
            _note: &crate::model::NoteId,
            _body: &str,
        ) -> Result<(), StoreError> {
            unreachable!()
        }
    }

    async fn list_tools_for(default_repo: Option<RepoTarget>) -> ListToolsResult {
        let (client_transport, server_transport) = duplex(65_536);
        let server = PlansServer::with_meta(
            Arc::new(EmptyStore),
            ServerMeta {
                name: "test".into(),
                instructions: "test".into(),
                target_policy: TargetPolicy::GitHub { default_repo },
            },
        );
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);
        type TestServerService = RunningService<RoleServer, PlansServer<EmptyStore>>;
        type TestClientService = RunningService<RoleClient, TestClientHandler>;
        let (server_res, client_res): (Result<TestServerService, _>, Result<TestClientService, _>) =
            tokio::join!(server_fut, client_fut);
        let _server = server_res.unwrap();
        let client = client_res.unwrap();
        let peer = client.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client.waiting().await;
        });
        peer.list_tools(Default::default()).await.unwrap()
    }

    fn required_for(tool: &Tool) -> Vec<String> {
        tool.input_schema
            .get("required")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn github_tool_schema_owner_repo_requiredness_tracks_default_repo() {
        let with_default = list_tools_for(Some(RepoTarget {
            owner: "acme".to_string(),
            repo: "plans".to_string(),
        }))
        .await;
        let list_plans = with_default
            .tools
            .iter()
            .find(|tool| tool.name == "list_plans")
            .unwrap();
        let required = required_for(list_plans);
        assert!(!required.contains(&"owner".to_string()));
        assert!(!required.contains(&"repo".to_string()));

        let without_default = list_tools_for(None).await;
        let list_plans = without_default
            .tools
            .iter()
            .find(|tool| tool.name == "list_plans")
            .unwrap();
        let required = required_for(list_plans);
        assert!(required.contains(&"owner".to_string()));
        assert!(required.contains(&"repo".to_string()));
    }

    #[tokio::test]
    async fn resolve_target_rejects_path_traversal_owner_and_repo() {
        let server = PlansServer::with_meta(
            Arc::new(EmptyStore),
            ServerMeta {
                name: "test".into(),
                instructions: "test".into(),
                target_policy: TargetPolicy::GitHub { default_repo: None },
            },
        );

        let repo_err = server
            .resolve_target(Some("acme"), Some("../../../user"))
            .expect_err("repo traversal should be rejected");
        assert!(repo_err.message.contains("GitHub repo"));
        assert!(repo_err.message.contains("path separators"));

        let owner_err = server
            .resolve_target(Some("../../../user"), Some("plans"))
            .expect_err("owner traversal should be rejected");
        assert!(owner_err.message.contains("GitHub owner"));
        assert!(owner_err.message.contains("path separators"));
    }

    #[tokio::test]
    async fn resolve_target_accepts_normal_repo_slug_with_dot() {
        let server = PlansServer::with_meta(
            Arc::new(EmptyStore),
            ServerMeta {
                name: "test".into(),
                instructions: "test".into(),
                target_policy: TargetPolicy::GitHub { default_repo: None },
            },
        );
        let target = server
            .resolve_target(Some("acme"), Some("plans.rs"))
            .expect("normal repo target should resolve");

        assert_eq!(
            target,
            Target::GitHub(RepoTarget {
                owner: "acme".to_string(),
                repo: "plans.rs".to_string(),
            })
        );
    }

    // Wire-level tests proving isError handling at the rmcp client boundary.
    mod wire_tests {
        use super::*;
        use crate::store::StoreError;
        use std::collections::BTreeMap;
        use std::sync::Mutex;

        /// A minimal mock store that can simulate failures for wire-level testing.
        struct MockStore {
            plans: Mutex<BTreeMap<String, Plan>>,
        }

        impl MockStore {
            fn new() -> Self {
                Self {
                    plans: Mutex::new(BTreeMap::new()),
                }
            }
        }

        #[async_trait::async_trait]
        impl PlanStore for MockStore {
            async fn list_plans(
                &self,
                _target: &Target,
                _page: Option<PageToken>,
            ) -> Result<Page<Plan>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_plan(&self, _target: &Target, plan: &PlanId) -> Result<Plan, StoreError> {
                let plans = self.plans.lock().unwrap();
                plans
                    .get(plan.as_str())
                    .cloned()
                    .ok_or(StoreError::NotFound)
            }

            async fn add_plan(
                &self,
                _target: &Target,
                new_plan: NewPlan,
            ) -> Result<Plan, StoreError> {
                let name = new_plan.id.clone();
                let plan = Plan {
                    id: name.clone(),
                    title: new_plan.title,
                    summary: new_plan.summary,
                    author: new_plan.author,
                    assignee: new_plan.assignee,
                    executor: new_plan.executor,
                    git_branch: new_plan.git_branch,
                    github_owner_repo: new_plan.github_owner_repo,
                    created_at: jiff::Timestamp::now(),
                    updated_at: None,
                };
                self.plans.lock().unwrap().insert(name, plan.clone());
                Ok(plan)
            }

            async fn update_plan_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _update: PlanMetaUpdate,
            ) -> Result<Plan, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_plan(
                &self,
                _target: &Target,
                _plan: &PlanId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_plan_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_plan_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Ok(())
            }

            async fn list_tasks(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _filter: TaskFilter,
                _page: Option<PageToken>,
            ) -> Result<Page<Task>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn add_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _new_task: NewTask,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn update_task_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
                _update: TaskMetaUpdate,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_task_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_task_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn list_notes(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _page: Option<PageToken>,
            ) -> Result<Page<Note>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn add_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _new_note: NewNote,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn update_note_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
                _update: crate::model::NoteMetaUpdate,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_note_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_note_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }
        }

        type MockServerService = RunningService<RoleServer, PlansServer<MockStore>>;
        type TestClientService = RunningService<RoleClient, TestClientHandler>;

        async fn setup_client_server() -> (TestClientService, MockServerService) {
            let (client_transport, server_transport) = duplex(65_536);
            let store = Arc::new(MockStore::new());
            let server = PlansServer::new(store);

            let server_fut = serve_server(server, server_transport);
            let client_fut = serve_client(TestClientHandler, client_transport);

            let (server_res, client_res): (
                Result<MockServerService, _>,
                Result<TestClientService, _>,
            ) = tokio::join!(server_fut, client_fut);

            let server = server_res.unwrap();
            let client = client_res.unwrap();
            (client, server)
        }

        /// Asserts that a known-tool domain failure returns `Ok` with `is_error: Some(true)`
        /// and that the content contains the expected message fragment.
        fn assert_is_error_result(
            result: &Result<CallToolResult, rmcp::service::ServiceError>,
            contains: &str,
        ) {
            match result {
                Ok(result) => {
                    assert!(
                        result.is_error == Some(true),
                        "expected is_error: Some(true), got {:?}",
                        result.is_error
                    );
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    assert!(
                        text.contains(contains),
                        "expected content to contain {:?}, got {:?}",
                        contains,
                        text
                    );
                }
                Err(e) => panic!("expected Ok result, got Err: {:?}", e),
            }
        }

        #[tokio::test]
        async fn get_missing_plan_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Try to get a plan that doesn't exist
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("get_plan").with_arguments(
                        json!({ "name": "nonexistent" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            assert_is_error_result(&result, "not found");

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn unknown_tool_returns_protocol_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Call a tool that doesn't exist
            let result = peer
                .call_tool(CallToolRequestParams::new("unknown_tool"))
                .await;

            // Unknown tool should return Err(ServiceError), not Ok(is_error)
            match result {
                Err(e) => {
                    // ServiceError should indicate invalid params or similar protocol error
                    assert!(
                        e.to_string().contains("unknown tool")
                            || e.to_string().contains("invalid")
                            || e.to_string().contains("InvalidParams"),
                        "expected protocol error for unknown tool, got: {:?}",
                        e
                    );
                }
                Ok(result) => {
                    panic!("expected Err for unknown tool, got Ok: {:?}", result);
                }
            }

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn get_missing_note_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Try to get a note from a non-existent plan
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("get_note").with_arguments(
                        json!({ "plan": "test-plan", "note_id": "missing-note" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            assert_is_error_result(&result, "not found");

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn get_missing_task_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Try to get a task from a non-existent plan
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("get_task").with_arguments(
                        json!({ "plan": "test-plan", "id": "missing-task" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            assert_is_error_result(&result, "not found");

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn successful_call_returns_ok_without_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Create a plan (should succeed)
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("add_plan").with_arguments(
                        json!({ "name": "test-plan", "title": "Test Plan" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            match result {
                Ok(result) => {
                    // Successful calls should NOT have is_error set to true
                    assert!(
                        result.is_error != Some(true),
                        "successful call should not have is_error: true"
                    );
                }
                Err(e) => {
                    panic!("successful call should return Ok, got Err: {:?}", e);
                }
            }

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn invalid_arguments_returns_is_error_not_protocol_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Call a known tool with missing required argument (add_plan needs 'name')
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("add_plan").with_arguments(
                        json!({ "title": "Missing name field" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            // Argument validation errors should return Ok with is_error: true, NOT Err
            assert_is_error_result(&result, "missing");

            // Cleanup
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn empty_arguments_returns_is_error() {
            let (client, _server) = setup_client_server().await;
            let peer = client.peer();

            // Call a known tool with empty arguments (add_plan needs 'name')
            let result = peer.call_tool(CallToolRequestParams::new("add_plan")).await;

            // Argument validation errors should return Ok with is_error: true
            assert_is_error_result(&result, "missing");

            // Cleanup
            client.cancel().await.unwrap();
        }

        /// Mock store that returns RateLimited for get_plan operations.
        struct RateLimitedMockStore;

        #[async_trait::async_trait]
        impl PlanStore for RateLimitedMockStore {
            async fn list_plans(
                &self,
                _target: &Target,
                _page: Option<PageToken>,
            ) -> Result<Page<Plan>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_plan(&self, _target: &Target, _plan: &PlanId) -> Result<Plan, StoreError> {
                Err(StoreError::RateLimited {
                    retry_after_secs: 30,
                })
            }

            async fn add_plan(
                &self,
                _target: &Target,
                _new_plan: NewPlan,
            ) -> Result<Plan, StoreError> {
                Err(StoreError::RateLimited {
                    retry_after_secs: 30,
                })
            }

            async fn update_plan_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _update: PlanMetaUpdate,
            ) -> Result<Plan, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_plan(
                &self,
                _target: &Target,
                _plan: &PlanId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_plan_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_plan_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn list_tasks(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _filter: TaskFilter,
                _page: Option<PageToken>,
            ) -> Result<Page<Task>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn add_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _new_task: NewTask,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn update_task_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
                _update: TaskMetaUpdate,
            ) -> Result<Task, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_task(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_task_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_task_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _task: &crate::model::TaskId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn list_notes(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _page: Option<PageToken>,
            ) -> Result<Page<Note>, StoreError> {
                Ok(Page {
                    items: vec![],
                    next: None,
                })
            }

            async fn get_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn add_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _new_note: NewNote,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn update_note_meta(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
                _update: crate::model::NoteMetaUpdate,
            ) -> Result<Note, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn delete_note(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }

            async fn read_note_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
            ) -> Result<String, StoreError> {
                Err(StoreError::NotFound)
            }

            async fn write_note_body(
                &self,
                _target: &Target,
                _plan: &PlanId,
                _note: &crate::model::NoteId,
                _body: &str,
            ) -> Result<(), StoreError> {
                Err(StoreError::NotFound)
            }
        }

        type RateLimitedServerService =
            RunningService<RoleServer, PlansServer<RateLimitedMockStore>>;
        type RateLimitedClientService = RunningService<RoleClient, TestClientHandler>;

        async fn setup_rate_limited_client_server(
        ) -> (RateLimitedClientService, RateLimitedServerService) {
            let (client_transport, server_transport) = duplex(65_536);
            let store = Arc::new(RateLimitedMockStore);
            let server = PlansServer::new(store);

            let server_fut = serve_server(server, server_transport);
            let client_fut = serve_client(TestClientHandler, client_transport);

            let (server_res, client_res): (
                Result<RateLimitedServerService, _>,
                Result<RateLimitedClientService, _>,
            ) = tokio::join!(server_fut, client_fut);

            let server = server_res.unwrap();
            let client = client_res.unwrap();
            (client, server)
        }

        #[tokio::test]
        async fn rate_limited_error_returns_is_error_with_retry_guidance() {
            let (client, _server) = setup_rate_limited_client_server().await;
            let peer = client.peer();

            // Try to add a plan - the mock store will return RateLimited
            let result = peer
                .call_tool(
                    CallToolRequestParams::new("add_plan").with_arguments(
                        json!({ "name": "test-plan", "title": "Test Plan" })
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await;

            // RateLimited should return Ok with is_error: true, NOT a protocol error
            assert_is_error_result(&result, "rate limited");

            // Verify the retry guidance is present
            match result {
                Ok(result) => {
                    assert!(
                        result.is_error == Some(true),
                        "expected is_error: Some(true), got {:?}",
                        result.is_error
                    );
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    assert!(
                        text.contains("retry after 30s"),
                        "expected content to contain retry guidance, got {:?}",
                        text
                    );
                }
                Err(e) => {
                    panic!(
                        "RateLimited should return Ok(CallToolResult), got Err: {:?}",
                        e
                    );
                }
            }

            // Cleanup
            client.cancel().await.unwrap();
        }
    }
}
