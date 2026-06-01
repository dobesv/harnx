// Auto-split from server.rs for cohesion. See server/mod.rs.
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

fn delete_md_file(
    path: &Path,
    not_found_message: String,
    diff_path: String,
    deleted_message: String,
) -> Result<CallToolResult, ErrorData> {
    if !path.exists() {
        return Err(ErrorData::invalid_params(not_found_message, None));
    }
    let before_content = std::fs::read_to_string(path)
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
    std::fs::remove_file(path).map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
    let diff = diff_text(&before_content, "", &diff_path);
    result_with_diff(deleted_message, diff)
}

fn collect_dir_deletion_diffs(
    dir: &Path,
    plan: &str,
    subdir: &str,
    default_stem: &str,
) -> Vec<String> {
    let mut diffs = Vec::new();
    if dir.exists() {
        let mut files = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flat_map(|rd| rd.filter_map(Result::ok))
            .collect::<Vec<_>>();
        files.sort_by_key(|entry| entry.file_name());
        for entry in files {
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("md") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(default_stem);
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            diffs.push(diff_text(
                &before,
                "",
                &format!("{plan}/{subdir}/{stem}.md"),
            ));
        }
    }
    diffs
}

fn build_update_plan_tasks(
    dir: &Path,
    name: &str,
    task_specs: Option<Vec<TaskSpec>>,
) -> Result<Vec<TaskRecord>, ErrorData> {
    let Some(task_specs) = task_specs else {
        return Ok(Vec::new());
    };

    let existing_tasks = list_tasks(dir, Some(name), None, None);
    let mut seen_ids: Vec<String> = existing_tasks
        .iter()
        .map(|task| normalize_id(&task.front.id))
        .collect();
    for spec in &task_specs {
        if let Some(ref raw_id) = spec.id {
            let id = validate_id(raw_id).map_err(|err| ErrorData::invalid_params(err, None))?;
            if seen_ids.iter().any(|existing| existing == &id) {
                return Err(ErrorData::invalid_params(
                    format!("task '{}' already exists in plan '{}'", id, name),
                    None,
                ));
            }
            seen_ids.push(id);
        }
    }

    let mut task_records = Vec::new();
    for spec in task_specs {
        let id = spec
            .id
            .map(|raw| validate_id(&raw))
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?
            .unwrap_or_else(gen_id);
        task_records.push(TaskRecord {
            front: TaskFrontMatter {
                id,
                title: spec.title,
                summary: spec.summary,
                author: spec.author,
                assignee: spec.assignee,
                executor: spec.executor,
                tags: spec.tags,
                plan: name.to_owned(),
                status: spec.status.unwrap_or_else(default_open_status),
                created_at: now_iso(),
                updated_at: None,
                dependencies: spec.dependencies,
            },
            body: spec.body.unwrap_or_default(),
        });
    }

    Ok(task_records)
}

impl PlansServer {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) async fn handle_list_tasks(
        &self,
        params: ListTasksParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let status_filter = match params.filter.as_str() {
            "all" => None,
            other => Some(other),
        };
        let tasks = list_tasks(
            &self.dir,
            Some(&plan_name),
            params.tag.as_deref(),
            status_filter,
        );
        let tasks_json: Vec<Value> = tasks
            .iter()
            .map(|task| {
                serde_json::to_value(TaskWithBody {
                    front: &task.front,
                    body: &task.body,
                })
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))
            })
            .collect::<Result<_, _>>()?;
        result_json(Value::Array(tasks_json))
    }

    pub(crate) async fn handle_get_task(
        &self,
        params: GetTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let task = read_task(&self.dir, &plan_name, &params.id)
            .map_err(|e| ErrorData::invalid_params(e, None))?;

        result_json(
            serde_json::to_value(TaskWithBody {
                front: &task.front,
                body: &task.body,
            })
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?,
        )
    }

    pub(crate) async fn handle_add_task(
        &self,
        params: AddTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_path = plan_dir(&self.dir, &plan_name);
        if !plan_path.exists() {
            std::fs::create_dir_all(&plan_path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        }

        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?
            .unwrap_or_else(gen_id);
        if task_file_path(&self.dir, &plan_name, &id).exists() {
            return Err(ErrorData::invalid_params(
                format!("task '{}' already exists in plan '{}'", id, plan_name),
                None,
            ));
        }
        let now = now_iso();
        let task = TaskRecord {
            front: TaskFrontMatter {
                id: id.clone(),
                title: params.title,
                summary: params.summary,
                author: params.author,
                assignee: params.assignee,
                executor: params.executor,
                tags: params.tags,
                plan: plan_name.clone(),
                status: params.status.unwrap_or_else(default_open_status),
                created_at: now,
                updated_at: None,
                dependencies: params.dependencies,
            },
            body: params.body.unwrap_or_default(),
        };
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        let serialized =
            serialize_task(&task).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            "",
            &serialized,
            &format!("{}/tasks/{}.md", task.front.plan, task.front.id),
        );
        let msg = format!(
            "added task {} to plan {}",
            display_id(&task.front.id),
            task.front.plan
        );
        result_with_diff(msg, diff)
    }

    pub(crate) async fn handle_update_task(
        &self,
        params: UpdateTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let mut task = read_task(&self.dir, &plan_name, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;

        if let Some(title) = params.title {
            task.front.title = title;
        }
        if let Some(summary) = params.summary {
            task.front.summary = Some(summary);
        }
        if let Some(author) = params.author {
            task.front.author = Some(author);
        }
        if let Some(assignee) = params.assignee {
            task.front.assignee = Some(assignee);
        }
        if let Some(executor) = params.executor {
            task.front.executor = Some(executor);
        }
        if let Some(tags) = params.tags {
            task.front.tags = tags;
        }
        if let Some(status) = params.status {
            task.front.status = status;
        }

        // Body-edit: at most one of replace_body / append_body / replace_in_body
        let before_body = task.body.clone();
        task.body = apply_body_edit(
            task.body,
            params.replace_body,
            params.append_body,
            params.replace_in_body.as_ref(),
            "at most one of replace_body, append_body, replace_in_body may be provided",
        )?;

        if let Some(dependencies) = params.dependencies {
            task.front.dependencies = dependencies;
        }

        task.front.updated_at = Some(now_iso());

        let task_id = task.front.id.clone();
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            &before_body,
            &task.body,
            &format!("{}/tasks/{}.md", plan_name, task_id),
        );
        let msg = format!("updated task {}", display_id(&task_id));
        result_with_diff(msg, diff)
    }

    pub(crate) async fn handle_delete_task(
        &self,
        params: DeleteTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = task_file_path(&self.dir, &plan_name, &params.id);
        delete_md_file(
            &path,
            format!(
                "task {} not found in plan '{}'",
                display_id(&params.id),
                plan_name
            ),
            format!("{}/tasks/{}.md", plan_name, normalize_id(&params.id)),
            format!("deleted task {}", display_id(&params.id)),
        )
    }

    pub(crate) async fn handle_list_plans(&self) -> Result<CallToolResult, ErrorData> {
        let mut plans = Vec::new();
        for dir in plan_dirs(&self.dir) {
            let Some(name) = dir.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let normalized = normalize_plan_name(name);
            let plan_path = plan_file_path(&self.dir, &normalized);
            let record = if plan_path.exists() {
                let content = std::fs::read_to_string(&plan_path)
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                let (front, body) = parse_plan_frontmatter(&content, &normalized)
                    .map_err(|err| ErrorData::internal_error(err, None))?;
                PlanRecord { front, body }
            } else {
                PlanRecord {
                    front: PlanFrontMatter {
                        id: normalized.clone(),
                        created_at: String::new(),
                        ..Default::default()
                    },
                    body: String::new(),
                }
            };
            let task_ids = list_tasks(&self.dir, Some(&normalized), None, None)
                .into_iter()
                .map(|task| display_id(&task.front.id))
                .collect::<Vec<_>>();
            let note_ids = list_note_ids(&self.dir, &normalized);
            plans.push(json!({
                "id": record.front.id,
                "title": record.front.title,
                "summary": record.front.summary,
                "author": record.front.author,
                "assignee": record.front.assignee,
                "executor": record.front.executor,
                "git_branch": record.front.git_branch,
                "github_owner_repo": record.front.github_owner_repo,
                "created_at": record.front.created_at,
                "updated_at": record.front.updated_at,
                "task_count": task_ids.len(),
                "note_count": note_ids.len(),
            }));
        }
        result_json(Value::Array(plans))
    }

    pub(crate) async fn handle_add_plan(
        &self,
        params: AddPlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        if dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' already exists", name),
                None,
            ));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let record = PlanRecord {
            front: PlanFrontMatter {
                id: name.clone(),
                title: params.title,
                summary: params.summary,
                author: params.author,
                assignee: params.assignee,
                executor: params.executor,
                git_branch: params.git_branch,
                github_owner_repo: params.github_owner_repo,
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body.unwrap_or_default(),
        };
        let serialized =
            serialize_plan(&record).map_err(|err| ErrorData::internal_error(err, None))?;
        write_plan_file(&plan_file_path(&self.dir, &name), &serialized)
            .map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text("", &serialized, &format!("{name}/plan.md"));
        let msg = format!("added plan {}", name);
        result_with_diff(msg, diff)
    }

    pub(crate) async fn handle_get_plan(
        &self,
        params: GetPlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = plan_file_path(&self.dir, &name);
        let body = if path.exists() {
            std::fs::read_to_string(&path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?
        } else if plan_dir(&self.dir, &name).exists() {
            String::new()
        } else {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' not found", name),
                None,
            ));
        };
        let (front, content) = parse_plan_frontmatter(&body, &name)
            .map_err(|err| ErrorData::internal_error(err, None))?;
        let task_ids = list_tasks(&self.dir, Some(&name), None, None)
            .into_iter()
            .map(|task| display_id(&task.front.id))
            .collect::<Vec<_>>();
        let note_ids = list_note_ids(&self.dir, &name);
        result_json(json!({
            "id": front.id,
            "title": front.title,
            "summary": front.summary,
            "author": front.author,
            "assignee": front.assignee,
            "executor": front.executor,
            "git_branch": front.git_branch,
            "github_owner_repo": front.github_owner_repo,
            "created_at": front.created_at,
            "updated_at": front.updated_at,
            "body": content,
            "task_ids": task_ids,
            "note_ids": note_ids,
        }))
    }

    pub(crate) async fn handle_update_plan(
        &self,
        params: UpdatePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        std::fs::create_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let path = plan_file_path(&self.dir, &name);
        let (existing, existing_body) = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
            let (front, body) = parse_plan_frontmatter(&content, &name)
                .map_err(|err| ErrorData::internal_error(err, None))?;
            (front, body)
        } else {
            (
                PlanFrontMatter {
                    id: name.clone(),
                    created_at: now_iso(),
                    ..Default::default()
                },
                String::new(),
            )
        };

        // Body-edit: at most one of replace_content / append_content / replace_in_content
        let before_body = existing_body.clone();
        let new_body = apply_body_edit(
            existing_body,
            params.replace_content,
            params.append_content,
            params.replace_in_content.as_ref(),
            "at most one of replace_content, append_content, replace_in_content may be provided",
        )?;

        let record = PlanRecord {
            front: PlanFrontMatter {
                id: name.clone(),
                title: params.title.or(existing.title),
                summary: params.summary.or(existing.summary),
                author: params.author.or(existing.author),
                assignee: params.assignee.or(existing.assignee),
                executor: params.executor.or(existing.executor),
                git_branch: params.git_branch.or(existing.git_branch),
                github_owner_repo: params.github_owner_repo.or(existing.github_owner_repo),
                created_at: if existing.created_at.is_empty() {
                    now_iso()
                } else {
                    existing.created_at
                },
                updated_at: Some(now_iso()),
            },
            body: new_body.clone(),
        };

        let task_records = build_update_plan_tasks(&self.dir, &name, params.tasks)?;

        // Write plan.md first, then tasks — plan always reflects its own metadata
        let serialized =
            serialize_plan(&record).map_err(|err| ErrorData::internal_error(err, None))?;
        write_plan_file(&path, &serialized).map_err(|err| ErrorData::internal_error(err, None))?;

        let mut created_task_ids = Vec::new();
        for task in task_records {
            let id = task.front.id.clone();
            write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
            created_task_ids.push(display_id(&id));
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
        let diff = diff_text(&before_body, &new_body, &format!("{name}/plan.md"));
        result_with_diff(base_msg, diff)
    }

    pub(crate) async fn handle_delete_plan(
        &self,
        params: DeletePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        if !dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' not found", name),
                None,
            ));
        }
        let mut diffs = Vec::new();

        let plan_file = plan_file_path(&self.dir, &name);
        if plan_file.exists() {
            let content = std::fs::read_to_string(&plan_file).unwrap_or_default();
            let d = diff_text(&content, "", &format!("{name}/plan.md"));
            if !d.is_empty() {
                diffs.push(d);
            }
        }

        for diff in collect_dir_deletion_diffs(&tasks_dir(&self.dir, &name), &name, "tasks", "task")
        {
            if !diff.is_empty() {
                diffs.push(diff);
            }
        }

        for diff in collect_dir_deletion_diffs(&notes_dir(&self.dir, &name), &name, "notes", "note")
        {
            if !diff.is_empty() {
                diffs.push(diff);
            }
        }

        std::fs::remove_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let msg = format!("deleted plan {}", name);
        if diffs.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{}

{}",
                msg,
                diffs.join(
                    "

"
                )
            ))
        }
    }

    pub(crate) async fn handle_list_notes(
        &self,
        params: ListNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let mut notes = Vec::new();
        let dir = notes_dir(&self.dir, &plan);
        if dir.exists() {
            let mut entries = std::fs::read_dir(&dir)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                let content = std::fs::read_to_string(&path)
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                let (front, body) = parse_note_frontmatter(&content)
                    .map_err(|err| ErrorData::internal_error(err, None))?;
                notes.push(json!({
                    "id": display_note_id(&front.id),
                    "summary": front.summary,
                    "author": front.author,
                    "created_at": front.created_at,
                    "updated_at": front.updated_at,
                    "body": body,
                }));
            }
        }
        result_json(Value::Array(notes))
    }

    pub(crate) async fn handle_add_note(
        &self,
        params: AddNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        std::fs::create_dir_all(notes_dir(&self.dir, &plan))
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?
            .unwrap_or_else(gen_id);
        if note_file_path(&self.dir, &plan, &id).exists() {
            return Err(ErrorData::invalid_params(
                format!("note '{}' already exists in plan '{}'", id, plan),
                None,
            ));
        }
        let note = NoteRecord {
            front: NoteFrontMatter {
                id,
                summary: params.summary,
                author: params.author,
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body,
        };
        write_note(&self.dir, &plan, &note).map_err(|err| ErrorData::internal_error(err, None))?;
        let serialized =
            serialize_note(&note).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            "",
            &serialized,
            &format!("{}/notes/{}.md", plan, display_note_id(&note.front.id)),
        );
        let msg = format!(
            "added note {} to plan {}",
            display_note_id(&note.front.id),
            plan
        );
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{msg}

{diff}"
            ))
        }
    }

    pub(crate) async fn handle_get_note(
        &self,
        params: GetNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note {} not found in plan '{}'",
                    display_note_id(&params.note_id),
                    plan
                ),
                None,
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let (front, body) =
            parse_note_frontmatter(&content).map_err(|err| ErrorData::internal_error(err, None))?;
        result_json(json!({
            "id": display_note_id(&front.id),
            "summary": front.summary,
            "author": front.author,
            "created_at": front.created_at,
            "updated_at": front.updated_at,
            "body": body,
        }))
    }

    pub(crate) async fn handle_delete_note(
        &self,
        params: DeleteNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        delete_md_file(
            &path,
            format!(
                "note {} not found in plan '{}'",
                display_note_id(&params.note_id),
                plan
            ),
            format!("{}/notes/{}.md", plan, normalize_id(&params.note_id)),
            format!("deleted note {}", display_note_id(&params.note_id)),
        )
    }

    pub(crate) async fn handle_update_note(
        &self,
        params: UpdateNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note {} not found in plan '{}'",
                    display_note_id(&params.note_id),
                    plan
                ),
                None,
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let (mut front, mut body) =
            parse_note_frontmatter(&content).map_err(|err| ErrorData::internal_error(err, None))?;

        if let Some(summary) = params.summary {
            front.summary = Some(summary);
        }
        if let Some(author) = params.author {
            front.author = Some(author);
        }

        let before_body = body.clone();
        body = apply_body_edit(
            body,
            params.replace_body,
            params.append_body,
            params.replace_in_body.as_ref(),
            "at most one of replace_body, append_body, replace_in_body may be provided",
        )?;

        front.updated_at = Some(now_iso());
        let note = NoteRecord {
            front,
            body: body.clone(),
        };
        write_note(&self.dir, &plan, &note).map_err(|err| ErrorData::internal_error(err, None))?;

        let note_path = format!("{}/notes/{}.md", plan, normalize_id(&params.note_id));
        let diff = diff_text(&before_body, &body, &note_path);
        let msg = format!("updated note {}", display_note_id(&params.note_id));
        result_with_diff(msg, diff)
    }
}
