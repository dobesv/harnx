mod handler;
pub mod model;
mod params;

pub(crate) use handler::grep_query_schema;
pub use params::GrepQueryParams;

#[derive(Clone)]
pub struct GrepServer {
    pub(super) client: reqwest::Client,
    pub(super) base_url: String,
}

impl GrepServer {
    pub fn new() -> Self {
        Self::with_base_url(crate::client::DEFAULT_SEARCH_URL)
    }

    /// Creates a server targeting a custom grep.app-compatible endpoint.
    ///
    /// This is primarily useful for exercising the full handler against a mock server.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }
}

impl Default for GrepServer {
    fn default() -> Self {
        Self::new()
    }
}
