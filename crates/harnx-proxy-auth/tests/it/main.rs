//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod acli_integration;
mod exec_hook;
mod gcp_auth_e2e;
mod gcp_hook_script;
mod github_app_hook_script;
mod hook_pipeline_integration;
mod https_connect;
mod integration;
mod nats_hook;
