use crate::server::{
    EditFileParams, FindFilesParams, FsServer, InsertParams, ListDirectoryParams, ReReplaceParams,
    ReadFileParams, RollbackParams, SearchFilesParams, WriteFileParams,
};
use crate::tool_templates;
use async_trait::async_trait;
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use rmcp::model::{CallToolResult, ErrorData, Tool};
use rmcp::schemars::JsonSchema;
use serde_json::{Map, Value};
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

#[async_trait]
impl Toolset for FsToolset {
    fn name(&self) -> &str {
        "fs"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            spec::<ReadFileParams>(
                "read",
                "Read a text file with line numbers, pagination, grep filtering, and smart truncation. Prefer this tool over shell commands like sed, cat, head, tail. Use offset+limit to read specific line ranges instead of sed -n. Also reads local image files (PNG, JPEG, GIF, WebP, up to 5MB) and returns them as viewable images for vision-capable models — use this to view/inspect an image file by its path.",
                true,
                tool_templates::READ_CALL,
            ),
            spec::<WriteFileParams>(
                "write",
                "Write or create a file, replacing its contents.",
                false,
                tool_templates::WRITE_CALL,
            ),
            spec::<EditFileParams>(
                "edit",
                "Replace exact text within an existing file.",
                false,
                tool_templates::EDIT_CALL,
            ),
            spec::<InsertParams>(
                "insert",
                "Insert text into a file at a specific line position.      insert_line: 0 prepends before line 1; insert_line: N inserts after line N; omit insert_line (or set N = total lines) to append to the end of the file. Optional column (1-indexed byte offset within      the line, default 1 = start of line) for mid-line insertion.      For exact-text replacement use edit; for regex replacement use re_replace.",
                false,
                tool_templates::INSERT_CALL,
            ),
            spec::<ReReplaceParams>(
                "re_replace",
                "Replace text in a file using a regular expression.      Uses fancy_regex syntax (supports lookahead/lookbehind).      Use $0 for the full match, $1/$2 etc. for capture groups in replacement.      Errors if pattern matches nothing. If pattern matches more than once,      set replace_all=true; otherwise only the first match is replaced.      For exact-text replacement use edit instead.",
                false,
                tool_templates::RE_REPLACE_CALL,
            ),
            spec::<ListDirectoryParams>(
                "ls",
                "List directory contents, optionally recursively. Prefer this tool over running bash ls.",
                true,
                tool_templates::LS_CALL,
            ),
            spec::<SearchFilesParams>(
                "grep",
                "Search file contents with regex and optional context lines. Prefer this tool over running bash grep.",
                true,
                tool_templates::GREP_CALL,
            ),
            spec::<FindFilesParams>(
                "find",
                "Find files by glob pattern. Prefer this tool over running bash find.",
                true,
                tool_templates::FIND_CALL,
            ),
            spec::<RollbackParams>(
                "rollback_file",
                "Restore a repository to a prior harnx history snapshot. Pass the commit SHA from the 'commit <sha>' line at the top of a prior tool response's diff as the commit_id parameter.",
                false,
                tool_templates::ROLLBACK_FILE_CALL,
            ),
        ]
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
}
