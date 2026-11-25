//! Credential connection and catalog publication.

use super::{filter_reserved, Lifecycle, Superseded, CAS_ROUNDS};
use crate::native::pool::list_all_tools;
use anyhow::{anyhow, Result};
use harnx_credentials::{Catalog, CredentialRecord, NewCredential, Principal, Snapshot, Versioned};

impl Lifecycle {
    /// Write the credential for a fresh connect. Assigns the new
    /// `connection_id`; a concurrent writer is resolved by the revision CAS.
    pub async fn connect_credential(
        &self,
        owner: &Principal,
        input: NewCredential,
    ) -> Result<Versioned<CredentialRecord>> {
        let store = self
            .credentials
            .for_principal(owner.clone())
            .map_err(|error| anyhow!("{error}"))?;
        for _ in 0..CAS_ROUNDS {
            let snapshot: Snapshot<CredentialRecord> =
                store.inspect().await.map_err(|error| anyhow!("{error}"))?;
            match store.put_connect(snapshot.revision, input.clone()).await {
                Ok(record) => return Ok(record),
                Err(harnx_credentials::Error::Conflict) => continue,
                Err(harnx_credentials::Error::Expired) => {
                    return Err(anyhow!("the requested expiry is in the past"))
                }
                Err(harnx_credentials::Error::InvalidRecord) => {
                    return Err(anyhow!(
                        "the requested expiry exceeds this server's credential TTL"
                    ))
                }
                Err(error) => return Err(anyhow!("{error}")),
            }
        }
        Err(anyhow!("credential write is contended; try again"))
    }

    /// The completing route's tail (decision 7 step 3): build the client,
    /// list tools, save the catalog and publish the registration, then mark
    /// the flow completed with the new connection.
    pub async fn finish_connection(
        &self,
        owner: &Principal,
        challenge: &str,
        credential: &Versioned<CredentialRecord>,
    ) -> Result<usize> {
        let connection_id = credential.record.connection_id.clone();
        let client = self.pool.client(owner.clone(), credential).await?;
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
        let catalog = self
            .publish_connection_catalog(owner, &connection_id, tools)
            .await?;
        self.publish_registration(owner, &connection_id, &catalog.tools, mrtr_capable)
            .await
            .map_err(|Superseded| anyhow!("superseded by a newer connection"))?;
        let flow = self
            .flows
            .get_by_challenge(challenge)
            .await
            .map_err(|error| anyhow!("{error}"))?
            .ok_or_else(|| anyhow!("connect flow disappeared"))?;
        self.flows
            .complete(challenge, flow.revision, connection_id)
            .await
            .map_err(|error| anyhow!("{error}"))?;
        Ok(catalog.tools.len())
    }

    async fn publish_connection_catalog(
        &self,
        owner: &Principal,
        connection_id: &str,
        tools: Vec<rmcp::model::Tool>,
    ) -> Result<Catalog> {
        // Catalog: CAS, and refuse if the credential was superseded while we
        // were listing (a newer reconnect wins).
        let catalogs = self
            .catalogs
            .for_principal(owner.clone())
            .map_err(|error| anyhow!("{error}"))?;
        let credentials = self
            .credentials
            .for_principal(owner.clone())
            .map_err(|error| anyhow!("{error}"))?;
        let catalog = Catalog {
            connection_id: connection_id.to_string(),
            tools,
        };
        let mut published = false;
        for _ in 0..CAS_ROUNDS {
            let snapshot = catalogs
                .inspect()
                .await
                .map_err(|error| anyhow!("{error}"))?;
            match catalogs
                .publish(&credentials, snapshot.revision, catalog.clone())
                .await
            {
                Ok(_) => {
                    published = true;
                    break;
                }
                Err(harnx_credentials::Error::Conflict) => continue,
                Err(harnx_credentials::Error::ConnectionMismatch) => {
                    return Err(anyhow!("superseded by a newer connection"));
                }
                Err(error) => return Err(anyhow!("{error}")),
            }
        }
        if !published {
            return Err(anyhow!("catalog write is contended; try again"));
        }
        Ok(catalog)
    }
}
