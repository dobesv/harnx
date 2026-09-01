use crate::ag_ui::AgUiSink;
use serde_json::json;

pub(super) struct UsagePayloadInput {
    pub(super) input: u64,
    pub(super) output: u64,
    pub(super) cached: u64,
    pub(super) cache_write: u64,
    pub(super) session_label: Option<String>,
}

impl AgUiSink {
    pub(super) fn build_usage_payload(&self, usage: UsagePayloadInput) -> serde_json::Value {
        let mut payload = json!({
            "input": usage.input,
            "output": usage.output,
            "cached": usage.cached,
            "cache_write": usage.cache_write,
            "session_label": usage.session_label,
        });
        if let Some(context) = self.session_usage_context() {
            payload["context_tokens"] = json!(context.context_tokens);
            payload["max_context_tokens"] = json!(context.max_context_tokens);
            if let Some(percent) = context.context_percent {
                payload["context_percent"] = json!(percent);
            }
        }
        payload
    }
}
