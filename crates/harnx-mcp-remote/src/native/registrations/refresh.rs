//! Registration refresh retains work on storage errors and fences local removals.

use super::{bookkeeping::ManagedRegistration, Lifecycle};
use anyhow::Result;
use harnx_credentials::{Error, Principal};
use harnx_toolset_server::RegistrationHandleError;

impl Lifecycle {
    /// Refresh all managed registrations, then adopt live registrations for
    /// active connections established by other instances. Never recreate a
    /// missing registration during a tick; startup recovery handles that.
    // qual:allow(iosp) — Process every principal despite transient failures and conditionally forget only the exact ownership snapshot that was checked.
    pub async fn refresh_tick(&self) -> Result<()> {
        let snapshot: Vec<_> = self
            .managed
            .lock()
            .await
            .iter()
            .map(|(principal, tracked)| (principal.clone(), tracked.clone()))
            .collect();
        #[cfg(test)]
        self.hooks.pause(super::tests::Point::RefreshSnapshot).await;
        let mut failure = None;
        for (principal, tracked) in snapshot {
            match self.refresh_registration(&principal, &tracked).await {
                Ok(true) => {}
                Ok(false) => self.forget_registration(&principal, &tracked).await,
                Err(error) => {
                    log::warn!("cannot refresh a user registration; will retry: {error:#}");
                    failure.get_or_insert(error);
                }
            }
        }
        if let Err(error) = self.adopt_registrations().await {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }

    // qual:allow(iosp) — Keep registration snapshot, authoritative connection check and CAS together so reconnects invalidate a stale refresh.
    async fn refresh_registration(
        &self,
        principal: &Principal,
        tracked: &ManagedRegistration,
    ) -> Result<bool> {
        let audience = Self::audience(principal);
        let Some(stored) = self.handle.read(&audience).await? else {
            return Ok(false);
        };
        let store = self.credentials.for_principal(principal.clone())?;
        let credential = match store.get().await {
            Ok(Some(credential)) => credential,
            Ok(None) | Err(Error::Expired | Error::NeedsLogin) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if credential.record.connection_id != tracked.connection_id
            || stored.registration.connection_id.as_deref() != Some(&tracked.connection_id)
        {
            return Ok(false);
        }
        match self
            .handle
            .update(&audience, stored.registration, stored.revision)
            .await
        {
            Ok(_) | Err(RegistrationHandleError::Conflict) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
}
