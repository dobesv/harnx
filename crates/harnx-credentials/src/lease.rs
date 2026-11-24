//! Cross-instance refresh coordination. Hold the lease across token exchange and save.

use async_nats::jetstream::{self, kv};
use std::time::Duration;
use tokio::time::Instant;

use crate::{
    backend, model::server_segment, CredentialMaterial, CredentialRecord, CredentialStore, Error,
    Result, Versioned,
};

pub const LOCKS_BUCKET: &str = "harnx_tool_credential_locks";
pub const REFRESH_LEASE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct RefreshLeaseStore {
    kv: kv::Store,
    ttl: Duration,
}

impl std::fmt::Debug for RefreshLeaseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshLeaseStore")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl RefreshLeaseStore {
    pub async fn open(js: &jetstream::Context, replicas: usize) -> Result<Self> {
        Self::open_with_ttl(js, replicas, REFRESH_LEASE_TTL).await
    }

    /// Use a shorter exchange budget if needed. Every instance must agree on this TTL;
    /// opening an existing bucket with a different TTL fails instead of changing live leases.
    pub async fn open_with_ttl(
        js: &jetstream::Context,
        replicas: usize,
        ttl: Duration,
    ) -> Result<Self> {
        if ttl.is_zero() || ttl > REFRESH_LEASE_TTL {
            return Err(Error::InvalidRecord);
        }
        let kv = backend::ensure_bucket(js, LOCKS_BUCKET, ttl, replicas).await?;
        Ok(Self { kv, ttl })
    }

    /// Acquire once; `LeaseBusy` asks the caller to re-read/wait, never refresh in parallel.
    /// The credential revision is read only after owning the lease.
    pub async fn acquire(&self, credentials: &CredentialStore) -> Result<RefreshLease> {
        let deadline = Instant::now() + self.ttl;
        let key = format!(
            "lock.v1.{}.{}",
            server_segment(credentials.server()),
            credentials.principal().key_segment()
        );
        let previous = backend::entry(&self.kv, &key).await?;
        if previous
            .as_ref()
            .is_some_and(|entry| entry.operation == kv::Operation::Put)
        {
            return Err(Error::LeaseBusy);
        }
        let previous_revision = previous.map_or(0, |entry| entry.revision);
        let owner = uuid::Uuid::new_v4().to_string().into_bytes();
        let revision = backend::update(&self.kv, &key, owner, previous_revision)
            .await
            .map_err(|error| {
                if error == Error::Conflict {
                    Error::LeaseBusy
                } else {
                    error
                }
            })?;
        let mut lease = RefreshLease {
            kv: self.kv.clone(),
            key,
            revision,
            deadline,
            credentials: credentials.clone(),
            credential: None,
            released: false,
        };
        lease.credential = Some(credentials.get().await?.ok_or(Error::NotFound)?);
        lease.ensure_valid().await?;
        Ok(lease)
    }
}

/// Not Clone: one owner, one commit. Drop releases best-effort; TTL handles process death.
/// Reconnect/disconnect don't need this lease: the captured credential KV revision fences them.
pub struct RefreshLease {
    kv: kv::Store,
    key: String,
    revision: u64,
    deadline: Instant,
    credentials: CredentialStore,
    credential: Option<Versioned<CredentialRecord>>,
    released: bool,
}

impl std::fmt::Debug for RefreshLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshLease")
            .field("revision", &self.revision)
            .field("credential", &self.credential)
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

impl RefreshLease {
    /// Snapshot used for the exchange and the only revision a commit may save against.
    pub fn credential(&self) -> Result<&Versioned<CredentialRecord>> {
        self.credential.as_ref().ok_or(Error::LeaseLost)
    }

    /// Save rotated tokens without extending absolute expiry or changing connection ID.
    /// If the lease expires or the record changed, discard the provider's response and reload.
    pub async fn commit(
        mut self,
        material: CredentialMaterial,
    ) -> Result<Versioned<CredentialRecord>> {
        self.ensure_valid().await?;
        let revision = self.credential()?.revision;
        let result = tokio::time::timeout_at(
            self.deadline,
            self.credentials.put_refresh(revision, material),
        )
        .await
        .map_err(|_| Error::LeaseLost)?;
        // Cleanup failure must not hide an acknowledged credential write; TTL releases the lock.
        let _ = self.release_inner().await;
        result
    }

    /// Definitive invalid_grant only. Still fenced by the acquisition revision and lease.
    pub async fn mark_needs_login(mut self) -> Result<Versioned<CredentialRecord>> {
        self.ensure_valid().await?;
        let revision = self.credential()?.revision;
        let result =
            tokio::time::timeout_at(self.deadline, self.credentials.mark_needs_login(revision))
                .await
                .map_err(|_| Error::LeaseLost)?;
        // Cleanup failure must not hide an acknowledged credential write; TTL releases the lock.
        let _ = self.release_inner().await;
        result
    }

    pub async fn release(mut self) -> Result<()> {
        self.release_inner().await
    }

    pub(crate) async fn ensure_valid(&self) -> Result<()> {
        if self.released || Instant::now() >= self.deadline {
            return Err(Error::LeaseLost);
        }
        let entry = backend::entry(&self.kv, &self.key)
            .await?
            .ok_or(Error::LeaseLost)?;
        if entry.revision != self.revision
            || entry.operation != kv::Operation::Put
            || Instant::now() >= self.deadline
        {
            return Err(Error::LeaseLost);
        }
        Ok(())
    }

    async fn release_inner(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        match backend::delete(&self.kv, &self.key, self.revision).await {
            Err(Error::Conflict) => Ok(()),
            result => result,
        }
    }
}

impl Drop for RefreshLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (kv, key, revision) = (self.kv.clone(), self.key.clone(), self.revision);
            runtime.spawn(async move {
                let _ = backend::delete(&kv, &key, revision).await;
            });
        }
    }
}
