//! Disconnect and credential status views.

use super::{Lifecycle, CAS_ROUNDS};
use anyhow::{anyhow, Result};
use harnx_credentials::Principal;
use harnx_toolset_server::ConditionalDelete;

pub enum DisconnectOutcome {
    NotConnected,
    Disconnected { account_label: Option<String> },
}

pub enum StatusView {
    Disconnected,
    Connected {
        account_label: Option<String>,
        expires_at: chrono::DateTime<chrono::Utc>,
        needs_login: bool,
    },
}

impl Lifecycle {
    /// Disconnect (decision 5): CAS-delete the credential first, then remove
    /// derived state only where it still belongs to that connection.
    pub async fn disconnect(&self, principal: &Principal) -> Result<DisconnectOutcome> {
        let store = self
            .credentials
            .for_principal(principal.clone())
            .map_err(|error| anyhow!("{error}"))?;
        let snapshot = store.inspect().await.map_err(|error| anyhow!("{error}"))?;
        let Some(record) = snapshot.record else {
            self.pool.drop_principal(principal).await;
            return Ok(DisconnectOutcome::NotConnected);
        };
        let connection_id = record.connection_id.clone();
        let account_label = record.account_label.clone();
        match store.delete(snapshot.revision).await {
            Ok(()) => {}
            Err(harnx_credentials::Error::Conflict) => {
                return Err(anyhow!(
                    "the connection changed while disconnecting; call {}_disconnect again",
                    self.config.server_name
                ))
            }
            Err(error) => return Err(anyhow!("{error}")),
        }
        #[cfg(test)]
        self.hooks
            .pause(super::tests::Point::CredentialDeleted)
            .await;
        self.forget_connection(principal, &connection_id).await;
        let catalogs = self
            .catalogs
            .for_principal(principal.clone())
            .map_err(|error| anyhow!("{error}"))?;
        catalogs
            .delete_if_connection(&connection_id)
            .await
            .map_err(|error| anyhow!("{error}"))?;
        self.delete_registration_for_connection(principal, &connection_id)
            .await?;
        self.pool.drop_principal(principal).await;
        Ok(DisconnectOutcome::Disconnected { account_label })
    }

    /// The management view for `credential_status`. Never includes material.
    pub async fn status(&self, principal: &Principal) -> StatusView {
        let Ok(store) = self.credentials.for_principal(principal.clone()) else {
            return StatusView::Disconnected;
        };
        let Ok(snapshot) = store.inspect().await else {
            return StatusView::Disconnected;
        };
        match snapshot.record {
            None => StatusView::Disconnected,
            Some(record) => StatusView::Connected {
                account_label: record.account_label,
                expires_at: record.expires_at,
                needs_login: record.status != harnx_credentials::CredentialStatus::Active,
            },
        }
    }

    // qual:allow(iosp) — CAS deletion must reread the stored revision after a conflict and stop when connection ownership changes.
    async fn delete_registration_for_connection(
        &self,
        principal: &Principal,
        connection_id: &str,
    ) -> Result<()> {
        let audience = Self::audience(principal);
        for _ in 0..CAS_ROUNDS {
            let stored = match self.handle.read(&audience).await {
                Ok(stored) => stored,
                Err(harnx_toolset_server::RegistrationHandleError::Conflict) => continue,
                Err(error) => return Err(error.into()),
            };
            let Some(stored) = stored else {
                break;
            };
            match self
                .handle
                .delete(&audience, stored.revision, connection_id)
                .await
            {
                Ok(ConditionalDelete::Deleted) => break,
                Ok(ConditionalDelete::ConnectionMismatch) => break,
                Err(harnx_toolset_server::RegistrationHandleError::Conflict) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
