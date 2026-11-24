//! Upstream MCP client construction: the connector trait, the HTTP
//! implementation and the service-task keeper. Split out of `pool` so the
//! pool module stays focused on per-principal entry lifecycle.

use anyhow::{Context, Result};
use async_trait::async_trait;
use rmcp::model::ProtocolVersion;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
pub use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::native::client_handler::PoolClientHandler;
use crate::native::NativeConfig;

use super::pool::UpstreamAuth;

/// Connects one upstream rmcp client for a principal's credential. The HTTP
/// implementation lives here; other transports plug in through this trait.
#[async_trait]
pub trait UpstreamConnector: Send + Sync {
    async fn connect(
        &self,
        auth: Arc<dyn UpstreamAuth>,
        handler: PoolClientHandler,
    ) -> Result<UpstreamConnection>;
}

/// A live upstream client: the peer to call plus the token that shuts its
/// service task down when the entry is evicted.
pub struct UpstreamConnection {
    pub peer: Peer<RoleClient>,
    pub negotiated: Option<ProtocolVersion>,
    shutdown: CancellationToken,
}

impl Drop for UpstreamConnection {
    fn drop(&mut self) {
        // Any exit path that drops the connection without it becoming a
        // pool entry — the caller was cancelled, a fenced publish refused
        // it, the pool went away — must still end the upstream session.
        // Without this the keeper below waits forever and the provider's
        // session leaks.
        self.shutdown.cancel();
    }
}

/// HTTP (Streamable HTTP) upstream connector, reusing the stdio proxy's TLS
/// flag handling. Auth comes from the credential, never from the process.
pub struct HttpUpstreamConnector {
    url: String,
    http: reqwest::Client,
    strict_session: bool,
}

impl HttpUpstreamConnector {
    pub fn from_config(config: &NativeConfig) -> Result<Self> {
        Ok(Self {
            url: config.url.clone(),
            http: super::tls::http_client_builder(config)?
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            strict_session: config.strict_session,
        })
    }

    /// Direct construction for callers that already hold an HTTP client
    /// (integration tests driving the pool standalone).
    pub fn new(url: impl Into<String>, http: reqwest::Client, strict_session: bool) -> Self {
        Self {
            url: url.into(),
            http,
            strict_session,
        }
    }
}

#[async_trait]
impl UpstreamConnector for HttpUpstreamConnector {
    async fn connect(
        &self,
        auth: Arc<dyn UpstreamAuth>,
        handler: PoolClientHandler,
    ) -> Result<UpstreamConnection> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(self.url.clone());
        if self.strict_session {
            config.allow_stateless = false;
        }
        let http = super::authenticated_http::AuthenticatedHttp {
            http: self.http.clone(),
            auth,
        };
        let transport = StreamableHttpClientTransport::with_client(http, config);
        let service: RunningService<RoleClient, PoolClientHandler> =
            rmcp::service::serve_client_with_lifecycle(handler, transport, negotiation_ladder())
                .await
                .context("connect to remote MCP server")?;
        let negotiated = service
            .peer()
            .peer_info()
            .map(|info| info.protocol_version.clone());
        let peer = service.peer().clone();
        let shutdown = CancellationToken::new();
        let service_shutdown = shutdown.clone();
        // Keep the RunningService alive until eviction; dropping it early
        // can tear down the transport tasks. The connection's Drop cancels
        // the token on every exit path, whether or not the connection ever
        // becomes a pool entry; cancelling twice is a no-op.
        tokio::spawn(async move {
            service_shutdown.cancelled().await;
            let _ = service.cancel().await;
        });
        Ok(UpstreamConnection {
            peer,
            negotiated,
            shutdown,
        })
    }
}

/// Same negotiation ladder as the stdio bridge (R10 parity): MRTR (SEP-2322)
/// rides the 2026-07-28 discover-era revision, so probe `server/discover`
/// newest-first and fall back to the legacy `initialize` handshake for older
/// upstreams. A plain `serve_client` never probes discover, so MRTR-capable
/// upstreams would negotiate down and their tools would lose the durable
/// challenge path.
pub(crate) fn negotiation_ladder() -> rmcp::service::ClientLifecycleMode {
    rmcp::service::ClientLifecycleMode::Auto {
        preferred_versions: vec![
            ProtocolVersion::LATEST,
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// List every upstream tool, following pagination.
pub async fn list_all_tools(
    peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
) -> anyhow::Result<Vec<rmcp::model::Tool>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    loop {
        let params = cursor.map(|cursor| {
            let mut params = rmcp::model::PaginatedRequestParams::default();
            params.cursor = Some(cursor);
            params
        });
        let result = peer
            .list_tools(params)
            .await
            .context("list upstream tools")?;
        tools.extend(result.tools);
        cursor = result.next_cursor.clone();
        if cursor.is_none() {
            return Ok(tools);
        }
    }
}
