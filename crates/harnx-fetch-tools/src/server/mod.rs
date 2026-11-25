mod handler;
mod image;
pub mod model;
mod params;
mod script_result;

use std::sync::Arc;

pub(crate) use handler::tool_schema;
pub use params::{FetchParams, YoutubeTranscriptParams};
pub use script_result::ResultStyle;

use crate::client::FetchClient;
use crate::net::Lookup;

pub(crate) const TOOL_DESCRIPTIONS: [(&str, &str); 6] = [
    (
        "fetch_html",
        "Fetch a URL and return its raw HTML, or return a PNG, JPEG, GIF, or WebP image up to 5 MiB.",
    ),
    (
        "fetch_markdown",
        "Fetch a URL and convert its HTML content to Markdown, or return a PNG, JPEG, GIF, or WebP image up to 5 MiB.",
    ),
    (
        "fetch_txt",
        "Fetch a URL and return plain DOM text with scripts and styles removed, or return a PNG, JPEG, GIF, or WebP image up to 5 MiB.",
    ),
    (
        "fetch_json",
        "Fetch a URL and return validated, pretty-printed JSON, or return a PNG, JPEG, GIF, or WebP image up to 5 MiB.",
    ),
    (
        "fetch_readable",
        "Fetch a URL and extract its main readable article as Markdown, or return a PNG, JPEG, GIF, or WebP image up to 5 MiB.",
    ),
    (
        "fetch_youtube_transcript",
        "Fetch a YouTube watch page and return its timestamped caption transcript.",
    ),
];

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
