//! Per-principal upstream MCP client pool, keyed by `(principal,
//! connection_id)` (decisions 5 and 8).
//!
//! There is never a process-wide auth header: every pool client is built with
//! exactly one credential's [`UpstreamAuth`]. An entry is dropped as soon as
//! the stored credential's `connection_id` moves on (a reconnect or a
//! disconnect), so a stale client can never act for a newer connection.

use anyhow::Result;
use harnx_credentials::{CredentialRecord, Principal, Versioned};
use rmcp::model::ProtocolVersion;
use rmcp::service::Peer;
use rmcp::service::RoleClient;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::native::client_handler::PoolClientHandler;

/// The per-credential auth contract (and the token implementation) live in
/// `upstream_auth`; re-exported here because every consumer reaches them
/// through `native::pool`.
pub use super::upstream_auth::{TokenAuthFactory, UpstreamAuth, UpstreamAuthFactory};
pub use super::upstream_connector::StreamableHttpClientTransportConfig;

pub(crate) use super::tls::http_client_builder;
pub use super::upstream_connector::list_all_tools;
/// The connector machinery lives next door; re-exported here because the
/// pool is its only production caller and every consumer reaches it through
/// `native::pool`.
pub use super::upstream_connector::{HttpUpstreamConnector, UpstreamConnection, UpstreamConnector};

/// Idle lifetime after which an unused pool entry is closed.
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Upper bound on simultaneously cached upstream clients.
pub const POOL_MAX_ENTRIES: usize = 32;
/// Upper bound on the upstream handshake alone, matching the stdio bridge's
/// 30s initialize bound. Without it a stalled upstream would park the caller
/// forever.
pub const POOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a caller waits for the credential refresh. Sized above the OAuth
/// refresh deadline (`oauth::REFRESH_TIMEOUT` = 65s) plus its contention
/// retries so a normal rotation always fits; the refresh itself runs
/// detached (see [`UpstreamPool::client`]), so this bounds the caller's
/// patience, not the rotation.
pub const POOL_REFRESH_TIMEOUT: Duration = Duration::from_secs(80);

/// One upstream `tools/list_changed` signal, from the client that saw it.
pub struct ListChanged {
    pub principal: Principal,
    pub connection_id: String,
}

/// A borrowed view of one pool entry, safe to use for a single call.
#[derive(Clone)]
pub struct PoolClient {
    pub peer: Peer<RoleClient>,
    pub negotiated: Option<ProtocolVersion>,
    /// The credential's auth, kept so upstream-sourced text can be redacted
    /// before it reaches a tool result or the journal.
    auth: Arc<dyn UpstreamAuth>,
}

impl PoolClient {
    /// Redact the credential's secret material from upstream-sourced text.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.auth.redact(text)
    }
}

struct PoolEntry {
    connection: UpstreamConnection,
    auth: Arc<dyn UpstreamAuth>,
    /// Store revision of the credential this entry was built with; a newer
    /// store write has a higher revision, so this fences stale connects.
    revision: u64,
    last_used: Instant,
}

impl PoolEntry {
    // qual:allow(dry, boilerplate) reason: "PoolClient is a borrowed view of a cached entry; the peer handle, negotiated version and auth are cloned per call by design."
    fn client(&self) -> PoolClient {
        PoolClient {
            peer: self.connection.peer.clone(),
            negotiated: self.connection.negotiated.clone(),
            auth: self.auth.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    principal: Principal,
    connection_id: String,
}

pub struct UpstreamPool {
    connector: Arc<dyn UpstreamConnector>,
    auth_factory: Arc<dyn UpstreamAuthFactory>,
    entries: Mutex<HashMap<PoolKey, PoolEntry>>,
    /// Per-key single-flight slots: callers connecting the same
    /// `(principal, connection_id)` serialize on one slot, so everyone else
    /// keeps using the pool while a slow connect is in flight.
    inflight: Mutex<HashMap<PoolKey, Arc<tokio::sync::Mutex<()>>>>,
    notify: tokio::sync::mpsc::Sender<ListChanged>,
    idle_timeout: Duration,
    max_entries: usize,
    connect_timeout: Duration,
    refresh_timeout: Duration,
}

impl UpstreamPool {
    pub fn new(
        connector: Arc<dyn UpstreamConnector>,
        auth_factory: Arc<dyn UpstreamAuthFactory>,
    ) -> (Self, tokio::sync::mpsc::Receiver<ListChanged>) {
        let (notify, list_changed) = tokio::sync::mpsc::channel(64);
        (
            Self {
                connector,
                auth_factory,
                entries: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                notify,
                idle_timeout: POOL_IDLE_TIMEOUT,
                max_entries: POOL_MAX_ENTRIES,
                connect_timeout: POOL_CONNECT_TIMEOUT,
                refresh_timeout: POOL_REFRESH_TIMEOUT,
            },
            list_changed,
        )
    }

    /// Shrink the pool's timers so expiry and connect-timeout paths are
    /// observable quickly (integration tests only).
    // qual:test_helper
    pub fn set_timeouts(
        &mut self,
        idle_timeout: Duration,
        connect_timeout: Duration,
        refresh_timeout: Duration,
    ) {
        self.idle_timeout = idle_timeout;
        self.connect_timeout = connect_timeout;
        self.refresh_timeout = refresh_timeout;
    }

    /// A notification sender clone for handlers constructed by integration tests.
    // qual:test_helper
    pub fn notify_sender(&self) -> tokio::sync::mpsc::Sender<ListChanged> {
        self.notify.clone()
    }

    pub fn auth_factory(&self) -> Arc<dyn UpstreamAuthFactory> {
        self.auth_factory.clone()
    }

    /// The upstream client for `(principal, connection_id)`. A stored
    /// credential whose `connection_id` moved on gets a fresh client; the old
    /// entry is closed first.
    ///
    /// Locking: the shared table lock is never held across the OAuth
    /// refresh or the upstream handshake. Callers for the same key share one
    /// connect through the key's in-flight slot — held from the lock until
    /// the entry is published — so exactly one caller connects per key and a
    /// slow upstream only delays that one key.
    pub async fn client(
        &self,
        principal: Principal,
        credential: &Versioned<CredentialRecord>,
    ) -> Result<PoolClient> {
        let key = PoolKey {
            principal: principal.clone(),
            connection_id: credential.record.connection_id.clone(),
        };
        let slot = self.inflight_slot(&key).await;
        // Serialize same-key connects, keeping the slot claimed in the
        // in-flight map until the winner has connected and published: every
        // runner-up then finds the published entry instead of opening a
        // second upstream session for the same credential.
        let outcome = async {
            let _guard = slot.lock().await;
            if let Some(client) = self.live_entry(&key).await {
                return Ok(client);
            }
            // Refresh and handshake run outside every shared lock.
            let (auth, connection) = self.connect_bounded(&principal, credential).await?;
            self.publish_entry(
                key.clone(),
                &principal,
                credential.revision,
                auth,
                connection,
            )
            .await
        }
        .await;
        self.prune_slot(&key, &slot).await;
        outcome
    }

    /// The single-flight slot for one key; callers serialize on it.
    async fn inflight_slot(&self, key: &PoolKey) -> Arc<tokio::sync::Mutex<()>> {
        let mut inflight = self.inflight.lock().await;
        Arc::clone(
            inflight
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Prune our slot when nobody is waiting behind us, so reconnect churn
    /// can't grow the in-flight map. A caller that races this removal takes a
    /// fresh slot and may start one extra connect; the table re-check keeps
    /// the pool correct either way.
    // qual:allow(iosp) reason: "the strong-count check and the map removal are one atomic decision; splitting them apart would make the pruning condition stale."
    async fn prune_slot(&self, key: &PoolKey, slot: &Arc<tokio::sync::Mutex<()>>) {
        if Arc::strong_count(slot) == 2 {
            self.inflight.lock().await.remove(key);
        }
    }

    /// The live entry for `key`, if one exists; dead-transport entries are
    /// reported as absent so the caller reconnects.
    async fn live_entry(&self, key: &PoolKey) -> Option<PoolClient> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(key)
            .filter(|entry| !entry.connection.peer.is_transport_closed())?;
        entry.last_used = Instant::now();
        Some(entry.client())
    }

    /// Build one upstream connection. The refresh and the handshake are
    /// bounded separately.
    ///
    /// The refresh runs as a detached task: a caller that times out or is
    /// cancelled cannot abort a rotation between the provider consuming the
    /// old refresh token and the store saving the new one — with single-use
    /// refresh tokens that cutoff would leave the credential unusable. The
    /// pool only bounds how long it waits.
    ///
    /// The handshake keeps the pool's connect timeout, so a stalled upstream
    /// parks this caller only. Errors from both are rendered through the
    /// credential's redaction: upstream-sourced text in them can echo the
    /// presented secret, and connect errors reach tool results verbatim.
    async fn connect_bounded(
        &self,
        principal: &Principal,
        credential: &Versioned<CredentialRecord>,
    ) -> Result<(Arc<dyn UpstreamAuth>, UpstreamConnection)> {
        let auth = self.auth_factory.build(credential)?;
        let mut refresh = {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move { auth.ensure_fresh().await })
        };
        match tokio::time::timeout(self.refresh_timeout, &mut refresh).await {
            Err(_) => anyhow::bail!(
                "credential refresh timed out after {:?}; retry the call",
                self.refresh_timeout
            ),
            Ok(Err(error)) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Ok(Err(_)) => anyhow::bail!("credential refresh task ended; retry the call"),
            Ok(Ok(Err(error))) => return Err(redact_connect_error(&*auth, error)),
            Ok(Ok(Ok(()))) => {}
        }
        let handler = PoolClientHandler::new(
            self.notify.clone(),
            principal.clone(),
            credential.record.connection_id.clone(),
        );
        let connect = self.connector.connect(Arc::clone(&auth), handler);
        match tokio::time::timeout(self.connect_timeout, connect).await {
            Ok(result) => result.map_err(|error| redact_connect_error(&*auth, error)),
            Err(_) => anyhow::bail!(
                "upstream handshake timed out after {:?}; retry the call",
                self.connect_timeout
            ),
        }
        .map(|connection| (auth, connection))
    }

    /// Install a freshly connected entry: it replaces every stale entry of
    /// this principal and the idle-aged ones, respects the size bound, and
    /// returns the caller's borrowed view. Removed entries end their
    /// upstream session: dropping the connection cancels the keeper.
    ///
    /// A connect built from an older credential revision than an already
    /// published entry of the same principal never installs. Its store write
    /// is stale — a reconnect has since superseded this credential — so the
    /// fresh connection is dropped (ending its session) and the stale call
    /// fails instead of clobbering the newer connection's entry. The message
    /// matches the connect flow's fence so a superseded flow reports the
    /// same failure either way.
    async fn publish_entry(
        &self,
        key: PoolKey,
        principal: &Principal,
        revision: u64,
        auth: Arc<dyn UpstreamAuth>,
        connection: UpstreamConnection,
    ) -> Result<PoolClient> {
        let mut entries = self.entries.lock().await;
        if entries
            .iter()
            .any(|(existing, entry)| entry.revision > revision && existing.principal == *principal)
        {
            anyhow::bail!("superseded by a newer connection; retry the call");
        }
        entries.retain(|existing, entry| {
            if &existing.principal == principal {
                return false;
            }
            entry.last_used.elapsed() < self.idle_timeout
        });
        while entries.len() >= self.max_entries {
            let Some(evict) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            entries.remove(&evict);
        }
        let client = PoolClient {
            peer: connection.peer.clone(),
            negotiated: connection.negotiated.clone(),
            auth: Arc::clone(&auth),
        };
        entries.insert(
            key,
            PoolEntry {
                connection,
                auth,
                revision,
                last_used: Instant::now(),
            },
        );
        Ok(client)
    }

    /// Drop every entry of one principal (credential deleted or superseded).
    pub async fn drop_principal(&self, principal: &Principal) {
        let mut entries = self.entries.lock().await;
        let stale: Vec<PoolKey> = entries
            .keys()
            .filter(|key| &key.principal == principal)
            .cloned()
            .collect();
        for key in stale {
            // Removal ends the upstream session when the entry's connection
            // is dropped.
            entries.remove(&key);
        }
    }
}

/// Render one connect-path error with the credential's secret forms
/// redacted. A handshake or refresh failure can quote upstream-sourced text
/// that echoes the presented credential, and connect errors reach tool
/// results verbatim, so the whole error chain is rendered and redacted
/// before the pool returns it.
fn redact_connect_error(auth: &dyn UpstreamAuth, error: anyhow::Error) -> anyhow::Error {
    let rendered = format!("{error:#}");
    anyhow::anyhow!("{}", auth.redact(&rendered))
}
