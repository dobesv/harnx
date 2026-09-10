use harnx_core::message::{Message, MessageContent};
use harnx_core::tool::ToolCall;
use std::collections::HashSet;

const IMPORTED_TOOL_CALL_ID_PREFIX: &str = "harnx_imported_call_";

/// Allocates request-local correlation IDs for imported calls that have no ID.
///
/// Gemini doesn't return correlation IDs, while Claude, Bedrock, and OpenAI
/// require them when replaying tool history. Existing IDs remain unchanged.
pub(crate) struct ToolCallIdAllocator {
    used: HashSet<String>,
    next: usize,
}

impl ToolCallIdAllocator {
    pub(crate) fn new(messages: &[Message]) -> Self {
        let used = messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::ToolCalls(calls) => Some(&calls.tool_results),
                _ => None,
            })
            .flatten()
            .filter_map(|result| result.call.id.as_deref())
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        Self { used, next: 0 }
    }

    pub(crate) fn id_for(&mut self, call: &ToolCall) -> String {
        if let Some(id) = call.id.as_deref().filter(|id| !id.is_empty()) {
            return id.to_owned();
        }

        loop {
            let id = format!("{IMPORTED_TOOL_CALL_ID_PREFIX}{}", self.next);
            self.next += 1;
            if self.used.insert(id.clone()) {
                return id;
            }
        }
    }
}
