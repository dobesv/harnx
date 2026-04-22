//! Re-export shim. ACP protocol bits live in `harnx-acp`; the ACP server
//! (`HarnxAgent`) lives in `harnx-acp-server` (plan P48). Re-exports
//! preserve `harnx::acp::X` callsites in integration tests and test_utils.

// ACP types are used by integration tests (tmux_e2e.rs) and test_utils/
// (interrupt.rs), but not by the harnx binary itself — hence the allow.
#[allow(unused_imports)]
pub use harnx_acp::{AcpClient, AcpManager, AcpServerConfig};
pub use harnx_acp_server::HarnxAgent;
