//! Script output metadata for `grep_query`: the whole search report is the
//! structured result, and `help` lists its fields.

use harnx_toolset::{output_field, output_nullable_field, ScriptOutput};
use serde_json::{json, Value};

/// The whole result document is the structured content; a script pipes it.
pub(crate) fn grep_script_output() -> ScriptOutput {
    ScriptOutput {
        summary: Some("the search report as JSON; pipe it to jq".to_string()),
        ..ScriptOutput::default()
    }
}

/// The fields of both result shapes: hit responses carry
/// `results_by_repository`, no-results responses carry `results`.
pub(crate) fn grep_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": output_field("string", "the search query"),
            "summary": summary_schema(),
            "results_by_repository": object_array(
                "grouped matches, one entry per repository (hit responses)",
            ),
            "results": object_array("always empty; present on no-results responses"),
        }
    })
}

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "description": "match counts and facet summaries",
        "properties": {
            "total_results": output_field("integer", "total matches reported by grep.app"),
            "results_shown": output_nullable_field(
                "integer",
                "files shown, capped at 10 (hit responses)",
            ),
            "repositories_found": output_nullable_field(
                "integer",
                "distinct repositories among the shown files (hit responses)",
            ),
            "top_languages": object_array("top language facets (hit responses)"),
            "top_repositories": object_array("top repository facets (hit responses)"),
            "message": output_nullable_field(
                "string",
                "why there are no results (no-results responses)",
            ),
        }
    })
}

fn object_array(description: &str) -> Value {
    json!({
        "type": ["array", "null"],
        "items": { "type": "object" },
        "description": description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_output_maps_the_whole_object() {
        assert_eq!(
            serde_json::to_value(grep_script_output()).expect("serialize script output"),
            json!({"summary": "the search report as JSON; pipe it to jq"}),
        );
    }

    #[test]
    fn output_schema_declares_both_result_shapes() {
        let schema = grep_output_schema();
        assert_eq!(schema["type"], "object");
        let properties = schema["properties"].as_object().expect("properties");
        for field in ["query", "summary", "results_by_repository", "results"] {
            assert!(properties.contains_key(field), "schema lacks {field}");
        }
        assert_eq!(schema["properties"]["query"]["type"], "string");
        assert_eq!(
            schema["properties"]["results_by_repository"]["type"],
            json!(["array", "null"]),
        );
        let summary = &schema["properties"]["summary"];
        assert_eq!(summary["type"], "object");
        assert_eq!(summary["properties"]["total_results"]["type"], "integer");
        assert_eq!(
            summary["properties"]["message"]["type"],
            json!(["string", "null"])
        );
    }
}
