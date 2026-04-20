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

// Module declarations filled in by later tasks.
