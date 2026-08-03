use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};

pub struct RemoteClientHandler;

impl ClientHandler for RemoteClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-remote", env!("CARGO_PKG_VERSION")),
        )
    }
}
