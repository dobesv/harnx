//! Credential-derived state: catalogs, user registrations and their
//! connection_id-fenced lifecycle (decision 5).
//!
//! The credential KV entry is authoritative. Catalogs and user registrations
//! carry the `connection_id` they were derived from, and every write or delete
//! here is a CAS after re-checking that connection, so a stale instance can
//! neither resurrect nor clobber a newer connection's derived state.

use crate::native::{
    pool::{UpstreamAuthFactory, UpstreamPool},
    NativeConfig,
};
use anyhow::{anyhow, Result};
use harnx_credentials::{
    CatalogStore, CredentialRecord, CredentialStore, PendingFlowStore, Principal,
    RefreshLeaseStore, Versioned,
};
use harnx_toolset_server::{
    result_limits::ResultLimits, HandleAudience, RegistrationHandle, RegistrationIdentity,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

mod bookkeeping;
mod connection;
mod listener;
mod management;
mod notifications;
mod publication;
mod recovery;
mod refresh;
mod tools;

#[cfg(test)]
mod tests;

pub use listener::run_list_changed_loop;
pub use management::{DisconnectOutcome, StatusView};
pub use tools::{filter_reserved, map_upstream_tool, RESERVED_TOOL_NAMES};

/// Upstream tool-list refresh debounce (decision 9).
pub const LIST_CHANGED_DEBOUNCE: Duration = Duration::from_secs(1);
/// Bounded CAS retries under contention; conflicts beyond this are reported.
const CAS_ROUNDS: usize = 8;

/// A registration write was refused because its credential connection moved on.
#[derive(Debug, PartialEq, Eq)]
pub struct Superseded;

/// Everything the native server keeps per credential: stores, the
/// registration handle, the upstream pool and the refresh bookkeeping.
pub struct Lifecycle {
    pub config: NativeConfig,
    pub identity: RegistrationIdentity,
    pub identity_token: String,
    pub credentials: CredentialStore,
    pub flows: PendingFlowStore,
    pub catalogs: CatalogStore,
    /// Shared OAuth refresh leases prevent instances rotating the same token
    /// concurrently. Opened in token-only mode too so bucket settings agree.
    pub leases: RefreshLeaseStore,
    pub handle: RegistrationHandle,
    pub pool: UpstreamPool,
    pub auth_factory: Arc<dyn UpstreamAuthFactory>,
    pub oauth: Option<Arc<super::oauth::OAuthBackend>>,
    pub result_limits: ResultLimits,
    pub refresh_interval: Duration,
    pub shutdown: CancellationToken,
    /// Principals this instance refreshes registrations for, with the
    /// connection and publication revision each registration was derived from.
    managed: tokio::sync::Mutex<HashMap<Principal, bookkeeping::ManagedRegistration>>,
    #[cfg(test)]
    hooks: tests::Hooks,
}

/// Inputs required to assemble a registration lifecycle.
pub struct LifecycleInit {
    pub config: NativeConfig,
    pub identity: RegistrationIdentity,
    pub identity_token: String,
    pub credentials: CredentialStore,
    pub flows: PendingFlowStore,
    pub catalogs: CatalogStore,
    /// Shared OAuth refresh leases prevent instances rotating the same token
    /// concurrently. Opened in token-only mode too so bucket settings agree.
    pub leases: RefreshLeaseStore,
    pub handle: RegistrationHandle,
    pub pool: UpstreamPool,
    pub auth_factory: Arc<dyn UpstreamAuthFactory>,
    pub result_limits: ResultLimits,
    pub refresh_interval: Duration,
    pub shutdown: CancellationToken,
}

impl Lifecycle {
    pub fn new(init: LifecycleInit) -> Self {
        let LifecycleInit {
            config,
            identity,
            identity_token,
            credentials,
            flows,
            catalogs,
            leases,
            handle,
            pool,
            auth_factory,
            result_limits,
            refresh_interval,
            shutdown,
        } = init;
        Self {
            config,
            identity,
            identity_token,
            credentials,
            flows,
            catalogs,
            leases,
            handle,
            pool,
            auth_factory,
            oauth: None,
            result_limits,
            refresh_interval,
            shutdown,
            managed: tokio::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            hooks: tests::Hooks::default(),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.config.server_name
    }

    /// The registration audience for one principal: user-scoped, or the
    /// ownerless-only `.anon` key in `--single-user` mode (decision 4).
    pub fn audience(principal: &Principal) -> HandleAudience {
        match principal {
            Principal::User(user_id) => HandleAudience::User(user_id.clone()),
            Principal::Ownerless => HandleAudience::OwnerlessOnly,
        }
    }

    /// Build one upstream client for a principal's live credential.
    pub async fn upstream(
        &self,
        principal: &Principal,
    ) -> Result<(Versioned<CredentialRecord>, crate::native::pool::PoolClient)> {
        let store = self
            .credentials
            .for_principal(principal.clone())
            .map_err(|error| anyhow!("{error}"))?;
        let credential = store
            .get()
            .await
            .map_err(|error| anyhow!("{error}"))?
            .ok_or_else(|| anyhow!("no credential"))?;
        let client = self.pool.client(principal.clone(), &credential).await?;
        Ok((credential, client))
    }
}
