//! Connection-fenced registration publication.

use super::{map_upstream_tool, Lifecycle, Superseded, CAS_ROUNDS};
use harnx_credentials::Principal;
use harnx_toolset::{Registration, TOOL_PROTOCOL_VERSION};
use harnx_toolset_server::{HandleAudience, RegistrationHandleError, StoredRegistration};

impl Lifecycle {
    /// The registration payload for one principal's catalog.
    fn registration_payload(
        &self,
        principal: &Principal,
        connection_id: &str,
        tools: &[rmcp::model::Tool],
        mrtr_capable: bool,
    ) -> Registration {
        let identity = self.identity.clone();
        let mut registration = Registration {
            package: identity.package,
            config: identity.config,
            server: self.config.server_name.clone(),
            tools: tools
                .iter()
                .map(|tool| map_upstream_tool(tool, mrtr_capable))
                .collect(),
            schema_version: harnx_toolset_server::TOOL_SCHEMA_VERSION,
            proto_version: TOOL_PROTOCOL_VERSION,
            user_id: None,
            ownerless_only: false,
            connection_id: Some(connection_id.to_string()),
        };
        match principal {
            Principal::User(user_id) => registration.user_id = Some(user_id.clone()),
            Principal::Ownerless => registration.ownerless_only = true,
        }
        registration
    }

    /// Publish or refresh one principal's registration. Overwrites an older
    /// connection's registration (the live credential is authoritative), but
    /// refuses to write anything once the credential moved on.
    // qual:allow(iosp) — Recheck the authoritative credential and connection_id inside every CAS retry before attempting a registration write.
    pub async fn publish_registration(
        &self,
        principal: &Principal,
        connection_id: &str,
        tools: &[rmcp::model::Tool],
        mrtr_capable: bool,
    ) -> Result<(), Superseded> {
        let audience = Self::audience(principal);
        let payload = self.registration_payload(principal, connection_id, tools, mrtr_capable);
        let store = self
            .credentials
            .for_principal(principal.clone())
            .map_err(|error| {
                log::warn!("cannot open the credential store for {principal:?}: {error}");
                Superseded
            })?;
        for _ in 0..CAS_ROUNDS {
            // Snapshot first: a reconnect after the credential check must
            // invalidate this exact revision, not hand us its new revision.
            let stored = self.handle.read(&audience).await.map_err(|error| {
                log::warn!("cannot read the stored registration: {error}");
                Superseded
            })?;
            let credential = match store.get().await {
                Ok(Some(credential)) => credential,
                Ok(None) => return Err(Superseded),
                Err(harnx_credentials::Error::Conflict) => continue,
                Err(error) => {
                    log::warn!("cannot read the credential for {principal:?}: {error}");
                    return Err(Superseded);
                }
            };
            if credential.record.connection_id != connection_id {
                return Err(Superseded);
            }
            #[cfg(test)]
            self.hooks
                .pause(super::tests::Point::PublicationChecked)
                .await;
            if let Some(revision) = self
                .try_publish_registration(&audience, &payload, stored.as_ref())
                .await?
            {
                self.track_registration(principal, connection_id, revision)
                    .await;
                return Ok(());
            }
        }
        Err(Superseded)
    }

    async fn try_publish_registration(
        &self,
        audience: &HandleAudience,
        payload: &Registration,
        stored: Option<&StoredRegistration>,
    ) -> Result<Option<u64>, Superseded> {
        let result = match stored {
            Some(stored) => {
                self.handle
                    .update(audience, payload.clone(), stored.revision)
                    .await
            }
            None => self.handle.publish(audience, payload.clone()).await,
        };
        match result {
            Ok(revision) => Ok(Some(revision)),
            Err(RegistrationHandleError::Conflict) => Ok(None),
            Err(error) => {
                log::warn!("cannot publish the user registration: {error}");
                Err(Superseded)
            }
        }
    }
}
