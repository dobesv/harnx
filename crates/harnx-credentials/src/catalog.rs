//! Non-secret per-principal tool catalogs, fenced by connection ID and KV revision.

use async_nats::jetstream::{self, kv};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{
    backend,
    model::{server_segment, validate_server},
    CredentialStore, Error, Principal, Result, Snapshot, Versioned,
};

pub const CATALOGS_BUCKET: &str = "harnx_tool_user_catalogs";

/// Upstream tool definitions. Never include credentials or other secrets in tools.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub connection_id: String,
    pub tools: Vec<rmcp::model::Tool>,
}

// qual:allow(srp, god_struct) reason: "scoped key construction and KV mutations belong to one catalog handle"
#[derive(Clone)]
pub struct CatalogStore {
    kv: kv::Store,
    server: String,
    principal: Principal,
}

impl std::fmt::Debug for CatalogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogStore")
            .field("server", &self.server)
            .field("principal", &self.principal)
            .finish()
    }
}

impl CatalogStore {
    pub async fn open(
        js: &jetstream::Context,
        replicas: usize,
        server: impl Into<String>,
        principal: Principal,
    ) -> Result<Self> {
        let server = server.into();
        validate_server(&server)?;
        principal.validate()?;
        let kv = backend::ensure_bucket(js, CATALOGS_BUCKET, Duration::ZERO, replicas).await?;
        Ok(Self {
            kv,
            server,
            principal,
        })
    }

    pub fn for_principal(&self, principal: Principal) -> Result<Self> {
        principal.validate()?;
        Ok(Self {
            principal,
            ..self.clone()
        })
    }

    pub fn key(&self) -> String {
        format!(
            "catalog.v1.{}.{}",
            server_segment(&self.server),
            self.principal.key_segment()
        )
    }

    /// Read the tombstone revision as well as a live catalog.
    pub async fn inspect(&self) -> Result<Snapshot<Catalog>> {
        let Some(entry) = backend::entry(&self.kv, &self.key()).await? else {
            return Ok(Snapshot {
                revision: 0,
                record: None,
            });
        };
        let record = if entry.operation == kv::Operation::Put {
            Some(serde_json::from_slice(&entry.value).map_err(|_| Error::InvalidRecord)?)
        } else {
            None
        };
        Ok(Snapshot {
            revision: entry.revision,
            record,
        })
    }

    pub async fn get(&self) -> Result<Option<Versioned<Catalog>>> {
        Ok(self.inspect().await?.into_versioned())
    }

    /// Initial publish or reconnect replacement. Re-check the authoritative credential
    /// on every retry; a completed-but-superseded flow may not publish derived state.
    pub async fn publish(
        &self,
        credentials: &CredentialStore,
        expected_revision: u64,
        catalog: Catalog,
    ) -> Result<Versioned<Catalog>> {
        if credentials.server() != self.server || credentials.principal() != &self.principal {
            return Err(Error::ConnectionMismatch);
        }
        let credential = credentials.get().await?.ok_or(Error::NotFound)?;
        if credential.record.connection_id != catalog.connection_id {
            return Err(Error::ConnectionMismatch);
        }
        self.write(expected_revision, catalog).await
    }

    /// `list_changed`: never let an old client's notification replace a newer catalog.
    pub async fn update(
        &self,
        expected_revision: u64,
        connection_id: &str,
        tools: Vec<rmcp::model::Tool>,
    ) -> Result<Versioned<Catalog>> {
        let current = self.inspect().await?;
        if current.revision != expected_revision {
            return Err(Error::Conflict);
        }
        let current = current.record.ok_or(Error::NotFound)?;
        if current.connection_id != connection_id {
            return Err(Error::ConnectionMismatch);
        }
        self.write(
            expected_revision,
            Catalog {
                connection_id: connection_id.into(),
                tools,
            },
        )
        .await
    }

    /// Disconnect cleanup. Conflicts re-read and re-check, leaving a newer connection alone.
    pub async fn delete_if_connection(&self, connection_id: &str) -> Result<bool> {
        loop {
            let current = self.inspect().await?;
            let Some(catalog) = current.record else {
                return Ok(false);
            };
            if catalog.connection_id != connection_id {
                return Ok(false);
            }
            match backend::delete(&self.kv, &self.key(), current.revision).await {
                Ok(()) => return Ok(true),
                Err(Error::Conflict) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn write(&self, revision: u64, catalog: Catalog) -> Result<Versioned<Catalog>> {
        if catalog.connection_id.is_empty() {
            return Err(Error::InvalidRecord);
        }
        let bytes = serde_json::to_vec(&catalog).map_err(|_| Error::InvalidRecord)?;
        let revision = backend::update(&self.kv, &self.key(), bytes, revision).await?;
        Ok(Versioned {
            revision,
            record: catalog,
        })
    }
}
