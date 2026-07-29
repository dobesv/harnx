//! ACP (Agent Client Protocol) for harnx: protocol types, the `AcpClient`
//! wire implementation, and the `AcpManager` coordinator used by
//! harnx-runtime. The ACP server (`HarnxAgent`) still lives in the `harnx`
//! crate and will move out in a later plan.

mod client;
#[doc(hidden)]
pub mod compat;
mod config;
mod event;
pub mod manager;

pub use client::AcpClient;

/// Internal process-role marker set on ACP servers spawned as worker-owned
/// tool/sub-agent backends. Standalone ACP frontends never set this marker.
pub const ACP_EXECUTION_ROLE_ENV: &str = "HARNX_INTERNAL_ACP_ROLE";
pub const ACP_BACKEND_ROLE: &str = "backend";
pub use config::AcpServerConfig;
pub use event::NestedAcpEvent;
pub use manager::{
    forward_acp_chunks, session_prompt_with_abort, session_prompt_with_abort_for_test, AcpManager,
};
