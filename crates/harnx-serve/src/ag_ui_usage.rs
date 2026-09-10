use crate::ag_ui::AgUiSink;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct UsageContextSnapshot {
    pub(crate) context_tokens: usize,
    pub(crate) max_context_tokens: Option<usize>,
    pub(crate) context_percent: Option<f32>,
}

impl UsageContextSnapshot {
    pub(crate) fn from_session(session: &harnx_core::session::Session) -> Self {
        let (context_tokens, context_percent) = session.tokens_usage();
        let max_context_tokens = session.model().max_input_tokens();
        Self {
            context_tokens,
            max_context_tokens,
            context_percent: max_context_tokens.map(|_| context_percent),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::UsageContextSnapshot;
    use harnx_core::{api_types::CompletionTokenUsage, session::Session};

    #[test]
    fn context_usage_keeps_unknown_capacity_optional_and_ignores_completion_totals() {
        let mut session = Session {
            tokens: 321,
            completion_usage: CompletionTokenUsage {
                input_tokens: 9000,
                output_tokens: 2000,
                ..Default::default()
            },
            ..Default::default()
        };
        let unknown = UsageContextSnapshot::from_session(&session);
        assert_eq!(unknown.context_tokens, 321);
        assert_eq!(unknown.max_context_tokens, None);
        assert_eq!(unknown.context_percent, None);

        session.model.data_mut().max_input_tokens = Some(1000);
        let known = UsageContextSnapshot::from_session(&session);
        assert_eq!(known.context_tokens, 321);
        assert_eq!(known.max_context_tokens, Some(1000));
        assert_eq!(known.context_percent, Some(32.1));
    }
}
