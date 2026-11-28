//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod mcp_http_client;
mod pr_waiter;
mod publish_plan;
mod rollback;
mod script_results;
mod template_support;
mod tool_template_bindings;
mod tool_templates;
