mod handler;
pub mod model;
mod params;

use std::sync::Arc;

pub(crate) use handler::{tool_schema, TOOL_DESCRIPTIONS};
pub use params::{FetchParams, YoutubeTranscriptParams};

use crate::client::FetchClient;
use crate::net::Lookup;

#[derive(Clone)]
pub struct FetchServer {
    pub(super) client: FetchClient,
}

impl FetchServer {
    pub fn new(allow_private_ip: bool) -> Self {
        Self {
            client: FetchClient::new(allow_private_ip)
                .expect("hard-coded HTTP client configuration must be valid"),
        }
    }

    #[doc(hidden)]
    pub fn with_lookup(allow_private_ip: bool, lookup: Arc<dyn Lookup>) -> Self {
        Self {
            client: FetchClient::with_lookup(allow_private_ip, lookup)
                .expect("hard-coded HTTP client configuration must be valid"),
        }
    }

    pub fn allows_private_ip(&self) -> bool {
        self.client.allow_private_ip()
    }
}

impl Default for FetchServer {
    fn default() -> Self {
        Self::new(false)
    }
}
