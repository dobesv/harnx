use super::*;

fn apply_body_edit(
    mut current: String,
    replace: Option<String>,
    append: Option<String>,
    replace_in: Option<&ReplaceInContent>,
    error_message: &'static str,
) -> Result<String, ErrorData> {
    let body_count = [replace.is_some(), append.is_some(), replace_in.is_some()]
        .iter()
        .filter(|&&b| b)
        .count();
    if body_count > 1 {
        return Err(ErrorData::invalid_params(error_message, None));
    }

    if let Some(replace) = replace {
        current = replace;
    } else if let Some(append) = append {
        if !current.is_empty() && !current.ends_with('\n') {
            current.push('\n');
        }
        current.push_str(&append);
    } else if let Some(replace_in) = replace_in {
        current = apply_replace_in(&current, replace_in)?;
    }

    Ok(current)
}

fn result_with_diff(msg: String, diff: String) -> Result<CallToolResult, ErrorData> {
    if diff.is_empty() {
        result_text(msg)
    } else {
        result_text(format!("{msg}\n\n{diff}"))
    }
}

fn task_to_json(task: Task, body: String) -> Value {
    json!({
        "id": task.id,
        "title": task.title,
        "summary": task.summary,
        "author": task.author,
        "assignee": task.assignee,
        "executor": task.executor,
        "tags": task.tags,
        "plan": task.plan,
        "status": task.status,
        "created_at": timestamp_to_string(task.created_at),
        "updated_at": optional_timestamp_to_string(task.updated_at),
        "dependencies": task.dependencies,
        "body": body,
    })
}

fn note_to_json(note: Note, body: String) -> Value {
    json!({
        "id": display_note_id(&note.id),
        "summary": note.summary,
        "author": note.author,
        "created_at": timestamp_to_string(note.created_at),
        "updated_at": optional_timestamp_to_string(note.updated_at),
        "body": body,
    })
}

fn plan_summary_json(plan: Plan, task_count: usize, note_count: usize) -> Value {
    json!({
        "id": plan.id,
        "title": plan.title,
        "summary": plan.summary,
        "author": plan.author,
        "assignee": plan.assignee,
        "executor": plan.executor,
        "git_branch": plan.git_branch,
        "github_owner_repo": plan.github_owner_repo,
        "created_at": timestamp_to_string(plan.created_at),
        "updated_at": optional_timestamp_to_string(plan.updated_at),
        "task_count": task_count,
        "note_count": note_count,
    })
}

fn plan_detail_json(
    plan: Plan,
    body: String,
    task_ids: Vec<String>,
    note_ids: Vec<String>,
) -> Value {
    json!({
        "id": plan.id,
        "title": plan.title,
        "summary": plan.summary,
        "author": plan.author,
        "assignee": plan.assignee,
        "executor": plan.executor,
        "git_branch": plan.git_branch,
        "github_owner_repo": plan.github_owner_repo,
        "created_at": timestamp_to_string(plan.created_at),
        "updated_at": optional_timestamp_to_string(plan.updated_at),
        "body": body,
        "task_ids": task_ids,
        "note_ids": note_ids,
    })
}

fn task_filter_from_params(params: &ListTasksParams) -> TaskFilter {
    TaskFilter {
        status: match params.filter.as_str() {
            "all" => None,
            other => Some(other.to_string()),
        },
        tag: params.tag.clone(),
    }
}

fn map_task_not_found(plan_name: &str, task_id: &str, err: StoreError) -> ErrorData {
    match err {
        StoreError::NotFound => ErrorData::invalid_params(
            format!(
                "task {} not found in plan '{}'",
                display_id(task_id),
                plan_name
            ),
            None,
        ),
        other => store_error_to_error_data(other),
    }
}

fn map_note_not_found(plan_name: &str, note_id: &str, err: StoreError) -> ErrorData {
    match err {
        StoreError::NotFound => ErrorData::invalid_params(
            format!(
                "note {} not found in plan '{}'",
                display_note_id(note_id),
                plan_name
            ),
            None,
        ),
        other => store_error_to_error_data(other),
    }
}

async fn validate_batch_task_specs<S: PlanStore>(
    store: &S,
    target: &Target,
    name: &str,
    plan_id: &PlanId,
    task_specs: &[TaskSpec],
) -> Result<(), ErrorData> {
    let existing_tasks = store
        .list_tasks(
            target,
            plan_id,
            TaskFilter {
                status: None,
                tag: None,
            },
            None,
        )
        .await
        .map_err(store_error_to_error_data)?;
    let mut seen_ids = existing_tasks
        .items
        .into_iter()
        .map(|task| normalize_id(&task.id))
        .collect::<std::collections::BTreeSet<_>>();
    for spec in task_specs {
        if let Some(raw_id) = spec.id.as_deref() {
            let id = validate_id(raw_id).map_err(|err| ErrorData::invalid_params(err, None))?;
            if !seen_ids.insert(id.clone()) {
                return Err(ErrorData::invalid_params(
                    format!("task '{}' already exists in plan '{}'", id, name),
                    None,
                ));
            }
        }
    }
    Ok(())
}

fn build_new_task(_plan_name: &str, spec: TaskSpec) -> Result<(NewTask, String), ErrorData> {
    let id = spec
        .id
        .as_deref()
        .map(validate_id)
        .transpose()
        .map_err(|err| ErrorData::invalid_params(err, None))?
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    Ok((
        NewTask {
            id,
            title: spec.title,
            summary: spec.summary,
            author: spec.author,
            assignee: spec.assignee,
            executor: spec.executor,
            tags: spec.tags,
            status: spec.status,
            dependencies: spec.dependencies,
        },
        spec.body.unwrap_or_default(),
    ))
}

impl<S: PlanStore> PlansServer<S> {
    pub async fn handle_list_tasks(
        &self,
        params: ListTasksParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan_name.clone();
        let tasks = self
            .store
            .list_tasks(&target, &plan_id, task_filter_from_params(&params), None)
            .await
            .map_err(store_error_to_error_data)?;
        let mut tasks_json = Vec::new();
        for task in tasks.items {
            let body = self
                .store
                .read_task_body(&target, &plan_id, &task.id)
                .await
                .map_err(|err| map_task_not_found(&plan_name, &task.id, err))?;
            tasks_json.push(task_to_json(task, body));
        }
        result_json(Value::Array(tasks_json))
    }

    pub async fn handle_get_task(
        &self,
        params: GetTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let task_id =
            validate_id(&params.id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan_name.clone();
        let task = self
            .store
            .get_task(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        let body = self
            .store
            .read_task_body(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        result_json(task_to_json(task, body))
    }

    pub async fn handle_add_task(
        &self,
        params: AddTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan_name.clone();
        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let body = params.body.unwrap_or_default();
        let new_task = NewTask {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_string()),
            title: params.title,
            summary: params.summary,
            author: params.author,
            assignee: params.assignee,
            executor: params.executor,
            tags: params.tags,
            status: params.status,
            dependencies: params.dependencies,
        };
        let task = self
            .store
            .add_task(&target, &plan_id, new_task)
            .await
            .map_err(|err| match err {
                StoreError::AlreadyExists => ErrorData::invalid_params(
                    format!(
                        "task '{}' already exists in plan '{}'",
                        display_id(&body),
                        plan_name
                    ),
                    None,
                ),
                other => store_error_to_error_data(other),
            })?;
        self.store
            .write_task_body(&target, &plan_id, &task.id, &body)
            .await
            .map_err(store_error_to_error_data)?;
        let serialized = serde_yaml::to_string(&task_to_json(task.clone(), body.clone()))
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let diff = diff_text(
            "",
            &serialized,
            &format!("{}/tasks/{}.md", task.plan, task.id),
        );
        let msg = format!("added task {} to plan {}", display_id(&task.id), task.plan);
        result_with_diff(msg, diff)
    }

    pub async fn handle_update_task(
        &self,
        params: UpdateTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let task_id =
            validate_id(&params.id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan_name.clone();
        let task = self
            .store
            .get_task(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        let before_body = self
            .store
            .read_task_body(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        let body = apply_body_edit(
            before_body.clone(),
            params.replace_body,
            params.append_body,
            params.replace_in_body.as_ref(),
            "at most one of replace_body, append_body, replace_in_body may be provided",
        )?;
        let update = TaskMetaUpdate {
            title: params.title,
            summary: params.summary.or(task.summary),
            author: params.author.or(task.author),
            assignee: params.assignee.or(task.assignee),
            executor: params.executor.or(task.executor),
            tags: params.tags,
            status: params.status,
            dependencies: params.dependencies,
        };
        self.store
            .update_task_meta(&target, &plan_id, &task_id, update)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        self.store
            .write_task_body(&target, &plan_id, &task_id, &body)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        let diff = diff_text(
            &before_body,
            &body,
            &format!("{}/tasks/{}.md", plan_name, task_id),
        );
        let msg = format!("updated task {}", display_id(&task_id));
        result_with_diff(msg, diff)
    }

    pub async fn handle_delete_task(
        &self,
        params: DeleteTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let task_id =
            validate_id(&params.id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan_name.clone();
        let before_body = self
            .store
            .read_task_body(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        self.store
            .delete_task(&target, &plan_id, &task_id)
            .await
            .map_err(|err| map_task_not_found(&plan_name, &task_id, err))?;
        let after_body = self
            .store
            .read_task_body(&target, &plan_id, &task_id)
            .await
            .ok();
        match after_body {
            Some(_current_body) => result_text(format!(
                "left task {} unchanged (delete behavior is leave)",
                display_id(&task_id)
            )),
            None => {
                let diff = diff_text(
                    &before_body,
                    "",
                    &format!("{}/tasks/{}.md", plan_name, task_id),
                );
                result_with_diff(format!("deleted task {}", display_id(&task_id)), diff)
            }
        }
    }

    pub async fn handle_list_plans(
        &self,
        params: ListPlansParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plans = self
            .store
            .list_plans(&target, None)
            .await
            .map_err(store_error_to_error_data)?;
        let mut result = Vec::new();
        for plan in plans.items {
            let task_count = self
                .store
                .list_tasks(&target, &plan.id, TaskFilter::default(), None)
                .await
                .map_err(store_error_to_error_data)?
                .items
                .len();
            let note_count = self
                .store
                .list_notes(&target, &plan.id, None)
                .await
                .map_err(store_error_to_error_data)?
                .items
                .len();
            result.push(plan_summary_json(plan, task_count, note_count));
        }
        result_json(Value::Array(result))
    }

    pub async fn handle_add_plan(
        &self,
        params: AddPlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = name.clone();
        let body = params.body.unwrap_or_default();
        let plan = self
            .store
            .add_plan(
                &target,
                NewPlan {
                    id: plan_id.clone(),
                    title: params.title,
                    summary: params.summary,
                    author: params.author,
                    assignee: params.assignee,
                    executor: params.executor,
                    git_branch: params.git_branch,
                    github_owner_repo: params.github_owner_repo,
                },
            )
            .await
            .map_err(|err| match err {
                StoreError::AlreadyExists => {
                    ErrorData::invalid_params(format!("plan '{}' already exists", name), None)
                }
                other => store_error_to_error_data(other),
            })?;
        self.store
            .write_plan_body(&target, &plan_id, &body)
            .await
            .map_err(store_error_to_error_data)?;

        let task_specs = params.tasks.unwrap_or_default();
        validate_batch_task_specs(self.store.as_ref(), &target, &name, &plan_id, &task_specs)
            .await?;
        let mut created_task_ids = Vec::new();
        let mut task_failures = Vec::new();
        for spec in task_specs {
            match build_new_task(&name, spec) {
                Ok((new_task, task_body)) => {
                    match self.store.add_task(&target, &plan_id, new_task).await {
                        Ok(task) => {
                            if let Err(err) = self
                                .store
                                .write_task_body(&target, &plan_id, &task.id, &task_body)
                                .await
                            {
                                task_failures.push(format!(
                                    "{}: {}",
                                    display_id(&task.id),
                                    store_error_to_error_data(err).message
                                ));
                            } else {
                                created_task_ids.push(display_id(&task.id));
                            }
                        }
                        Err(err) => {
                            task_failures.push(store_error_to_error_data(err).message.to_string())
                        }
                    }
                }
                Err(err) => task_failures.push(err.message.to_string()),
            }
        }

        let diff = diff_text("", &body, &format!("{name}/plan.md"));
        let mut msg = format!("added plan {}", name);
        if !created_task_ids.is_empty() {
            msg.push_str(&format!(" and added tasks {}", created_task_ids.join(", ")));
        }
        if !task_failures.is_empty() {
            msg.push_str(&format!(
                "; task creation failures: {}",
                task_failures.join("; ")
            ));
        }
        let _ = plan;
        result_with_diff(msg, diff)
    }

    pub async fn handle_get_plan(
        &self,
        params: GetPlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = name.clone();
        let plan = self
            .store
            .get_plan(&target, &plan_id)
            .await
            .map_err(|err| match err {
                StoreError::NotFound => {
                    ErrorData::invalid_params(format!("plan '{}' not found", name), None)
                }
                other => store_error_to_error_data(other),
            })?;
        let body = self
            .store
            .read_plan_body(&target, &plan_id)
            .await
            .map_err(store_error_to_error_data)?;
        let task_ids = self
            .store
            .list_tasks(&target, &plan_id, TaskFilter::default(), None)
            .await
            .map_err(store_error_to_error_data)?
            .items
            .into_iter()
            .map(|task| display_id(&task.id))
            .collect();
        let note_ids = self
            .store
            .list_notes(&target, &plan_id, None)
            .await
            .map_err(store_error_to_error_data)?
            .items
            .into_iter()
            .map(|note| display_note_id(&note.id))
            .collect();
        result_json(plan_detail_json(plan, body, task_ids, note_ids))
    }

    pub async fn handle_update_plan(
        &self,
        params: UpdatePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = name.clone();

        let existing = match self.store.get_plan(&target, &plan_id).await {
            Ok(plan) => Some(plan),
            Err(StoreError::NotFound) => None,
            Err(err) => return Err(store_error_to_error_data(err)),
        };
        if existing.is_none() {
            self.store
                .add_plan(
                    &target,
                    NewPlan {
                        id: plan_id.clone(),
                        title: None,
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                        git_branch: None,
                        github_owner_repo: None,
                    },
                )
                .await
                .map_err(store_error_to_error_data)?;
        }
        let before_body = self
            .store
            .read_plan_body(&target, &plan_id)
            .await
            .unwrap_or_default();
        let new_body = apply_body_edit(
            before_body.clone(),
            params.replace_content,
            params.append_content,
            params.replace_in_content.as_ref(),
            "at most one of replace_content, append_content, replace_in_content may be provided",
        )?;
        self.store
            .update_plan_meta(
                &target,
                &plan_id,
                PlanMetaUpdate {
                    title: params
                        .title
                        .or(existing.as_ref().and_then(|p| p.title.clone())),
                    summary: params
                        .summary
                        .or(existing.as_ref().and_then(|p| p.summary.clone())),
                    author: params
                        .author
                        .or(existing.as_ref().and_then(|p| p.author.clone())),
                    assignee: params
                        .assignee
                        .or(existing.as_ref().and_then(|p| p.assignee.clone())),
                    executor: params
                        .executor
                        .or(existing.as_ref().and_then(|p| p.executor.clone())),
                    git_branch: params
                        .git_branch
                        .or(existing.as_ref().and_then(|p| p.git_branch.clone())),
                    github_owner_repo: params
                        .github_owner_repo
                        .or(existing.as_ref().and_then(|p| p.github_owner_repo.clone())),
                },
            )
            .await
            .map_err(store_error_to_error_data)?;
        self.store
            .write_plan_body(&target, &plan_id, &new_body)
            .await
            .map_err(store_error_to_error_data)?;

        let task_specs = params.tasks.unwrap_or_default();
        validate_batch_task_specs(self.store.as_ref(), &target, &name, &plan_id, &task_specs)
            .await?;
        let mut created_task_ids = Vec::new();
        let mut task_failures = Vec::new();
        for spec in task_specs {
            match build_new_task(&name, spec) {
                Ok((new_task, task_body)) => {
                    match self.store.add_task(&target, &plan_id, new_task).await {
                        Ok(task) => {
                            if let Err(err) = self
                                .store
                                .write_task_body(&target, &plan_id, &task.id, &task_body)
                                .await
                            {
                                task_failures.push(format!(
                                    "{}: {}",
                                    display_id(&task.id),
                                    store_error_to_error_data(err).message
                                ));
                            } else {
                                created_task_ids.push(display_id(&task.id));
                            }
                        }
                        Err(StoreError::AlreadyExists) => {
                            task_failures.push(format!("task already exists in plan '{}'", name))
                        }
                        Err(err) => {
                            task_failures.push(store_error_to_error_data(err).message.to_string())
                        }
                    }
                }
                Err(err) => task_failures.push(err.message.to_string()),
            }
        }

        let base_msg = if created_task_ids.is_empty() {
            format!("updated plan {}", name)
        } else {
            format!(
                "updated plan {} and added tasks {}",
                name,
                created_task_ids.join(", ")
            )
        };
        let msg = if task_failures.is_empty() {
            base_msg
        } else {
            format!(
                "{base_msg}; task creation failures: {}",
                task_failures.join("; ")
            )
        };
        let diff = diff_text(&before_body, &new_body, &format!("{name}/plan.md"));
        result_with_diff(msg, diff)
    }

    pub async fn handle_delete_plan(
        &self,
        params: DeletePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = name.clone();
        let plan_body =
            self.store
                .read_plan_body(&target, &plan_id)
                .await
                .map_err(|err| match err {
                    StoreError::NotFound => {
                        ErrorData::invalid_params(format!("plan '{}' not found", name), None)
                    }
                    other => store_error_to_error_data(other),
                })?;
        let mut diffs = vec![diff_text(&plan_body, "", &format!("{name}/plan.md"))];
        let tasks = self
            .store
            .list_tasks(
                &target,
                &plan_id,
                TaskFilter {
                    status: None,
                    tag: None,
                },
                None,
            )
            .await
            .map_err(store_error_to_error_data)?;
        for task in tasks.items {
            let body = self
                .store
                .read_task_body(&target, &plan_id, &task.id)
                .await
                .map_err(store_error_to_error_data)?;
            let diff = diff_text(&body, "", &format!("{name}/tasks/{}.md", task.id));
            if !diff.is_empty() {
                diffs.push(diff);
            }
        }
        let notes = self
            .store
            .list_notes(&target, &plan_id, None)
            .await
            .map_err(store_error_to_error_data)?;
        for note in notes.items {
            let body = self
                .store
                .read_note_body(&target, &plan_id, &note.id)
                .await
                .map_err(store_error_to_error_data)?;
            let diff = diff_text(&body, "", &format!("{name}/notes/{}.md", note.id));
            if !diff.is_empty() {
                diffs.push(diff);
            }
        }
        self.store
            .delete_plan(&target, &plan_id)
            .await
            .map_err(|err| match err {
                StoreError::NotFound => {
                    ErrorData::invalid_params(format!("plan '{}' not found", name), None)
                }
                other => store_error_to_error_data(other),
            })?;
        let after_body = self.store.read_plan_body(&target, &plan_id).await.ok();
        if after_body.is_some() {
            result_text(format!(
                "left plan {} unchanged (delete behavior is leave)",
                name
            ))
        } else {
            let msg = format!("deleted plan {}", name);
            if diffs.iter().all(String::is_empty) {
                result_text(msg)
            } else {
                result_text(format!(
                    "{}\n\n{}",
                    msg,
                    diffs
                        .into_iter()
                        .filter(|d| !d.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                ))
            }
        }
    }

    pub async fn handle_list_notes(
        &self,
        params: ListNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan.clone();
        let notes = self
            .store
            .list_notes(&target, &plan_id, None)
            .await
            .map_err(store_error_to_error_data)?;
        let mut result = Vec::new();
        for note in notes.items {
            let body = self
                .store
                .read_note_body(&target, &plan_id, &note.id)
                .await
                .map_err(|err| map_note_not_found(&plan, &note.id, err))?;
            result.push(note_to_json(note, body));
        }
        result_json(Value::Array(result))
    }

    pub async fn handle_add_note(
        &self,
        params: AddNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan.clone();
        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let note = self
            .store
            .add_note(
                &target,
                &plan_id,
                NewNote {
                    id: id.unwrap_or_else(|| {
                        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
                    }),
                    summary: params.summary,
                    author: params.author,
                },
            )
            .await
            .map_err(|err| match err {
                StoreError::AlreadyExists => ErrorData::invalid_params(
                    format!("note '{}' already exists in plan '{}'", "", plan),
                    None,
                ),
                other => store_error_to_error_data(other),
            })?;
        self.store
            .write_note_body(&target, &plan_id, &note.id, &params.body)
            .await
            .map_err(store_error_to_error_data)?;
        let diff = diff_text(
            "",
            &params.body,
            &format!("{}/notes/{}.md", plan, display_note_id(&note.id)),
        );
        let msg = format!("added note {} to plan {}", display_note_id(&note.id), plan);
        result_with_diff(msg, diff)
    }

    pub async fn handle_get_note(
        &self,
        params: GetNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let note_id =
            validate_id(&params.note_id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan.clone();
        let note = self
            .store
            .get_note(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        let body = self
            .store
            .read_note_body(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        result_json(note_to_json(note, body))
    }

    pub async fn handle_delete_note(
        &self,
        params: DeleteNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let note_id =
            validate_id(&params.note_id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan.clone();
        let before_body = self
            .store
            .read_note_body(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        self.store
            .delete_note(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        let after_body = self
            .store
            .read_note_body(&target, &plan_id, &note_id)
            .await
            .ok();
        match after_body {
            Some(_) => result_text(format!(
                "left note {} unchanged (delete behavior is leave)",
                display_note_id(&note_id)
            )),
            None => {
                let diff = diff_text(&before_body, "", &format!("{}/notes/{}.md", plan, note_id));
                result_with_diff(format!("deleted note {}", display_note_id(&note_id)), diff)
            }
        }
    }

    pub async fn handle_update_note(
        &self,
        params: UpdateNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let target = self.resolve_target(params.owner.as_deref(), params.repo.as_deref())?;
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let note_id =
            validate_id(&params.note_id).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_id: PlanId = plan.clone();
        let note = self
            .store
            .get_note(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        let before_body = self
            .store
            .read_note_body(&target, &plan_id, &note_id)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        let body = apply_body_edit(
            before_body.clone(),
            params.replace_body,
            params.append_body,
            params.replace_in_body.as_ref(),
            "at most one of replace_body, append_body, replace_in_body may be provided",
        )?;
        self.store
            .update_note_meta(
                &target,
                &plan_id,
                &note_id,
                NoteMetaUpdate {
                    summary: params.summary.or(note.summary),
                    author: params.author.or(note.author),
                },
            )
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        self.store
            .write_note_body(&target, &plan_id, &note_id, &body)
            .await
            .map_err(|err| map_note_not_found(&plan, &note_id, err))?;
        let diff = diff_text(
            &before_body,
            &body,
            &format!("{}/notes/{}.md", plan, note_id),
        );
        let msg = format!("updated note {}", display_note_id(&note_id));
        result_with_diff(msg, diff)
    }
}
