//! Re-export shim. ACP coordinator (`AcpManager`) now lives in
//! `harnx-acp::manager` — moved in Plan 46a (step 9 β+). The ACP server
//! (`HarnxAgent`) still lives at `crates/harnx/src/acp/server.rs` and is
//! scheduled for its own crate (harnx-acp-server) later.

mod server;

pub use harnx_acp::*;
pub use server::HarnxAgent;

#[cfg(test)]
mod test_regression_issue_68;
