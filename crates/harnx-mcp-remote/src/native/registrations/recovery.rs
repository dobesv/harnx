//! Recover catalogs at startup and adopt live registrations from other instances.

use super::{Lifecycle, Superseded};
use anyhow::{anyhow, Result};
use harnx_credentials::{Catalog, Principal};

impl Lifecycle {
    /// Republish matching active catalogs without an upstream call.
    pub async fn recover_at_startup(&self) -> Result<()> {
        let mut failure = None;
        for principal in self.credentials.list_active_principals().await? {
            if let Err(error) = self.recover_registration(&principal).await {
                log::warn!("cannot recover a user registration: {error:#}");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    async fn recover_registration(&self, principal: &Principal) -> Result<()> {
        let Some(catalog) = self.recoverable_catalog(principal).await? else {
            return Ok(());
        };
        // Capability is unknown without a session; accepting the challenge
        // path keeps real upstream challenges from degrading to errors.
        if let Err(Superseded) = self
            .publish_registration(principal, &catalog.connection_id, &catalog.tools, true)
            .await
        {
            log::warn!("startup registration recovery skipped a superseded connection");
        }
        Ok(())
    }

    /// Every instance shares refresh responsibility for live registrations.
    /// Adoption never changes the payload or resurrects a deleted registration.
    // qual:allow(iosp) — Continue scanning independent principals after storage errors; each adoption retries on the next tick.
    pub(super) async fn adopt_registrations(&self) -> Result<()> {
        let mut failure = None;
        for principal in self.credentials.list_active_principals().await? {
            if self.managed.lock().await.contains_key(&principal) {
                continue;
            }
            if let Err(error) = self.adopt_registration(&principal).await {
                log::warn!("cannot adopt a user registration; will retry: {error:#}");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    // qual:allow(iosp) — Check the stored registration, catalog and authoritative credential together before recording revision-fenced local ownership.
    async fn adopt_registration(&self, principal: &Principal) -> Result<()> {
        let Some(stored) = self.handle.read(&Self::audience(principal)).await? else {
            return Ok(());
        };
        let Some(catalog) = self.recoverable_catalog(principal).await? else {
            return Ok(());
        };
        if stored.registration.connection_id.as_deref() == Some(&catalog.connection_id) {
            self.track_registration(principal, &catalog.connection_id, stored.revision)
                .await;
        }
        Ok(())
    }

    async fn recoverable_catalog(&self, principal: &Principal) -> Result<Option<Catalog>> {
        let catalogs = self.catalogs.for_principal(principal.clone())?;
        let Some(catalog) = catalogs.get().await? else {
            return Ok(None);
        };
        let credentials = self.credentials.for_principal(principal.clone())?;
        let credential = match credentials.get().await {
            Ok(Some(credential)) => credential,
            Ok(None)
            | Err(harnx_credentials::Error::Expired | harnx_credentials::Error::NeedsLogin) => {
                return Ok(None);
            }
            Err(error) => return Err(anyhow!(error)),
        };
        Ok(
            (catalog.record.connection_id == credential.record.connection_id)
                .then_some(catalog.record),
        )
    }
}
