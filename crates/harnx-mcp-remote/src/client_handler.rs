// rmcp deprecated MCP Roots (SEP-2577); proxy still must return method_not_found
// here to avoid overwriting remote roots.
#![allow(deprecated)]

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ErrorData, Implementation, InitializeRequestParams, ListRootsResult,
};
use rmcp::service::{RequestContext, RoleClient};

pub struct RemoteClientHandler;

impl ClientHandler for RemoteClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-remote", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(ErrorData::method_not_found::<
            rmcp::model::ListRootsRequestMethod,
        >())
    }
}
