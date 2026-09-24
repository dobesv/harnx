mod handler;
pub mod model;
mod params;

pub(crate) use handler::{web_fetch_schema, web_search_schema};
pub use params::{WebFetchParams, WebSearchParams};

#[derive(Clone)]
pub struct ExaServer {
    pub(super) client: reqwest::Client,
    pub(super) base_url: String,
}

impl ExaServer {
    pub fn new() -> Self {
        Self::with_base_url(crate::client::DEFAULT_BASE_URL)
    }

    /// Creates a server targeting a custom Exa-compatible API endpoint.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}

impl Default for ExaServer {
    fn default() -> Self {
        Self::new()
    }
}
