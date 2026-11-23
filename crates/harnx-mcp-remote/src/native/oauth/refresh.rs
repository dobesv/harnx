//! Per-connection OAuth refresh and fenced invalid-grant cleanup.
use super::super::pool::{TokenAuthFactory, UpstreamAuth, UpstreamAuthFactory};
use super::{restored_manager, NativeConfig, OAuthHttp};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use chrono::Utc;
use harnx_credentials::{
    rmcp_adapters::RmcpCredentialStore, CatalogStore, CredentialMaterial, CredentialRecord,
    CredentialStatus, CredentialStore, Principal, RefreshLeaseStore, Versioned,
};
use harnx_toolset_server::{HandleAudience, RegistrationHandle, RegistrationHandleError};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use rmcp::transport::{
    auth::AuthError, streamable_http_client::StreamableHttpClientTransportConfig,
};
use std::{
    borrow::Cow,
    sync::{Arc, RwLock},
    time::Duration,
};
use tracing::instrument::WithSubscriber;

/// How long one OAuth refresh attempt may take, including its contention
/// retries. Callers bound their own wait above this; the rotation itself
/// always runs to its own deadline.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(65);
const CONTENTION_BACKOFF: Duration = Duration::from_millis(25);
const CLEANUP_CAS_RETRIES: usize = 8;
const PRE_EXPIRY_WINDOW_SECS: i64 = 60;

/// Routes token credentials unchanged; OAuth managers remain bound to the
/// credential's principal and connection_id for their entire lifetime.
#[derive(Clone)]
pub struct NativeAuthFactory {
    pub credentials: CredentialStore,
    pub leases: RefreshLeaseStore,
    pub catalogs: CatalogStore,
    pub handle: RegistrationHandle,
    pub config: NativeConfig,
}

impl UpstreamAuthFactory for NativeAuthFactory {
    fn build(&self, credential: &Versioned<CredentialRecord>) -> Result<Arc<dyn UpstreamAuth>> {
        match &credential.record.material {
            CredentialMaterial::Token(_) => TokenAuthFactory {
                header: self.config.token_header.clone(),
                scheme: self.config.token_scheme.clone(),
            }
            .build(credential),
            CredentialMaterial::Oauth2(_) => {
                let principal = credential
                    .record
                    .owner_user_id
                    .clone()
                    .map(Principal::User)
                    .unwrap_or(Principal::Ownerless);
                let store = self.credentials.for_principal(principal)?;
                Ok(Arc::new(OAuthUpstreamAuth {
                    factory: self.clone(),
                    store,
                    connection_id: credential.record.connection_id.clone(),
                    header: RwLock::new(None),
                }))
            }
        }
    }
}

struct OAuthUpstreamAuth {
    factory: NativeAuthFactory,
    store: CredentialStore,
    connection_id: String,
    header: RwLock<Option<HeaderValue>>,
}

impl OAuthUpstreamAuth {
    async fn current(&self) -> Result<Versioned<CredentialRecord>> {
        let credential = match self.store.get().await {
            Ok(Some(credential)) => credential,
            Ok(None)
            | Err(harnx_credentials::Error::NeedsLogin | harnx_credentials::Error::Expired) => {
                bail!("{}", self.reconnect_message())
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            credential.record.connection_id == self.connection_id,
            "connection superseded; reconnect this server"
        );
        Ok(credential)
    }

    fn install(&self, record: &CredentialRecord) -> Result<()> {
        let CredentialMaterial::Oauth2(material) = &record.material else {
            bail!("connection superseded")
        };
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", material.access_token.expose()))
                .map_err(|_| anyhow!("invalid OAuth token"))?;
        value.set_sensitive(true);
        *self
            .header
            .write()
            .map_err(|_| anyhow!("OAuth credential unavailable"))? = Some(value);
        Ok(())
    }

    async fn prepare_refresh_manager(
        &self,
        record: &CredentialRecord,
        expected_revision: u64,
    ) -> Result<rmcp::transport::auth::AuthorizationManager> {
        let CredentialMaterial::Oauth2(material) = &record.material else {
            bail!("connection superseded")
        };
        let http = Arc::new(OAuthHttp::new(&self.factory.config)?);
        let metadata = material
            .authorization_metadata
            .as_ref()
            .ok_or_else(|| anyhow!("OAuth metadata unavailable; reconnect this server"))?
            .expose()
            .clone();
        let mut manager = restored_manager(
            &material.resource,
            metadata,
            &material.client,
            &material.scopes,
            http,
        )
        .await?;
        manager.set_credential_store(
            RmcpCredentialStore::new(
                self.store.clone(),
                self.factory.leases.clone(),
                self.connection_id.clone(),
            )
            .with_expected_revision(expected_revision),
        );
        Ok(manager)
    }

    async fn handle_refresh_rejected(&self, initial_revision: u64) -> Result<()> {
        let snapshot = self.store.inspect().await?;
        if snapshot.record.as_ref().is_some_and(|record| {
            record.connection_id == self.connection_id
                && record.status == CredentialStatus::NeedsLogin
        }) {
            self.cleanup_needs_login().await?;
            bail!("{}", self.reconnect_message());
        }
        if snapshot.revision != initial_revision {
            return self.install(&self.current().await?.record);
        }
        bail!("OAuth refresh state changed; retry the call");
    }

    async fn handle_auth_required(
        &self,
        current: &CredentialRecord,
        current_revision: u64,
    ) -> Result<()> {
        let CredentialMaterial::Oauth2(material) = &current.material else {
            bail!("connection superseded")
        };
        if material.refresh_token.is_none() {
            match self.store.mark_needs_login(current_revision).await {
                Ok(_) => self.cleanup_needs_login().await?,
                Err(harnx_credentials::Error::Conflict) => {
                    return self.install(&self.current().await?.record);
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("{}", self.reconnect_message());
    }

    async fn handle_store_error(
        &self,
        initial_revision: u64,
        reason: &str,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let latest = self.current().await?;
        if latest.revision != initial_revision {
            return self.install(&latest.record);
        }
        if reason != harnx_credentials::Error::LeaseBusy.to_string() {
            bail!("OAuth refresh could not be committed; retry the call");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("OAuth refresh contended; retry the call")
        }
        tokio::time::sleep(CONTENTION_BACKOFF).await;
        Ok(())
    }

    async fn refresh(&self, initial: Versioned<CredentialRecord>) -> Result<()> {
        let deadline = tokio::time::Instant::now() + REFRESH_TIMEOUT;
        loop {
            let current = self.current().await?;
            if current.revision != initial.revision {
                return self.install(&current.record);
            }
            let manager = self
                .prepare_refresh_manager(&current.record, initial.revision)
                .await?;
            let result = manager
                .refresh_token()
                .with_subscriber(tracing::subscriber::NoSubscriber::default())
                .await;
            match result {
                Ok(_) => return self.install(&self.current().await?.record),
                Err(AuthError::TokenRefreshRejected(_)) => {
                    self.handle_refresh_rejected(initial.revision).await?;
                }
                Err(AuthError::AuthorizationRequired) => {
                    self.handle_auth_required(&current.record, current.revision)
                        .await?;
                }
                Err(AuthError::CredentialStoreError(reason)) => {
                    self.handle_store_error(initial.revision, &reason, deadline)
                        .await?;
                }
                Err(_) => bail!("OAuth refresh temporarily unavailable; retry the call"),
            }
        }
    }

    async fn cleanup_needs_login(&self) -> Result<()> {
        let snapshot = self.store.inspect().await?;
        let Some(record) = snapshot.record else {
            return Ok(());
        };
        if record.connection_id != self.connection_id
            || record.status != CredentialStatus::NeedsLogin
        {
            return Ok(());
        }
        let principal = record
            .owner_user_id
            .map(Principal::User)
            .unwrap_or(Principal::Ownerless);
        self.factory
            .catalogs
            .for_principal(principal.clone())?
            .delete_if_connection(&self.connection_id)
            .await?;
        let audience = match principal {
            Principal::User(user) => HandleAudience::User(user),
            Principal::Ownerless => HandleAudience::OwnerlessOnly,
        };
        for _ in 0..CLEANUP_CAS_RETRIES {
            let Some(stored) = self.factory.handle.read(&audience).await? else {
                return Ok(());
            };
            match self
                .factory
                .handle
                .delete(&audience, stored.revision, &self.connection_id)
                .await
            {
                Ok(_) => return Ok(()),
                Err(RegistrationHandleError::Conflict) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("OAuth registration cleanup contended")
    }
}

#[async_trait]
impl UpstreamAuth for OAuthUpstreamAuth {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        if let Ok(header) = self.header.read() {
            if let Some(value) = &*header {
                config.custom_headers.insert(AUTHORIZATION, value.clone());
            }
        }
    }
    async fn ensure_fresh(&self) -> Result<()> {
        let current = self.current().await?;
        let CredentialMaterial::Oauth2(material) = &current.record.material else {
            bail!("connection superseded")
        };
        if material.access_expires_at.is_some_and(|expires| {
            expires <= Utc::now() + chrono::TimeDelta::seconds(PRE_EXPIRY_WINDOW_SECS)
        }) {
            return self.refresh(current).await;
        }
        self.install(&current.record)
    }
    fn refreshable(&self) -> bool {
        true
    }
    async fn refresh_after_rejection(&self, rejected: Option<&HeaderValue>) -> Result<()> {
        let current = self.current().await?;
        let CredentialMaterial::Oauth2(material) = &current.record.material else {
            bail!("connection superseded")
        };
        let expected = format!("Bearer {}", material.access_token.expose());
        if rejected.is_some_and(|rejected| rejected.as_bytes() != expected.as_bytes()) {
            return self.install(&current.record);
        }
        self.refresh(current).await
    }
    fn reconnect_message(&self) -> String {
        let name = &self.factory.config.server_name;
        format!("{name} credential expired; call {name}_connect")
    }

    /// Providers can echo tokens in response text, so redact both the bare
    /// token and its `Bearer <token>` header before exposing that text.
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let Ok(header) = self.header.read() else {
            return Cow::Borrowed(text);
        };
        crate::native::secret_redaction::redact_bearer(text, header.as_ref())
    }
}
