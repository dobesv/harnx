//! The fields of each mapped tool's script-style result, for `help <tool>`.

use harnx_toolset::{output_field, output_nullable_field};
use serde_json::{json, Value};

fn notes() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "what was cut off, ended the command or the wait",
    })
}

/// A finished command: `exec`, a command template, and the final state of a
/// spawned one.
fn finished_fields() -> Vec<(&'static str, Value)> {
    vec![
        (
            "stdout",
            output_field("string", "the command's standard output"),
        ),
        (
            "stderr",
            output_field("string", "the command's standard error"),
        ),
        (
            "exit_code",
            output_field("integer", "the command's exit status"),
        ),
        (
            "status",
            output_field("string", "exited, timeout or interrupted"),
        ),
        ("notes", notes()),
        (
            "execution_id",
            output_field("string", "id for wait, status and read_exec_log"),
        ),
        (
            "stdout_log_path",
            output_field("string", "file holding all of stdout"),
        ),
        (
            "stderr_log_path",
            output_field("string", "file holding all of stderr"),
        ),
        (
            "diff",
            output_field(
                "string",
                "what the command changed in git repositories under its working directory, as the diff option asks; only when something changed",
            ),
        ),
    ]
}

fn object_of(fields: Vec<(&'static str, Value)>) -> Value {
    let properties: serde_json::Map<String, Value> = fields
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect();
    json!({ "type": "object", "properties": properties })
}

pub(crate) fn exec_schema() -> Value {
    object_of(finished_fields())
}

/// `spawn`, `wait` and `status` answer with the finished fields once the
/// command has exited and with the status fields before that.
pub(crate) fn pending_schema() -> Value {
    let mut fields = finished_fields();
    for (name, schema) in &mut fields {
        match *name {
            "stdout" | "stderr" | "exit_code" => {
                let text = schema["description"].as_str().unwrap_or_default();
                schema["description"] = json!(format!("{text}, once the command has finished"));
            }
            "status" => {
                schema["description"] = json!("spawned, running, exited, timeout or interrupted");
            }
            _ => {}
        }
    }
    fields.extend([
        (
            "running_for_secs",
            output_field("integer", "how long a running command has run"),
        ),
        (
            "stopped_by",
            output_field(
                "string",
                "new_message or caller_deadline, when that ended the wait",
            ),
        ),
    ]);
    object_of(fields)
}

pub(crate) fn read_log_schema() -> Value {
    object_of(vec![
        ("text", output_field("string", "the log lines read")),
        ("notes", notes()),
        (
            "execution_id",
            output_field("string", "the execution the log belongs to"),
        ),
        ("stream", output_field("string", "stdout or stderr")),
        (
            "start_line",
            output_nullable_field("integer", "first line returned, 1-based"),
        ),
        (
            "end_line",
            output_nullable_field("integer", "last line returned"),
        ),
        (
            "total_lines",
            output_field("integer", "lines in the whole log"),
        ),
    ])
}
