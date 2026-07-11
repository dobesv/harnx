// Auto-split from server.rs for cohesion. See server/mod.rs.
use super::*;

pub(crate) fn default_open_status() -> String {
    "open".to_string()
}

pub(crate) fn normalize_id(id: &str) -> String {
    let trimmed = id.trim();
    let trimmed = trimmed
        .strip_prefix("task-")
        .or_else(|| trimmed.strip_prefix("TASK-"))
        .or_else(|| trimmed.strip_prefix("note-"))
        .or_else(|| trimmed.strip_prefix("NOTE-"))
        .unwrap_or(trimmed);
    trimmed.to_ascii_lowercase()
}

pub(crate) fn normalize_plan_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(' ', "-")
}

pub(crate) fn validate_plan_name(name: &str) -> Result<String, String> {
    let normalized = normalize_plan_name(name);
    if normalized.is_empty() {
        return Err("plan name must not be empty".to_string());
    }
    if normalized.contains('/') || normalized.contains('\\') || normalized.contains("..") {
        return Err(format!(
            "plan name '{}' must not contain path separators or '..'",
            name
        ));
    }
    Ok(normalized)
}

pub(crate) fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (nanos & 0xffff_ffff) as u32)
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn validate_id(id: &str) -> Result<String, String> {
    let normalized = normalize_id(id);
    if normalized.is_empty()
        || normalized.len() > 64
        || !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Err(format!(
            "ID '{}' must contain only alphanumeric, hyphen, or underscore characters (1-64 chars)",
            id
        ))
    } else {
        Ok(normalized)
    }
}

pub(crate) fn display_id(id: &str) -> String {
    normalize_id(id)
}

pub(crate) fn display_note_id(id: &str) -> String {
    normalize_id(id)
}

pub(crate) fn result_json(value: Value) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
    result_text(text)
}

pub(crate) fn result_text(text: String) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

pub(crate) fn diff_text(before: &str, after: &str, path: &str) -> String {
    if before == after {
        return String::new();
    }
    let diff = TextDiff::from_lines(before, after);
    let mut output = format!("--- a/{path}\n+++ b/{path}\n");
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            let value = change.value();
            output.push_str(sign);
            output.push_str(value);
            if !value.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    format!("```diff\n{output}```")
}

pub(crate) fn apply_replace_in(body: &str, r: &ReplaceInContent) -> Result<String, ErrorData> {
    if r.old_text.is_empty() {
        return Err(ErrorData::invalid_params(
            "old_text must not be empty",
            None,
        ));
    }
    if !body.contains(&*r.old_text) {
        return Err(ErrorData::invalid_params(
            format!("old_text {:?} not found in body", r.old_text),
            None,
        ));
    }
    let result = if r.replace_all == Some(true) {
        body.replace(&*r.old_text, &r.new_text)
    } else {
        body.replacen(&*r.old_text, &r.new_text, 1)
    };
    Ok(result)
}

pub(crate) fn parse_arguments<T: serde::de::DeserializeOwned>(
    args: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(args.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(err.to_string(), None))
}

pub(crate) fn plan_dir(dir: &Path, plan_name: &str) -> PathBuf {
    dir.join(plan_name)
}

pub(crate) fn plan_file_path(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("plan.md")
}

pub(crate) fn tasks_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("tasks")
}

pub(crate) fn task_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    tasks_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}

pub(crate) fn notes_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("notes")
}

pub(crate) fn note_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    notes_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}

pub(crate) fn serialize_task(task: &TaskRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&task.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, task.body))
}

pub(crate) fn parse_task_frontmatter(content: &str) -> Result<(TaskFrontMatter, String), String> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| "missing YAML front matter".to_string())?;
    let (front, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| "missing YAML front matter terminator".to_string())?;
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

pub(crate) fn serialize_plan(record: &PlanRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&record.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, record.body))
}

pub(crate) fn parse_plan_frontmatter(
    content: &str,
    plan_name: &str,
) -> Result<(PlanFrontMatter, String), String> {
    let Some(rest) = content.strip_prefix("---\n") else {
        return Ok((
            PlanFrontMatter {
                id: plan_name.to_string(),
                created_at: "".to_string(),
                ..Default::default()
            },
            content.to_string(),
        ));
    };
    let Some((front, body)) = rest.split_once("\n---\n") else {
        return Err("missing YAML front matter terminator".to_string());
    };
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

pub(crate) fn serialize_note(record: &NoteRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&record.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, record.body))
}

pub(crate) fn parse_note_frontmatter(content: &str) -> Result<(NoteFrontMatter, String), String> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| "missing YAML front matter".to_string())?;
    let (front, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| "missing YAML front matter terminator".to_string())?;
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

pub(crate) fn write_task(dir: &Path, task: &TaskRecord) -> Result<(), String> {
    let tasks = tasks_dir(dir, &task.front.plan);
    std::fs::create_dir_all(&tasks).map_err(|err| err.to_string())?;
    let content = serialize_task(task)?;
    let final_path = task_file_path(dir, &task.front.plan, &task.front.id);
    let tmp_path = final_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn write_plan_file(path: &Path, content: &str) -> Result<(), String> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, path).map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn write_note(dir: &Path, plan_name: &str, note: &NoteRecord) -> Result<(), String> {
    let notes = notes_dir(dir, plan_name);
    std::fs::create_dir_all(&notes).map_err(|err| err.to_string())?;
    let content = serialize_note(note)?;
    let final_path = note_file_path(dir, plan_name, &note.front.id);
    let tmp_path = final_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn read_task(dir: &Path, plan_name: &str, id: &str) -> Result<TaskRecord, String> {
    let path = task_file_path(dir, plan_name, id);
    if !path.exists() {
        return Err(format!(
            "task {} not found in plan '{}'",
            display_id(id),
            plan_name
        ));
    }
    let content = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let (front, body) = parse_task_frontmatter(&content)?;
    Ok(TaskRecord { front, body })
}

pub(crate) fn list_tasks(
    dir: &Path,
    plan_filter: Option<&str>,
    tag_filter: Option<&str>,
    status_filter: Option<&str>,
) -> Vec<TaskRecord> {
    let mut tasks = Vec::new();
    let plans: Vec<String> = if let Some(plan) = plan_filter {
        vec![normalize_plan_name(plan)]
    } else {
        plan_dirs(dir)
            .into_iter()
            .filter_map(|path| path.file_name().and_then(OsStr::to_str).map(str::to_string))
            .collect()
    };

    for plan in plans {
        let tasks_path = tasks_dir(dir, &plan);
        let Ok(entries) = std::fs::read_dir(&tasks_path) else {
            continue;
        };
        let mut files = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
            .collect::<Vec<_>>();
        files.sort();
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok((front, body)) = parse_task_frontmatter(&content) else {
                continue;
            };
            if let Some(status) = status_filter {
                if front.status != status {
                    continue;
                }
            }
            if let Some(tag) = tag_filter {
                if !front.tags.iter().any(|candidate| candidate == tag) {
                    continue;
                }
            }
            tasks.push(TaskRecord { front, body });
        }
    }
    tasks
}

pub(crate) fn plan_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

pub(crate) fn plan_last_activity(plan_dir: &Path) -> std::io::Result<std::time::SystemTime> {
    let mut latest = None;

    let plan_file = plan_dir.join("plan.md");
    if let Ok(metadata) = std::fs::metadata(&plan_file) {
        latest = Some(metadata.modified()?);
    }

    for subdir in ["tasks", "notes"] {
        let dir = plan_dir.join(subdir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };

        for entry in entries {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(OsStr::to_str) != Some("md") {
                continue;
            }

            let modified = std::fs::metadata(&path)?.modified()?;
            latest = Some(match latest {
                Some(current) => current.max(modified),
                None => modified,
            });
        }
    }

    match latest {
        Some(modified) => Ok(modified),
        None => plan_dir.metadata()?.modified(),
    }
}

pub(crate) async fn run_cleanup_pass(dir: &Path, retention: Duration) {
    let dir_owned = dir.to_owned();
    let dirs = match tokio::task::spawn_blocking(move || plan_dirs(&dir_owned)).await {
        Ok(dirs) => dirs,
        Err(e) => {
            eprintln!("[cleanup] error listing plans: {e}");
            return;
        }
    };

    for plan_dir in dirs {
        let name = plan_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let plan_dir_for_activity = plan_dir.clone();
        let last_activity =
            match tokio::task::spawn_blocking(move || plan_last_activity(&plan_dir_for_activity))
                .await
            {
                Ok(Ok(last_activity)) => last_activity,
                Ok(Err(e)) => {
                    eprintln!("[cleanup] error checking plan {name}: {e}");
                    continue;
                }
                Err(e) => {
                    eprintln!("[cleanup] error checking plan {name}: {e}");
                    continue;
                }
            };

        let age = std::time::SystemTime::now()
            .duration_since(last_activity)
            .unwrap_or_default();
        if age <= retention {
            continue;
        }

        let plan_dir_for_delete = plan_dir.clone();
        match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(plan_dir_for_delete))
            .await
        {
            Ok(Ok(())) => {
                eprintln!(
                    "[cleanup] deleted inactive plan {name} (inactive for {} days)",
                    age.as_secs() / 86_400
                );
            }
            Ok(Err(e)) => eprintln!("[cleanup] error deleting plan {name}: {e}"),
            Err(e) => eprintln!("[cleanup] error deleting plan {name}: {e}"),
        }
    }
}

pub async fn cleanup_loop(dir: PathBuf, retention_days: u64) {
    let retention = Duration::from_secs(retention_days.saturating_mul(86_400));

    run_cleanup_pass(&dir, retention).await;

    let mut interval = tokio::time::interval(Duration::from_secs(86_400));
    interval.tick().await;

    loop {
        interval.tick().await;
        run_cleanup_pass(&dir, retention).await;
    }
}

pub(crate) fn list_note_ids(dir: &Path, plan_name: &str) -> Vec<String> {
    let notes = notes_dir(dir, plan_name);
    let Ok(entries) = std::fs::read_dir(notes) else {
        return Vec::new();
    };
    let mut ids = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(OsStr::to_str)
                .map(display_note_id)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}
