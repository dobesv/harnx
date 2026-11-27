//! The strict `metadata` object schema shared by session_new,
//! session_prompt and session_send.
use serde_json::{json, Value};

pub(in crate::nats_worker::subagent_toolset) fn metadata_object_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional child-session metadata update: title, stored variables and \
    session properties. On creation, all requested state lands in the session's first metadata \
    record. On an existing session, title, variables and properties apply together in one \
    revision-guarded update before the prompt is dispatched. Omitted fields keep their current \
    state; `variables` replaces the stored overrides wholesale, including an empty map which \
    clears them (values are stored as given, never truncated; a running child does not reload \
    them mid-turn). `properties` follows the same names, values, inheritance flags and limits \
    as the child's own property write; read-only identity properties, execution overrides, raw \
    extensions and unknown fields are rejected. An invalid or conflicting update dispatches no \
    prompt; a dispatch failure after a successful update leaves the update in place (the two \
    are not transactional), and a revision conflict returns a recoverable error instead of \
    overwriting later child edits.",
        "properties": {
            "title": title_schema(),
            "variables": variables_schema(),
            "properties": properties_schema()
        },
        "additionalProperties": false
    })
}

fn title_schema() -> Value {
    json!({
        "type": "object",
        "description": "Session title. `value` is stored trimmed; a null or blank \
    value clears the title and its manual flag. `manual` defaults to false; pass true to keep a \
    later automatic title replacement from overwriting it.",
        "properties": {
            "value": {
                "type": ["string", "null"],
                "description": "The title text; null or blank clears the title."
            },
            "manual": {
                "type": "boolean",
                "description": "Whether the title is manual (default false)."
            }
        },
        "additionalProperties": false
    })
}

fn variables_schema() -> Value {
    json!({
        "type": "object",
        "description": "Stored session variables (string to string), replaced \
    wholesale when supplied. These are the stored overrides, not worker-resolved defaults.",
        "additionalProperties": { "type": "string" }
    })
}

fn properties_schema() -> Value {
    json!({
        "type": "object",
        "description": "Session properties update: `clear` runs first, then `set`, \
    then remove_labels, then add_labels.",
        "properties": {
            "set": {
                "type": "array",
                "description": "Properties to set. A blank value, 0 for a number, or \
    an empty list removes the property instead.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "A well-known or custom property name."
                        },
                        "value": {
                            "description": "A number for github_issue and \
    github_pull_request, a list of strings for labels, and text otherwise.",
                            "anyOf": [
                                { "type": "string" },
                                { "type": "integer" },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        },
                        "inherit": {
                            "type": "boolean",
                            "description": "Whether sub-agent sessions started from \
    this session copy the property. Omit to keep the current setting; web_session_url is never \
    inherited."
                        }
                    },
                    "required": ["name", "value"],
                    "additionalProperties": false
                }
            },
            "clear": string_list("Names of properties to remove."),
            "add_labels": string_list("Labels to add."),
            "remove_labels": string_list("Labels to remove.")
        },
        "additionalProperties": false
    })
}

fn string_list(description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": description,
    })
}
