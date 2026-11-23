//! rmcp client handler for per-user upstream connections.

use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
use rmcp::service::NotificationContext;

use harnx_credentials::Principal;

use crate::native::pool::ListChanged;

/// One upstream client's handler. `on_tool_list_changed` (decision 9) sends
/// the signal to the pool's channel; the registration task debounces it and
/// re-lists that principal's tools under the decision 5 fencing rules.
pub struct PoolClientHandler {
    notify: tokio::sync::mpsc::Sender<ListChanged>,
    principal: Principal,
    connection_id: String,
}

impl PoolClientHandler {
    pub fn new(
        notify: tokio::sync::mpsc::Sender<ListChanged>,
        principal: Principal,
        connection_id: String,
    ) -> Self {
        Self {
            notify,
            principal,
            connection_id,
        }
    }
}

impl ClientHandler for PoolClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-remote", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<rmcp::service::RoleClient>) {
        // A closed pool entry's channel send fails; nothing to update then.
        let _ = self
            .notify
            .send(ListChanged {
                principal: self.principal.clone(),
                connection_id: self.connection_id.clone(),
            })
            .await;
    }
}
