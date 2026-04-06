mod access_token;
mod common;
mod message;
#[macro_use]
mod macros;
mod model;
mod stream;

pub use crate::tool::ToolCall;
pub use common::*;
pub use message::*;
pub use model::*;
pub use stream::*;

#[cfg(test)]
static TEST_CLIENT: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<dyn Client>>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub fn set_test_client(client: Option<std::sync::Arc<dyn Client>>) {
    let slot = TEST_CLIENT.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().expect("test client mutex poisoned") = client;
}

#[cfg(test)]
pub(crate) fn take_test_client() -> Option<std::sync::Arc<dyn Client>> {
    TEST_CLIENT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("test client mutex poisoned")
        .clone()
}

#[cfg(test)]
pub(crate) struct TestClient(pub(crate) std::sync::Arc<dyn Client>);

#[cfg(test)]
#[async_trait::async_trait]
impl Client for TestClient {
    fn global_config(&self) -> &crate::config::GlobalConfig {
        self.0.global_config()
    }

    fn extra_config(&self) -> Option<&ExtraConfig> {
        self.0.extra_config()
    }

    fn patch_config(&self) -> Option<&RequestPatch> {
        self.0.patch_config()
    }

    fn name(&self) -> &str {
        self.0.name()
    }

    fn model(&self) -> &Model {
        self.0.model()
    }

    fn model_mut(&mut self) -> &mut Model {
        panic!("test client wrapper does not support mutable model access")
    }

    async fn chat_completions_inner(
        &self,
        client: &reqwest::Client,
        data: ChatCompletionsData,
    ) -> anyhow::Result<ChatCompletionsOutput> {
        self.0.chat_completions_inner(client, data).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &reqwest::Client,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> anyhow::Result<()> {
        self.0
            .chat_completions_streaming_inner(client, handler, data)
            .await
    }

    async fn embeddings_inner(
        &self,
        client: &reqwest::Client,
        data: &EmbeddingsData,
    ) -> anyhow::Result<EmbeddingsOutput> {
        self.0.embeddings_inner(client, data).await
    }

    async fn rerank_inner(
        &self,
        client: &reqwest::Client,
        data: &RerankData,
    ) -> anyhow::Result<RerankOutput> {
        self.0.rerank_inner(client, data).await
    }
}

register_client!(
    (openai, "openai", OpenAIConfig, OpenAIClient),
    (
        openai_compatible,
        "openai-compatible",
        OpenAICompatibleConfig,
        OpenAICompatibleClient
    ),
    (gemini, "gemini", GeminiConfig, GeminiClient),
    (claude, "claude", ClaudeConfig, ClaudeClient),
    (cohere, "cohere", CohereConfig, CohereClient),
    (
        azure_openai,
        "azure-openai",
        AzureOpenAIConfig,
        AzureOpenAIClient
    ),
    (vertexai, "vertexai", VertexAIConfig, VertexAIClient),
    (bedrock, "bedrock", BedrockConfig, BedrockClient),
);

pub const OPENAI_COMPATIBLE_PROVIDERS: [(&str, &str); 18] = [
    ("ai21", "https://api.ai21.com/studio/v1"),
    (
        "cloudflare",
        "https://api.cloudflare.com/client/v4/accounts/{ACCOUNT_ID}/ai/v1",
    ),
    ("deepinfra", "https://api.deepinfra.com/v1/openai"),
    ("deepseek", "https://api.deepseek.com"),
    ("ernie", "https://qianfan.baidubce.com/v2"),
    ("github", "https://models.inference.ai.azure.com"),
    ("groq", "https://api.groq.com/openai/v1"),
    ("hunyuan", "https://api.hunyuan.cloud.tencent.com/v1"),
    ("minimax", "https://api.minimax.chat/v1"),
    ("mistral", "https://api.mistral.ai/v1"),
    ("moonshot", "https://api.moonshot.cn/v1"),
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("perplexity", "https://api.perplexity.ai"),
    (
        "qianwen",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    ),
    ("xai", "https://api.x.ai/v1"),
    ("zhipuai", "https://open.bigmodel.cn/api/paas/v4"),
    // RAG-dedicated
    ("jina", "https://api.jina.ai/v1"),
    ("voyageai", "https://api.voyageai.com/v1"),
];
