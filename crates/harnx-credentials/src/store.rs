//! Encrypted credentials scoped to one server and principal.

use async_nats::jetstream;
use chrono::{TimeDelta, Utc};
use std::{sync::Arc, time::Duration};

use crate::{
    backend::{self, EncryptedBucket},
    model::{server_segment, validate_server},
    CredentialMaterial, CredentialRecord, CredentialStatus, Error, Keyring, NewCredential,
    Principal, Result,
};

pub const CREDENTIALS_BUCKET: &str = "harnx_tool_credentials";
pub const DEFAULT_CREDENTIAL_TTL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// A present record and the KV sequence used for its next CAS mutation.
#[derive(Clone, Debug)]
pub struct Versioned<T> {
    pub revision: u64,
    pub record: T,
}

/// Includes a deletion's revision, so reconnect can CAS over a tombstone.
#[derive(Clone, Debug)]
pub struct Snapshot<T> {
    pub revision: u64,
    pub record: Option<T>,
}

impl<T> Snapshot<T> {
    pub fn into_versioned(self) -> Option<Versioned<T>> {
        self.record.map(|record| Versioned {
            revision: self.revision,
            record,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CredentialStore {
    pub(crate) bucket: EncryptedBucket,
    server: String,
    principal: Principal,
    ttl: Duration,
}

impl CredentialStore {
    pub async fn open(
        js: &jetstream::Context,
        replicas: usize,
        keys: Arc<Keyring>,
        server: impl Into<String>,
        principal: Principal,
    ) -> Result<Self> {
        let server = server.into();
        validate_server(&server)?;
        principal.validate()?;
        let kv = backend::ensure_bucket(js, CREDENTIALS_BUCKET, Duration::ZERO, replicas).await?;
        Ok(Self {
            bucket: EncryptedBucket { kv, keys },
            server,
            principal,
            ttl: DEFAULT_CREDENTIAL_TTL,
        })
    }

    /// Change the maximum/default lifetime for future connects. Existing expiry never moves.
    pub fn with_credential_ttl(mut self, ttl: Duration) -> Result<Self> {
        if ttl.is_zero() || TimeDelta::from_std(ttl).is_err() {
            return Err(Error::InvalidRecord);
        }
        self.ttl = ttl;
        Ok(self)
    }

    /// Reuse this server's keyring and bucket for another principal.
    pub fn for_principal(&self, principal: Principal) -> Result<Self> {
        principal.validate()?;
        Ok(Self {
            principal,
            ..self.clone()
        })
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Full key, useful when binding derived state or diagnosing encrypted storage.
    pub fn key(&self) -> String {
        format!(
            "v1.{}.{}",
            server_segment(&self.server),
            self.principal.key_segment()
        )
    }

    /// Management-only snapshot, including expired/needs-login records and tombstones.
    /// Never use it to authorize an upstream request; use `get` instead.
    pub async fn inspect(&self) -> Result<Snapshot<CredentialRecord>> {
        let snapshot: Snapshot<CredentialRecord> = self.bucket.read(&self.key()).await?;
        if let Some(record) = &snapshot.record {
            self.validate_record(record)?;
        }
        Ok(snapshot)
    }

    /// Resolve an active, unexpired credential. Expired material is never returned.
    pub async fn get(&self) -> Result<Option<Versioned<CredentialRecord>>> {
        let snapshot = self.inspect().await?;
        if let Some(record) = &snapshot.record {
            record.ensure_active()?;
        }
        Ok(snapshot.into_versioned())
    }

    /// Connect/reconnect is always a CAS and always assigns a fresh connection ID.
    pub async fn put_connect(
        &self,
        expected_revision: u64,
        input: NewCredential,
    ) -> Result<Versioned<CredentialRecord>> {
        let now = Utc::now();
        let maximum = now
            .checked_add_signed(TimeDelta::from_std(self.ttl).map_err(|_| Error::InvalidRecord)?)
            .ok_or(Error::InvalidRecord)?;
        let expires_at = input.expires_at.unwrap_or(maximum);
        if expires_at <= now {
            return Err(Error::Expired);
        }
        if expires_at > maximum {
            return Err(Error::InvalidRecord);
        }
        let record = CredentialRecord {
            schema_version: 1,
            owner_user_id: self.principal.user_id().map(str::to_owned),
            server: self.server.clone(),
            created_at: now,
            updated_at: now,
            expires_at,
            status: CredentialStatus::Active,
            account_label: input.account_label,
            connection_id: uuid::Uuid::new_v4().to_string(),
            material: input.material,
        };
        self.save(record, expected_revision).await
    }

    /// CAS token rotation. Absolute expiry and connection ID come from the stored record,
    /// never from the caller. Prefer `RefreshLease::commit` for OAuth refresh exchanges.
    pub async fn put_refresh(
        &self,
        expected_revision: u64,
        material: CredentialMaterial,
    ) -> Result<Versioned<CredentialRecord>> {
        let mut current = self.at_revision(expected_revision).await?;
        current.ensure_active()?;
        current.material = preserve_refresh_token(&current.material, material);
        current.updated_at = Utc::now();
        self.save(current, expected_revision).await
    }

    pub async fn mark_needs_login(
        &self,
        expected_revision: u64,
    ) -> Result<Versioned<CredentialRecord>> {
        let mut current = self.at_revision(expected_revision).await?;
        current.status = CredentialStatus::NeedsLogin;
        current.updated_at = Utc::now();
        self.save(current, expected_revision).await
    }

    /// Disconnect at a previously read revision. Stale deletion can't remove a reconnect.
    pub async fn delete(&self, expected_revision: u64) -> Result<()> {
        backend::delete(&self.bucket.kv, &self.key(), expected_revision).await
    }

    /// Active credentials for this user within this server's keyring scope (zero or one).
    /// Different servers use separate keyrings and must be queried separately.
    // qual:api
    pub async fn list_for_user(
        &self,
        principal: &Principal,
    ) -> Result<Vec<Versioned<CredentialRecord>>> {
        Ok(self
            .for_principal(principal.clone())?
            .get()
            .await?
            .into_iter()
            .collect())
    }

    /// Active, unexpired principals for startup catalog/registration recovery.
    pub async fn list_active_principals(&self) -> Result<Vec<Principal>> {
        let prefix = format!("v1.{}.", server_segment(&self.server));
        let mut principals = Vec::new();
        for key in backend::keys(&self.bucket.kv).await? {
            if !key.starts_with(&prefix) {
                continue;
            }
            let snapshot: Snapshot<CredentialRecord> = self.bucket.read(&key).await?;
            if let Some(record) = snapshot.record {
                let principal = record
                    .owner_user_id
                    .clone()
                    .map(Principal::User)
                    .unwrap_or(Principal::Ownerless);
                let scoped = self.for_principal(principal.clone())?;
                if scoped.key() != key {
                    return Err(Error::InvalidRecord);
                }
                scoped.validate_record(&record)?;
                if record.ensure_active().is_ok() {
                    principals.push(principal);
                }
            }
        }
        Ok(principals)
    }

    async fn at_revision(&self, revision: u64) -> Result<CredentialRecord> {
        let snapshot = self.inspect().await?;
        if snapshot.revision != revision {
            return Err(Error::Conflict);
        }
        snapshot.record.ok_or(Error::NotFound)
    }

    async fn save(
        &self,
        record: CredentialRecord,
        revision: u64,
    ) -> Result<Versioned<CredentialRecord>> {
        self.validate_record(&record)?;
        let revision = self.bucket.write(&self.key(), &record, revision).await?;
        Ok(Versioned { revision, record })
    }

    fn validate_record(&self, record: &CredentialRecord) -> Result<()> {
        if record.schema_version != 1
            || record.server != self.server
            || record.owner_user_id.as_deref() != self.principal.user_id()
            || record.connection_id.is_empty()
        {
            return Err(Error::InvalidRecord);
        }
        Ok(())
    }
}

fn preserve_refresh_token(
    previous: &CredentialMaterial,
    mut next: CredentialMaterial,
) -> CredentialMaterial {
    if let (CredentialMaterial::Oauth2(old), CredentialMaterial::Oauth2(new)) =
        (previous, &mut next)
    {
        if new.refresh_token.is_none() {
            new.refresh_token = old.refresh_token.clone();
        }
    }
    next
}
