//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod claude_upload_mock;
mod gemini_upload_mock;
mod llama_server_mock;
mod models_yaml_patches;
