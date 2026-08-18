//! Specs the time toolset advertises, one builder per tool.

use crate::tool_templates;
use harnx_toolset::ToolSpec;
use serde_json::json;

pub(crate) fn all() -> Vec<ToolSpec> {
    vec![get_current_time(), convert_time(), wait(), wait_until()]
}

fn get_current_time() -> ToolSpec {
    ToolSpec {
        name: "get_current_time".to_string(),
        description: "Get current time in a specific timezone".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "timezone": { "type": "string" } }
        }),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(60),
        meta: None,
    }
    .with_call_template(tool_templates::GET_CURRENT_TIME_CALL)
}

fn convert_time() -> ToolSpec {
    ToolSpec {
        name: "convert_time".to_string(),
        description: "Convert timestamps, timezones, and time offsets".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "isoTimestamp": { "type": "string" },
                "unixTimestamp": { "type": "number" },
                "epochMillis": { "type": "integer" },
                "offsetSeconds": { "type": "integer" },
                "offsetMinutes": { "type": "integer" },
                "offsetHours": { "type": "integer" },
                "offsetDays": { "type": "integer" },
                "timezone": { "type": "string" },
                "sourceTimezone": { "type": "string" }
            }
        }),
        idempotent_hint: true,
        read_only_hint: true,
        timeout_secs: Some(60),
        meta: None,
    }
    .with_call_template(tool_templates::CONVERT_TIME_CALL)
}

fn wait() -> ToolSpec {
    ToolSpec {
        name: "wait".to_string(),
        description: "Wait for a specified number of seconds".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "seconds": { "type": "number" } },
            "required": ["seconds"]
        }),
        idempotent_hint: false,
        read_only_hint: true,
        timeout_secs: Some(3_660),
        meta: None,
    }
    .with_call_template(tool_templates::WAIT_CALL)
}

fn wait_until() -> ToolSpec {
    ToolSpec {
        name: "wait_until".to_string(),
        description: "Wait until a target time, up to 24 hours".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "time": { "type": "string" },
                "timezone": { "type": "string" }
            },
            "required": ["time"]
        }),
        idempotent_hint: false,
        read_only_hint: true,
        timeout_secs: Some(86_460),
        meta: None,
    }
    .with_call_template(tool_templates::WAIT_UNTIL_CALL)
}
