use anyhow::{bail, Result};

use crate::{ChatCompletionsData, ChatCompletionsOutput, Model, PromptAction, SseHandler};

impl crate::LlamaServerClient {
    pub const PROMPTS: [PromptAction<'static>; 0] = [];
}

#[async_trait::async_trait]
impl crate::Client for crate::LlamaServerClient {
    crate::client_common_fns!();

    async fn chat_completions_inner(
        &self,
        _client: &reqwest::Client,
        _data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        bail!("llama-server provider requires a Unix platform (uses unix domain sockets)")
    }

    async fn chat_completions_streaming_inner(
        &self,
        _client: &reqwest::Client,
        _handler: &mut SseHandler,
        _data: ChatCompletionsData,
    ) -> Result<()> {
        bail!("llama-server provider requires a Unix platform (uses unix domain sockets)")
    }
}
