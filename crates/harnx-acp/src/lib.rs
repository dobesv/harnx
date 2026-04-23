//! ACP (Agent Client Protocol) for harnx: protocol types, the `AcpClient`
//! wire implementation, and the `AcpManager` coordinator used by
//! harnx-runtime. The ACP server (`HarnxAgent`) still lives in the `harnx`
//! crate and will move out in a later plan.

mod client;
mod config;
mod event;
pub mod manager;

pub use client::AcpClient;
pub use config::AcpServerConfig;
pub use event::NestedAcpEvent;
pub use manager::{
    forward_acp_chunks, session_prompt_with_abort, session_prompt_with_abort_for_test, AcpManager,
};
