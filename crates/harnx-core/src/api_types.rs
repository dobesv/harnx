//! LLM provider API request/response data types shared across crates.
//! Pure data — no HTTP, no config, no side effects. Provider clients
//! build these from their protocol-specific inputs and consume them to
//! produce protocol-specific outputs.

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::tool::{ToolCall, ToolDeclaration};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExtraConfig {
    pub proxy: Option<String>,
    pub connect_timeout: Option<u64>,
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
}

#[derive(Debug, Clone, Default)]
pub struct ChatCompletionsOutput {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub thought: Option<String>,
    pub id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

impl ChatCompletionsOutput {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl CompletionTokenUsage {
    pub fn new(input: Option<u64>, output: Option<u64>, cached: Option<u64>) -> Self {
        Self {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
            cached_tokens: cached.unwrap_or(0),
        }
    }

    pub fn accumulate(&mut self, other: &CompletionTokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
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

#[derive(Debug, Deserialize)]
pub struct RerankResult {
    pub index: usize,
    pub relevance_score: f64,
}
