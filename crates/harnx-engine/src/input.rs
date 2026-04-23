//! Input passed into an engine turn. Intentionally minimal in this
//! scaffold — will be replaced by `harnx_core::input::Input` (or similar)
//! when the full engine lands.

#[derive(Debug, Clone, Default)]
pub struct EngineInput {
    /// Plain-text user message. Future versions will add file
    /// attachments, images, and tool-result re-injection.
    pub text: String,
}

impl EngineInput {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}
