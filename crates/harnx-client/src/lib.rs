//! `harnx-client` — LLM provider client layer for the harnx workspace.
//! Contains the `Client` trait, per-provider implementations
//! (OpenAI, Claude, Gemini, Bedrock, VertexAI, Cohere, AzureOpenAI,
//! OpenAI-compatible), the `register_client!` macro that wires them
//! together, and the shared HTTP infrastructure (request building,
//! SSE streaming, error parsing, access-token caching).
//!
//! Engine-level concerns (retry, tool-call loops, rendering, global
//! config integration) live in the `harnx` crate today and will move
//! to `harnx-engine` in a later plan.

#[macro_use]
extern crate log;

pub mod access_token;
pub mod client;
#[macro_use]
pub mod macros;
pub mod model;
pub mod stream;

// Flat re-exports so that the `register_client!` macro — which expands
// into this module — can resolve `Client`, `Model`, `SseHandler`, etc.
// via bare names, and so that downstream crates can use
// `harnx_client::Client` directly.
pub use access_token::*;
pub use client::*;
pub use model::*;
pub use stream::*;

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
    (
        llama_server,
        "llama-server",
        LlamaServerConfig,
        LlamaServerClient
    ),
);

impl ClientConfig {
    /// Returns the effective name used to identify this client.
    /// This is the configured `name` field (filename-derived), or "unknown" for Unknown.
    pub fn effective_name(&self) -> &str {
        match self {
            ClientConfig::OpenAIConfig(c) => &c.name,
            ClientConfig::OpenAICompatibleConfig(c) => &c.name,
            ClientConfig::GeminiConfig(c) => &c.name,
            ClientConfig::ClaudeConfig(c) => &c.name,
            ClientConfig::CohereConfig(c) => &c.name,
            ClientConfig::AzureOpenAIConfig(c) => &c.name,
            ClientConfig::VertexAIConfig(c) => &c.name,
            ClientConfig::BedrockConfig(c) => &c.name,
            ClientConfig::LlamaServerConfig(c) => &c.name,
            ClientConfig::Unknown => "unknown",
        }
    }

    /// Sets the `name` field on the inner config struct.
    /// Used at load time to set the filename-derived name.
    pub fn set_name(&mut self, name: String) {
        debug_assert!(!name.is_empty());
        match self {
            ClientConfig::OpenAIConfig(c) => c.name = name,
            ClientConfig::OpenAICompatibleConfig(c) => c.name = name,
            ClientConfig::GeminiConfig(c) => c.name = name,
            ClientConfig::ClaudeConfig(c) => c.name = name,
            ClientConfig::CohereConfig(c) => c.name = name,
            ClientConfig::AzureOpenAIConfig(c) => c.name = name,
            ClientConfig::VertexAIConfig(c) => c.name = name,
            ClientConfig::BedrockConfig(c) => c.name = name,
            ClientConfig::LlamaServerConfig(c) => c.name = name,
            ClientConfig::Unknown => {}
        }
    }

    /// Sets the `package` field on the inner config struct.
    /// Used at load time to set the package identifier.
    pub fn set_package(&mut self, package: Option<String>) {
        match self {
            ClientConfig::OpenAIConfig(c) => c.package = package,
            ClientConfig::OpenAICompatibleConfig(c) => c.package = package,
            ClientConfig::GeminiConfig(c) => c.package = package,
            ClientConfig::ClaudeConfig(c) => c.package = package,
            ClientConfig::CohereConfig(c) => c.package = package,
            ClientConfig::AzureOpenAIConfig(c) => c.package = package,
            ClientConfig::VertexAIConfig(c) => c.package = package,
            ClientConfig::BedrockConfig(c) => c.package = package,
            ClientConfig::LlamaServerConfig(c) => c.package = package,
            ClientConfig::Unknown => {}
        }
    }
}
