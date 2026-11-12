// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;
use crate::builtin_tool_specs;

fn domain_result(result: Result<CallToolResult, ErrorData>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(result) => Ok(result),
        Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
            error.message,
        )])),
    }
}

/// Request-timeout policy the native toolset advertises for `name`.
///
/// The MCP listing must carry the same policy so a bridged server
/// (`harnx-mcp-bridge` → NATS) inherits the native deadline behavior instead
/// of the caller's default backstop. MCP tools have no timeout field, so it
/// ships in `_meta` (see `call_template_meta`).
fn builtin_tool(spec: harnx_toolset::ToolSpec) -> Tool {
    let call_template = spec
        .meta
        .as_ref()
        .and_then(|meta| meta.get("call_template"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_schema = spec.input_schema.as_object().cloned().unwrap_or_default();
    Tool::new(spec.name.clone(), spec.description.clone(), input_schema)
        .with_meta(call_template_meta(call_template, spec.timeout_secs))
}

impl ServerHandler for BashServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-bash-tools",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Shell command MCP server with output truncation and retrievable temp logs.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Descriptions, schemas and templates come from the native specs so
        // the two routes cannot drift apart.
        let mut tools: Vec<Tool> = builtin_tool_specs().into_iter().map(builtin_tool).collect();
        tools.extend(self.tool_templates().map(|(name, registered)| {
            Tool::new(
                name.clone(),
                registered.description.clone(),
                registered.input_schema.clone(),
            )
            .with_meta(call_template_meta(
                &tool_templates::command_template_call(name),
                // Native command-template tools ship without a request
                // deadline (see `BashToolset::tools`); keep the routes in
                // policy parity.
                Some(0),
            ))
        }));
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.dispatch_call_tool(request, context)
            .await
            .map(Into::into)
    }

    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {}
}

impl BashServer {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let enabled = request_wants_execution_context(&request);
        let tool_name = request.name.to_string();
        let result = match tool_name.as_str() {
            tool if BUILTIN_TOOL_NAMES.contains(&tool) || self.has_tool_template(tool) => {
                domain_result(
                    self.invoke_tool_value(
                        tool,
                        Value::Object(request.arguments.unwrap_or_default()),
                    )
                    .await,
                )
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }?;
        Ok(finalize_direct_mcp_context(
            result,
            enabled,
            &tool_name,
            format!("{:?}", context.id),
        ))
    }
}

impl BashServer {
    pub(crate) async fn invoke_tool_value(
        &self,
        tool: &str,
        args: Value,
    ) -> Result<CallToolResult, ErrorData> {
        self.inner
            .spawned
            .lock()
            .await
            .evict(std::time::Instant::now());
        let observation_args = args.clone();
        let tracked_working_dir = self.tracked_working_dir(tool, &args).await;
        let arguments = args.as_object().cloned();
        let mut result = match tool {
            "exec" => {
                let params = parse_arguments::<ExecCommandParams>(arguments)?;
                self.exec_command_impl(params).await
            }
            "read_exec_log" => {
                let params = parse_arguments::<ReadExecLogParams>(arguments)?;
                self.read_exec_log_impl(params).await
            }
            "spawn" => {
                let params = parse_arguments::<SpawnCommandParams>(arguments)?;
                self.spawn_impl(params).await
            }
            "status" => {
                let params = parse_arguments::<StatusParams>(arguments)?;
                self.status_impl(params).await
            }
            "wait" => {
                let params = parse_arguments::<WaitParams>(arguments)?;
                self.wait_impl(params).await
            }
            "terminate" => {
                let params = parse_arguments::<TerminateParams>(arguments)?;
                self.terminate_impl(params).await
            }
            "rollback_file" => {
                let params = parse_arguments::<RollbackParams>(arguments)?;
                self.rollback_file_impl(params).await
            }
            template if self.has_tool_template(template) => {
                self.invoke_template(template, &arguments.unwrap_or_default())
                    .await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }?;
        if let Some(target) = self
            .execution_context_target(tool, &observation_args, tracked_working_dir)
            .await
        {
            self.attach_execution_context(&mut result, target).await;
        }
        Ok(result)
    }

    async fn tracked_working_dir(&self, tool: &str, args: &Value) -> Option<PathBuf> {
        if !matches!(tool, "wait" | "status" | "terminate") {
            return None;
        }
        let execution_id = args.get("execution_id").and_then(Value::as_str)?;
        self.inner
            .spawned
            .lock()
            .await
            .entries
            .get(execution_id)
            .map(|entry| entry.working_dir.clone())
    }

    async fn execution_context_target(
        &self,
        tool: &str,
        args: &Value,
        tracked_working_dir: Option<PathBuf>,
    ) -> Option<PathBuf> {
        match tool {
            "exec" | "spawn" => self
                .resolve_working_dir(args.get("working_dir").and_then(Value::as_str))
                .await
                .ok(),
            "wait" | "status" | "terminate" => tracked_working_dir,
            "rollback_file" => args
                .get("repo_path")
                .and_then(Value::as_str)
                .and_then(|path| validate_write_path(path, &self.inner.allowlist).ok()),
            "read_exec_log" => None,
            _ if self.has_tool_template(tool) => {
                let current_dir = std::env::current_dir().ok();
                self.inner
                    .allowlist
                    .default_read_directory(current_dir.as_deref())
            }
            _ => None,
        }
    }

    async fn attach_execution_context(&self, result: &mut CallToolResult, target: PathBuf) {
        let current_dir = std::env::current_dir().ok();
        let workspace = self
            .inner
            .allowlist
            .default_read_directory(current_dir.as_deref())
            .unwrap_or_else(|| target.clone());
        let observation = ExecutionContextObservation::observe_async(workspace, target).await;
        result.meta.get_or_insert_with(MetaObject::new).0.insert(
            EXECUTION_CONTEXT_NAMESPACE.to_string(),
            serde_json::to_value(observation).expect("execution context serializes"),
        );
    }
}
