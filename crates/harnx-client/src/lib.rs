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
);
