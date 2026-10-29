//! Keep an overflow and its producing model together for compaction recovery.
use super::{find_llm_error, stop_if_repetitive, TurnContext};
use harnx_core::{
    error::{LlmError, ModelFallbackReason},
    input::Input,
    model::Model,
};

#[derive(Default)]
pub(super) struct LastFailure {
    error: Option<anyhow::Error>,
    model: Option<Model>,
}

impl LastFailure {
    pub(super) fn remember(&mut self, error: anyhow::Error, model: Option<&Model>) {
        let was_overflow = self
            .error
            .as_ref()
            .and_then(find_llm_error)
            .is_some_and(LlmError::is_context_overflow);
        let access_failure = find_llm_error(&error)
            .is_some_and(|error| error.is_auth_error() || error.is_context_overflow())
            || error.downcast_ref::<ModelFallbackReason>().is_some();
        if !was_overflow || !access_failure {
            self.error = Some(error);
            self.model = model.cloned();
        }
    }

    pub(super) fn finish(self, input: &mut Input, context: &TurnContext) -> anyhow::Error {
        let error = self
            .error
            .unwrap_or_else(|| anyhow::anyhow!("All models are on cooldown"));
        if find_llm_error(&error).is_some_and(LlmError::is_context_overflow) {
            if let Some(model) = self.model {
                // A failed fallback changed Input's model_id. Restore the model
                // that can benefit from the caller's forced compaction retry.
                (context.select_model_fn)(input, &model);
            }
        }
        stop_if_repetitive(error)
    }
}
