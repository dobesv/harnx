//! The fields of each mapped tool's script-style result, for `help <tool>`.

use harnx_toolset::output_field;
use harnx_toolset::ScriptOutput;
use serde_json::{json, Value};

/// `ls` and `find` print one item per line and report cut-off output as notes.
pub(crate) fn list_output(stdout: &str, summary: &str) -> ScriptOutput {
    ScriptOutput {
        stdout: Some(stdout.to_string()),
        stderr: Some("notes".to_string()),
        summary: Some(summary.to_string()),
        ..Default::default()
    }
}

fn list_of(kind: &str, description: &str) -> Value {
    json!({ "type": "array", "items": { "type": kind }, "description": description })
}

pub(crate) fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": output_field("string", "the selected file text"),
            "notes": list_of("string", "what was cut off and how to continue"),
            "path": output_field("string", "the file that was read"),
            "total_lines": output_field("integer", "lines in the whole file"),
            "start_line": output_field("integer", "first line returned, 1-based"),
            "end_line": output_field("integer", "last line returned"),
        }
    })
}

pub(crate) fn ls_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "entries": list_of("string", "directory entries, one per item"),
            "notes": list_of("string", "what was cut off"),
        }
    })
}

pub(crate) fn find_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": list_of("string", "matching paths, usable as --path"),
            "notes": list_of("string", "what was cut off"),
        }
    })
}

pub(crate) fn grep_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "matches": {
                "type": "array",
                "items": { "type": "object" },
                "description": "rows with path, line, text and context (true for a context line)",
            },
            "notes": list_of("string", "what was cut off"),
            "exit_code": output_field("integer", "0 when something matched, 1 when nothing did"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builtin_tool_specs, FsToolset};
    use harnx_tool_allow::ResolvedAllowlist;
    use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset};
    use tokio_util::sync::CancellationToken;

    fn declared(name: &str) -> Value {
        builtin_tool_specs()
            .into_iter()
            .find(|spec| spec.name == name)
            .and_then(|spec| spec.output_schema().cloned())
            .unwrap_or_else(|| panic!("{name} declares no output schema"))
    }

    fn property_names(schema: &Value) -> Vec<String> {
        schema["properties"]
            .as_object()
            .expect("schema has properties")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn mapped_tools_declare_their_fields() {
        for spec in builtin_tool_specs() {
            let Some(output) = spec.script_output() else {
                assert!(spec.output_schema().is_none(), "{}", spec.name);
                continue;
            };
            let names = property_names(&declared(&spec.name));
            for named in [&output.stdout, &output.stderr, &output.exit_code]
                .into_iter()
                .flatten()
            {
                assert!(names.contains(named), "{} lacks {named}", spec.name);
            }
        }
    }

    async fn script_fields(toolset: &FsToolset, tool: &str, args: Value) -> Vec<String> {
        let mut context = ToolInvocationContext::default();
        context
            .capabilities
            .insert(harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_string());
        let result = toolset
            .invoke_with_context(ToolInvocation {
                tool: tool.to_string(),
                args,
                context,
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        result["structuredContent"]
            .as_object()
            .unwrap_or_else(|| panic!("{tool} returned no structured content"))
            .keys()
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn script_result_fields_match_the_declared_schema() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        std::fs::write(root_path.join("a.txt"), "one\ntwo\n").unwrap();
        let dir = root_path.to_string_lossy().into_owned();
        let file = root_path.join("a.txt").to_string_lossy().into_owned();
        let mut allowlist = ResolvedAllowlist::new();
        allowlist.insert_rwx(root_path);
        let toolset = FsToolset::new(allowlist);
        let calls = [
            ("read", json!({"path": file})),
            ("ls", json!({"path": dir})),
            ("find", json!({"pattern": "*.txt", "path": dir})),
            ("grep", json!({"pattern": "one", "path": dir, "context": 1})),
        ];
        for (tool, args) in calls {
            let allowed = property_names(&declared(tool));
            for key in script_fields(&toolset, tool, args).await {
                assert!(allowed.contains(&key), "{tool} emits undeclared {key}");
            }
        }
    }
}
