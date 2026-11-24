//! Catalog notifications publish registrations only after a successful catalog CAS.

use super::{filter_reserved, Lifecycle, Superseded, CAS_ROUNDS};
use crate::native::pool::list_all_tools;
use anyhow::{anyhow, Result};
use harnx_credentials::Principal;

impl Lifecycle {
    /// `tools/list_changed` for one principal (decision 9): re-list under the
    /// same fencing rules and republish. A notification from an old
    /// connection can never replace a newer catalog.
    pub async fn apply_list_changed(&self, principal: Principal, connection_id: String) {
        let outcome = self
            .apply_list_changed_inner(&principal, &connection_id)
            .await;
        match outcome {
            Ok(()) => {}
            Err(error) => {
                log::warn!(
                    "updating user tool list for a changed upstream catalog failed: {error:#}"
                );
            }
        }
    }

    async fn apply_list_changed_inner(
        &self,
        principal: &Principal,
        connection_id: &str,
    ) -> Result<()> {
        let (credential, client) = self.upstream(principal).await?;
        if credential.record.connection_id != connection_id {
            return Ok(()); // stale notification; a newer connection owns this
        }
        let mrtr_capable = client
            .negotiated
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        let tools = list_all_tools(&client.peer).await?;
        let (tools, skipped) = filter_reserved(tools);
        for name in skipped {
            log::warn!(
                "upstream tool '{name}' collides with a reserved management tool name on {}; it is excluded from the user catalog",
                self.config.server_name
            );
        }
        self.republish_catalog(principal, connection_id, tools, mrtr_capable)
            .await
    }

    pub(super) async fn republish_catalog(
        &self,
        principal: &Principal,
        connection_id: &str,
        tools: Vec<rmcp::model::Tool>,
        mrtr_capable: bool,
    ) -> Result<()> {
        let catalogs = self
            .catalogs
            .for_principal(principal.clone())
            .map_err(|error| anyhow!("{error}"))?;
        for _ in 0..CAS_ROUNDS {
            let snapshot = catalogs
                .inspect()
                .await
                .map_err(|error| anyhow!("{error}"))?;
            #[cfg(test)]
            self.hooks.pause(super::tests::Point::CatalogSnapshot).await;
            match catalogs
                .update(snapshot.revision, connection_id, tools.clone())
                .await
            {
                Ok(_) => {
                    return self
                        .publish_registration(principal, connection_id, &tools, mrtr_capable)
                        .await
                        .map_err(|Superseded| anyhow!("credential moved on during republish"));
                }
                Err(harnx_credentials::Error::Conflict) => continue,
                Err(harnx_credentials::Error::ConnectionMismatch) => return Ok(()),
                Err(error) => return Err(anyhow!("{error}")),
            }
        }
        Err(anyhow!("catalog write is contended; try again"))
    }
}
