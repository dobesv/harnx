//! LLM provider API request/response data types shared across crates.
//! Pure data — no HTTP, no config, no side effects. Provider clients
//! build these from their protocol-specific inputs and consume them to
//! produce protocol-specific outputs.

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::tool::{ToolCall, ToolDeclaration};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtraConfig {
    pub proxy: Option<String>,
    pub connect_timeout: Option<u64>,
    /// Per-read inactivity timeout in seconds for provider HTTP responses.
    pub read_timeout: Option<u64>,
    pub accept_invalid_certs: Option<bool>,
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

#[derive(Debug)]
pub struct ChatCompletionsData {
    pub messages: Vec<Message>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub functions: Option<Vec<ToolDeclaration>>,
    pub stream: bool,
    /// Attachments directory for providers that expand attachments internally.
    /// When set, `cid:` references in ImageUrl parts remain raw (not pre-expanded to base64)
    /// and the provider client reads blobs from this directory during request build.
    /// `None` means either no session (one-shot) or runtime pre-pass already inlined images.
    pub attachments_dir: Option<std::path::PathBuf>,
}

/// Completion output with token usage in the OTel-subset convention.
///
/// `input_tokens` includes all cache tokens. `cached_tokens` (cache reads) and
/// `cache_write_tokens` are subsets of it. The invariant is
/// `input_tokens >= cached_tokens + cache_write_tokens`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatCompletionsOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub thought: Option<String>,
    pub id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

impl ChatCompletionsOutput {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ..Default::default()
        }
    }
}

/// Token usage in the OTel-subset convention.
///
/// `input_tokens` includes all cache tokens. `cached_tokens` (cache reads) and
/// `cache_write_tokens` are subsets of it. The invariant is
/// `input_tokens >= cached_tokens + cache_write_tokens`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl CompletionTokenUsage {
    pub fn new(input: Option<u64>, output: Option<u64>, cached: Option<u64>) -> Self {
        Self {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
            cached_tokens: cached.unwrap_or(0),
            cache_write_tokens: 0,
        }
    }

    /// Returns input tokens outside both cache subsets, saturating on invalid provider counts.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_tokens)
            .saturating_sub(self.cache_write_tokens)
    }

    pub fn accumulate(&mut self, other: &CompletionTokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

impl std::fmt::Display for CompletionTokenUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = vec![];
        if self.input_tokens > 0 {
            parts.push(format!("📥 {}", self.input_tokens));
        }
        if self.output_tokens > 0 {
            parts.push(format!("📤 {}", self.output_tokens));
        }
        if self.cached_tokens > 0 {
            parts.push(format!("💾 {}", self.cached_tokens));
        }
        write!(f, "{}", parts.join("  "))
    }
}

#[derive(Debug)]
pub struct EmbeddingsData {
    pub texts: Vec<String>,
    pub query: bool,
}

impl EmbeddingsData {
    pub fn new(texts: Vec<String>, query: bool) -> Self {
        Self { texts, query }
    }
}

pub type EmbeddingsOutput = Vec<Vec<f32>>;

#[derive(Debug)]
pub struct RerankData {
    pub query: String,
    pub documents: Vec<String>,
    pub top_n: usize,
}

impl RerankData {
    pub fn new(query: String, documents: Vec<String>, top_n: usize) -> Self {
        Self {
            query,
            documents,
            top_n,
        }
    }
}

pub type RerankOutput = Vec<RerankResult>;

#[cfg(test)]
mod tests {
    use super::{ChatCompletionsOutput, CompletionTokenUsage};

    #[test]
    fn uncached_input_tokens_uses_cache_subsets_and_saturates() {
        let usage = CompletionTokenUsage {
            input_tokens: 1_000,
            cached_tokens: 200,
            cache_write_tokens: 50,
            ..Default::default()
        };
        assert_eq!(usage.uncached_input_tokens(), 750);

        let provider_quirk = CompletionTokenUsage {
            input_tokens: 100,
            cached_tokens: 200,
            ..Default::default()
        };
        assert_eq!(provider_quirk.uncached_input_tokens(), 0);
    }

    #[test]
    fn accumulate_sums_cache_write_tokens() {
        let mut total = CompletionTokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 30,
            cache_write_tokens: 10,
        };
        let next = CompletionTokenUsage {
            input_tokens: 50,
            output_tokens: 5,
            cached_tokens: 15,
            cache_write_tokens: 7,
        };

        total.accumulate(&next);

        assert_eq!(
            total,
            CompletionTokenUsage {
                input_tokens: 150,
                output_tokens: 25,
                cached_tokens: 45,
                cache_write_tokens: 17,
            }
        );
    }

    #[test]
    fn cache_write_tokens_default_when_deserializing_old_data() {
        let usage: CompletionTokenUsage =
            serde_json::from_str(r#"{"input_tokens":100,"output_tokens":20,"cached_tokens":30}"#)
                .expect("old token usage should deserialize");
        let output: ChatCompletionsOutput = serde_json::from_str(r#"{"text":"","tool_calls":[]}"#)
            .expect("old completion output should deserialize");

        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(output.cache_write_tokens, None);
    }
}

#[derive(Debug, Deserialize)]
pub struct RerankResult {
    pub index: usize,
    pub relevance_score: f64,
}
