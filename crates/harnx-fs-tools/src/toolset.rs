use crate::server::{
    EditFileParams, FindFilesParams, FsServer, InsertParams, ListDirectoryParams, ReReplaceParams,
    ReadFileParams, RollbackParams, SearchFilesParams, WriteFileParams,
};
use crate::tool_templates;
use async_trait::async_trait;
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::{
    ToolInvocation, ToolInvokeError, ToolProgressKind, ToolProgressLocation, ToolProgressPatch,
    ToolProgressStatus, ToolSpec, Toolset,
};
use rmcp::model::{CallToolResult, ErrorData, Tool};
use rmcp::schemars::JsonSchema;
use serde_json::{Map, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Toolset exposing the filesystem tools (read, write, edit, insert,
/// re_replace, ls, grep, find, rollback_file) backed by [`FsServer`].
/// Wraps the shared handler logic so the same `*_impl` methods serve both the
/// toolset path and the `--mcp-stdio` back-compat path.
#[derive(Clone)]
pub struct FsToolset {
    server: FsServer,
}

impl FsToolset {
    /// Build a toolset bounded to an immutable resolved allowlist.
    pub fn new(allowlist: ResolvedAllowlist) -> Self {
        Self {
            server: FsServer::new(allowlist),
        }
    }
}

fn input_schema<T: JsonSchema + 'static>() -> Value {
    Tool::new("schema", "schema", Map::new())
        .with_input_schema::<T>()
        .schema_as_json_value()
}

/// Build a spec carrying only `call_template`. Filesystem tools omit
/// `result_template` so the client keeps its audience-aware renderer, which is
/// what surfaces the history diff blocks mutating tools append to their output.
fn spec<T: JsonSchema + 'static>(
    name: &str,
    description: &str,
    read_only_hint: bool,
    call_template: &str,
) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: name.to_string(),
        description: description.to_string(),
        input_schema: input_schema::<T>(),
        idempotent_hint: false,
        read_only_hint,
        timeout_secs: None,
        meta: None,
    }
    .with_call_template(call_template)
}

fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|err| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {err}"))
        }),
        Err(err) => Err(ToolInvokeError::Recoverable(err.message.to_string())),
    }
}

fn progress_patch(server: &FsServer, tool: &str, args: &Value) -> Option<ToolProgressPatch> {
    let location = server.progress_location_for_args(tool, args)?;
    let path = location.display();
    let (title, kind) = match tool {
        "read" => (format!("Reading {path}"), ToolProgressKind::Read),
        "edit" => (format!("Editing {path}"), ToolProgressKind::Edit),
        "grep" => (
            format!(
                "Searching for {:?} in {path}",
                args.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
            ToolProgressKind::Search,
        ),
        "find" => (
            format!(
                "Finding {:?} in {path}",
                args.get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
            ToolProgressKind::Search,
        ),
        _ => return None,
    };
    Some(ToolProgressPatch {
        title: Some(title),
        status: Some(ToolProgressStatus::InProgress),
        kind: Some(kind),
        locations: Some(vec![ToolProgressLocation {
            path: location,
            line: None,
        }]),
        ..Default::default()
    })
}

fn discovered_locations(
    server: &FsServer,
    tool: &str,
    args: &Value,
    result: &CallToolResult,
) -> Option<ToolProgressPatch> {
    let root = server.progress_location_for_args(tool, args)?;
    let value = serde_json::to_value(result).ok()?;
    if value.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let text = value
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()?;
    let locations = match tool {
        "find" => find_locations(&root, text),
        "grep" => grep_locations(&root, text),
        _ => return None,
    };
    Some(ToolProgressPatch {
        locations: Some(locations),
        ..Default::default()
    })
}

fn find_locations(root: &Path, text: &str) -> Vec<ToolProgressLocation> {
    text.lines()
        .take_while(|line| !line.is_empty() && !line.starts_with('['))
        .filter(|line| !line.starts_with("No files found"))
        .map(|line| ToolProgressLocation {
            path: root.join(line),
            line: None,
        })
        .collect()
}

fn grep_locations(root: &Path, text: &str) -> Vec<ToolProgressLocation> {
    let mut locations = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((path, remainder)) = line.split_once(':') else {
            continue;
        };
        let Some((line_number, _)) = remainder.split_once(':') else {
            continue;
        };
        let Ok(line_number) = line_number.parse::<u32>() else {
            continue;
        };
        locations.entry(root.join(path)).or_insert(line_number);
    }
    locations
        .into_iter()
        .map(|(path, line)| ToolProgressLocation {
            path,
            line: Some(line),
        })
        .collect()
}
/// Canonical specifications for the filesystem tools.
pub fn builtin_tool_specs() -> Vec<ToolSpec> {
    vec![
        spec::<ReadFileParams>(
            "read",
            "Read a text file with line numbers, pagination, grep filtering, and smart truncation. Prefer this tool over shell commands like sed, cat, head, tail. Use offset+limit to read specific line ranges instead of sed -n. Also reads local image files (PNG, JPEG, GIF, WebP, up to 5MB) and returns them as viewable images for vision-capable models — use this to view/inspect an image file by its path.",
            true,
            tool_templates::READ_CALL,
        ),
        spec::<WriteFileParams>("write", "Write or create a file, replacing its contents.", false, tool_templates::WRITE_CALL),
        spec::<EditFileParams>("edit", "Replace exact text within an existing file.", false, tool_templates::EDIT_CALL),
        spec::<InsertParams>("insert", "Insert text into a file at a specific line position.      insert_line: 0 prepends before line 1; insert_line: N inserts after line N; omit insert_line (or set N = total lines) to append to the end of the file. Optional column (1-indexed byte offset within      the line, default 1 = start of line) for mid-line insertion.      For exact-text replacement use edit; for regex replacement use re_replace.", false, tool_templates::INSERT_CALL),
        spec::<ReReplaceParams>("re_replace", "Replace text in a file using a regular expression.      Uses fancy_regex syntax (supports lookahead/lookbehind).      Use $0 for the full match, $1/$2 etc. for capture groups in replacement.      Errors if pattern matches nothing. If pattern matches more than once,      set replace_all=true; otherwise only the first match is replaced.      For exact-text replacement use edit instead.", false, tool_templates::RE_REPLACE_CALL),
        spec::<ListDirectoryParams>("ls", "List directory contents, optionally recursively. Prefer this tool over running bash ls.", true, tool_templates::LS_CALL),
        spec::<SearchFilesParams>("grep", "Search file contents with regex and optional context lines. Prefer this tool over running bash grep.", true, tool_templates::GREP_CALL),
        spec::<FindFilesParams>("find", "Find files by glob pattern. Prefer this tool over running bash find.", true, tool_templates::FIND_CALL),
        spec::<RollbackParams>("rollback_file", "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.", false, tool_templates::ROLLBACK_FILE_CALL),
    ]
}

#[async_trait]
impl Toolset for FsToolset {
    fn name(&self) -> &str {
        "fs"
    }

    fn default_mcp_http_port(&self) -> u16 {
        3003
    }

    fn tools(&self) -> Vec<ToolSpec> {
        builtin_tool_specs()
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        _cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        let result = self.server.invoke_tool_value(tool, args).await;
        map_result(result)
    }

    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        if let Some(patch) = progress_patch(&self.server, &invocation.tool, &invocation.args) {
            invocation.context.progress.update(patch);
        }
        let result = self
            .server
            .invoke_tool_value(&invocation.tool, invocation.args.clone())
            .await;
        if let Ok(result) = &result {
            if let Some(patch) =
                discovered_locations(&self.server, &invocation.tool, &invocation.args, result)
            {
                invocation.context.progress.update(patch);
            }
        }
        map_result(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harnx-fs-toolset-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn assert_success_shape(result: &Value) {
        assert!(result.get("content").and_then(Value::as_array).is_some());
        assert_ne!(result.get("isError"), Some(&Value::Bool(true)));
    }

    async fn invoke(toolset: &FsToolset, tool: &str, args: Value) -> Value {
        let result = toolset
            .invoke(tool, args, CancellationToken::new())
            .await
            .unwrap();
        assert_success_shape(&result);
        assert!(result
            .get("_meta")
            .and_then(|meta| meta.get(harnx_core::execution_context::EXECUTION_CONTEXT_NAMESPACE))
            .is_some());
        result
    }

    async fn invoke_content_tools(toolset: &FsToolset, file_arg: &str, root_arg: &str) {
        invoke(
            toolset,
            "write",
            json!({"path": file_arg, "content": "one\n"}),
        )
        .await;
        invoke(toolset, "read", json!({"path": file_arg})).await;
        invoke(
            toolset,
            "edit",
            json!({"path": file_arg, "old_text": "one", "new_text": "two"}),
        )
        .await;
        invoke(
            toolset,
            "insert",
            json!({"path": file_arg, "insert_text": "three\n"}),
        )
        .await;
        invoke(
            toolset,
            "re_replace",
            json!({"path": file_arg, "pattern": "two", "replacement": "TWO"}),
        )
        .await;
        invoke(toolset, "ls", json!({"path": root_arg})).await;
        invoke(toolset, "grep", json!({"pattern": "TWO", "path": root_arg})).await;
        invoke(
            toolset,
            "find",
            json!({"pattern": "**/*.txt", "path": root_arg}),
        )
        .await;
    }

    async fn invoke_rollback(toolset: &FsToolset, file: &Path, root_arg: &str) {
        let before = toolset
            .server
            .snapshot_before(file, "before rollback test")
            .await;
        std::fs::write(file, "changed outside tool\n").unwrap();
        let diff = toolset
            .server
            .snapshot_after_diff(file, before, "after rollback test")
            .await
            .unwrap();
        let commit_id = diff
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("commit "))
            .unwrap();
        invoke(
            toolset,
            "rollback_file",
            json!({"commit_id": commit_id, "repo_path": root_arg}),
        )
        .await;
    }

    fn assert_tool_specs(toolset: &FsToolset) {
        let tools = toolset.tools();
        assert_eq!(tools.len(), 9);
        for tool in tools {
            assert_eq!(
                tool.read_only_hint,
                matches!(tool.name.as_str(), "read" | "ls" | "grep" | "find")
            );
            assert_eq!(tool.input_schema.get("type"), Some(&json!("object")));
        }
    }

    #[tokio::test]
    async fn invokes_all_fs_tools() {
        let root = TestDir::new();
        git(&root.0, &["init"]);
        git(&root.0, &["config", "user.name", "harnx test"]);
        git(&root.0, &["config", "user.email", "harnx@example.com"]);
        let root_path = root.0.canonicalize().unwrap();
        let file = root_path.join("sample.txt");
        let file_arg = file.to_string_lossy().into_owned();
        let root_arg = root_path.to_string_lossy().into_owned();
        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_rwx(root_path);
        let toolset = FsToolset::new(allowlist);

        invoke_content_tools(&toolset, &file_arg, &root_arg).await;
        invoke_rollback(&toolset, &file, &root_arg).await;
        assert_tool_specs(&toolset);
    }

    #[derive(Default)]
    struct RecordingProgress(std::sync::Mutex<Vec<ToolProgressPatch>>);

    impl harnx_toolset::ToolProgress for RecordingProgress {
        fn update(&self, patch: ToolProgressPatch) {
            self.0.lock().unwrap().push(patch);
        }
    }

    async fn invoke_with_progress(
        toolset: &FsToolset,
        recorder: &std::sync::Arc<RecordingProgress>,
        tool: &str,
        args: Value,
    ) {
        let result = toolset
            .invoke_with_context(ToolInvocation {
                tool: tool.to_string(),
                args,
                context: harnx_toolset::ToolInvocationContext {
                    progress: harnx_toolset::ToolProgressHandle::new(recorder.clone()),
                    ..Default::default()
                },
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        assert_success_shape(&result);
    }

    #[tokio::test]
    async fn native_read_search_find_and_edit_emit_resolved_progress() {
        let root = TestDir::new();
        let root_path = root.0.canonicalize().unwrap();
        let file = root_path.join("sample.txt");
        std::fs::write(&file, "one\n").unwrap();
        let file_arg = file.to_string_lossy().into_owned();
        let root_arg = root_path.to_string_lossy().into_owned();
        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_rwx(root_path);
        let toolset = FsToolset::new(allowlist);
        let recorder = std::sync::Arc::new(RecordingProgress::default());

        invoke_with_progress(
            &toolset,
            &recorder,
            "read",
            json!({"path": file_arg.clone()}),
        )
        .await;
        invoke_with_progress(
            &toolset,
            &recorder,
            "grep",
            json!({"pattern": "one", "path": root_arg.clone()}),
        )
        .await;
        invoke_with_progress(
            &toolset,
            &recorder,
            "find",
            json!({"pattern": "**/*.txt", "path": root_arg}),
        )
        .await;
        invoke_with_progress(
            &toolset,
            &recorder,
            "edit",
            json!({"path": file_arg, "old_text": "one", "new_text": "two"}),
        )
        .await;

        let patches = recorder.0.lock().unwrap();
        assert_eq!(patches.len(), 6);
        assert_eq!(patches[0].kind, Some(ToolProgressKind::Read));
        assert_eq!(patches[1].kind, Some(ToolProgressKind::Search));
        assert_eq!(patches[3].kind, Some(ToolProgressKind::Search));
        assert_eq!(patches[5].kind, Some(ToolProgressKind::Edit));
        assert_eq!(patches[2].locations.as_ref().unwrap()[0].line, Some(1));
        let reported_path = &patches[4].locations.as_ref().unwrap()[0].path;
        assert_eq!(
            reported_path.canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
        assert!([0, 1, 3, 5].into_iter().all(|index| {
            patches[index].status == Some(ToolProgressStatus::InProgress)
                && patches[index].locations.as_ref().is_some_and(|locations| {
                    locations.len() == 1 && locations[0].path.is_absolute()
                })
        }));
    }

    #[tokio::test]
    async fn zero_match_find_clears_locations_without_emitting_result_text_as_a_path() {
        let root = TestDir::new();
        let root_path = root.0.canonicalize().unwrap();
        let root_arg = root_path.to_string_lossy().into_owned();
        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_read(root_path);
        let toolset = FsToolset::new(allowlist);
        let recorder = std::sync::Arc::new(RecordingProgress::default());

        invoke_with_progress(
            &toolset,
            &recorder,
            "find",
            json!({"pattern": "**/*.missing", "path": root_arg}),
        )
        .await;

        let patches = recorder.0.lock().unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[1].locations, Some(Vec::new()));
        assert!(patches.iter().all(|patch| {
            patch.locations.as_ref().is_none_or(|locations| {
                locations
                    .iter()
                    .all(|location| !location.path.to_string_lossy().contains("No files found"))
            })
        }));
    }
}
