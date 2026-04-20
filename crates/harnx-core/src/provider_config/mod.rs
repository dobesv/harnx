//! Per-provider LLM client configuration structs (API keys, base URLs,
//! model catalogs, request patches, etc.). Pure data types — the
//! corresponding Client trait implementations live in the `harnx` crate
//! (and will move to `harnx-client` in a future plan). Every struct
//! here is a serde-deserializable leaf of `harnx`'s global config.

pub mod azure_openai;
pub mod bedrock;
pub mod claude;
pub mod cohere;
pub mod gemini;
pub mod openai;
pub mod openai_compatible;
pub mod vertexai;
