//! Task 1 contract tests for the shared bounded read: validation, defaults,
//! error messages, budget accounting, continuation and UTF-8 safety.

use super::*;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::tool::ToolCall;
use serde_json::json;

fn message(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn tool_call_entry(name: &str) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall {
            name: name.to_string(),
            arguments: json!({}),
            id: Some("call-1".to_string()),
            thought_signature: None,
            reasoning_provenance: None,
        }],
        timestamp: None,
        fence_token: None,
    }
}

fn resolve(value: Value) -> Result<ResolvedRead, String> {
    let options: ReadOptions = serde_json::from_value(value).expect("read options deserialize");
    options.resolve()
}

fn serialized(value: &Value) -> usize {
    serde_json::to_string(value)
        .expect("value serializes")
        .len()
}

#[path = "session_read_tests/validation.rs"]
mod validation;

#[path = "session_read_tests/paging.rs"]
mod paging;

#[path = "session_read_tests/continuation.rs"]
mod continuation;

#[path = "session_read_tests/budget.rs"]
mod budget;
