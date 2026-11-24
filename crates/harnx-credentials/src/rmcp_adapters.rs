//! rmcp 3.8 OAuth persistence adapters.
//!
//! [`RmcpCredentialStore`] is bound to an existing connection. Complete initial
//! acquisition through `CredentialStore::put_connect`, then install this adapter
//! on that connection's AuthorizationManager. It refuses unguarded saves: rmcp
//! must hold its refresh guard across load, exchange and save. A reconnect needs
//! a new adapter, so an old manager can never refresh another account's tokens.
//!
//! [`RmcpStateStore`] binds to a durable connect flow. Prepare rmcp's authorization
//! state first, pass it through `FlowOptions::authorization_state` when creating
//! the flow, then use this adapter on any instance serving the callback.

use async_trait::async_trait;
use rmcp::transport::auth::{
    self, AuthError, CredentialRefreshGuard, StoredAuthorizationState, StoredCredentials,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{
    CredentialRecord, CredentialStore, Error, PendingFlowStore, RefreshLease, RefreshLeaseStore,
    Result, Secret,
};

type LeaseSlot = Arc<Mutex<Option<RefreshLease>>>;

pub struct RmcpCredentialStore {
    credentials: CredentialStore,
    leases: RefreshLeaseStore,
    connection_id: String,
    expected_revision: Option<u64>,
    active: LeaseSlot,
    gate: Arc<AsyncMutex<()>>,
}

impl std::fmt::Debug for RmcpCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RmcpCredentialStore")
            .field("connection_id", &self.connection_id)
            .finish_non_exhaustive()
    }
}

impl RmcpCredentialStore {
    pub fn new(
        credentials: CredentialStore,
        leases: RefreshLeaseStore,
        connection_id: String,
    ) -> Self {
        Self {
            credentials,
            leases,
            connection_id,
            expected_revision: None,
            active: Arc::new(Mutex::new(None)),
            gate: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Refuse an exchange if another instance already refreshed this snapshot.
    /// Check after acquiring the lease, before sending any refresh token upstream.
    pub fn with_expected_revision(mut self, revision: u64) -> Self {
        self.expected_revision = Some(revision);
        self
    }

    fn take_lease(&self) -> Result<RefreshLease> {
        self.active
            .lock()
            .map_err(|_| Error::Storage)?
            .take()
            .ok_or(Error::LeaseLost)
    }

    fn check_connection(&self, record: &CredentialRecord) -> Result<()> {
        if record.connection_id != self.connection_id {
            return Err(Error::ConnectionMismatch);
        }
        Ok(())
    }
}

struct AdapterRefreshGuard {
    active: LeaseSlot,
    _gate: OwnedMutexGuard<()>,
}

impl Drop for AdapterRefreshGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.take();
        }
    }
}

#[async_trait]
impl auth::CredentialStore for RmcpCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let Some(current) = self.credentials.get().await.map_err(auth_error)? else {
            return Ok(None);
        };
        self.check_connection(&current.record).map_err(auth_error)?;
        to_rmcp_credentials(&current.record)
            .map(Some)
            .map_err(auth_error)
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let lease = self.take_lease().map_err(auth_error)?;
        let record = &lease.credential().map_err(auth_error)?.record;
        self.check_connection(record).map_err(auth_error)?;
        let material = from_rmcp_credentials(record, credentials).map_err(auth_error)?;
        lease.commit(material).await.map_err(auth_error)?;
        Ok(())
    }

    /// Clear is fenced to this connection, never whichever connection happens to exist now.
    async fn clear(&self) -> std::result::Result<(), AuthError> {
        let Some(current) = self.credentials.get().await.map_err(auth_error)? else {
            return Ok(());
        };
        self.check_connection(&current.record).map_err(auth_error)?;
        self.credentials
            .delete(current.revision)
            .await
            .map_err(auth_error)
    }

    async fn acquire_refresh_guard(
        &self,
    ) -> std::result::Result<Option<CredentialRefreshGuard>, AuthError> {
        let gate = self.gate.clone().lock_owned().await;
        let lease = self
            .leases
            .acquire(&self.credentials)
            .await
            .map_err(auth_error)?;
        let snapshot = lease.credential().map_err(auth_error)?;
        self.check_connection(&snapshot.record)
            .map_err(auth_error)?;
        if self
            .expected_revision
            .is_some_and(|revision| revision != snapshot.revision)
        {
            return Err(auth_error(Error::Conflict));
        }
        *self.active.lock().map_err(|_| auth_error(Error::Storage))? = Some(lease);
        Ok(Some(CredentialRefreshGuard::new(AdapterRefreshGuard {
            active: self.active.clone(),
            _gate: gate,
        })))
    }

    async fn on_refresh_token_rejected(
        &self,
        _credentials: &StoredCredentials,
    ) -> std::result::Result<(), AuthError> {
        let lease = self.take_lease().map_err(auth_error)?;
        self.check_connection(&lease.credential().map_err(auth_error)?.record)
            .map_err(auth_error)?;
        lease.mark_needs_login().await.map_err(auth_error)?;
        Ok(())
    }
}

mod conversion;
use conversion::from_rmcp_credentials;
pub use conversion::to_rmcp_credentials;

#[derive(Clone, Debug)]
pub struct RmcpStateStore {
    flows: PendingFlowStore,
    challenge: Secret<String>,
}

impl RmcpStateStore {
    pub fn new(flows: PendingFlowStore, challenge: Secret<String>) -> Self {
        Self { flows, challenge }
    }
}

#[async_trait]
impl auth::StateStore for RmcpStateStore {
    async fn save(
        &self,
        csrf_token: &str,
        state: StoredAuthorizationState,
    ) -> std::result::Result<(), AuthError> {
        check_state_token(csrf_token, &state).map_err(auth_error)?;
        self.flows
            .save_authorization_state(self.challenge.expose(), state)
            .await
            .map_err(auth_error)
    }

    async fn load(
        &self,
        csrf_token: &str,
    ) -> std::result::Result<Option<StoredAuthorizationState>, AuthError> {
        let Some(flow) = self
            .flows
            .get_by_state(csrf_token)
            .await
            .map_err(auth_error)?
        else {
            return Ok(None);
        };
        if flow.record.challenge_id.expose() != self.challenge.expose() {
            return Err(auth_error(Error::AuthenticationFailed));
        }
        Ok(flow
            .record
            .authorization_state
            .map(|state| state.expose().clone()))
    }

    async fn delete(&self, csrf_token: &str) -> std::result::Result<(), AuthError> {
        let Some(flow) = self
            .flows
            .get_by_state(csrf_token)
            .await
            .map_err(auth_error)?
        else {
            return Ok(());
        };
        if flow.record.challenge_id.expose() != self.challenge.expose() {
            return Err(auth_error(Error::AuthenticationFailed));
        }
        self.flows
            .delete_authorization_state(self.challenge.expose())
            .await
            .map_err(auth_error)
    }
}

fn auth_error(error: Error) -> AuthError {
    AuthError::CredentialStoreError(error.to_string())
}

fn check_state_token(csrf_token: &str, state: &StoredAuthorizationState) -> Result<()> {
    if state.csrf_token != csrf_token {
        return Err(Error::AuthenticationFailed);
    }
    Ok(())
}
