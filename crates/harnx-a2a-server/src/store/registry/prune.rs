//! Cleanup losing discovery hints, never retained winning message identities.
use super::*;

impl A2aStore {
    pub(crate) async fn prune_unused_registration(
        &self,
        entry: &RecoveryRegistration,
    ) -> Result<bool> {
        let storage = &entry.allocation.storage_key;
        if !self.registration_unused(entry).await? {
            return Ok(false);
        }
        let leases = harnx_runtime::nats_lease::ensure_lease_bucket(
            self.metadata().jetstream(),
            &harnx_runtime::nats_lease::NatsLeaseConfig {
                replicas: self.metadata().replicas(),
                ..Default::default()
            },
        )
        .await?;
        if harnx_nats_common::leader_reads::entry(&leases, &format!("sessions/{storage}/a2a/lock"))
            .await?
            .is_some_and(|lease| lease.operation == async_nats::jetstream::kv::Operation::Put)
        {
            return Ok(false);
        }
        let key = format!("a2a.registry.{}", sha256(storage));
        if let Some(saved) =
            harnx_nats_common::leader_reads::entry(self.metadata().kv_store(), &key).await?
        {
            self.metadata()
                .kv_store()
                .purge_expect_revision(key, Some(saved.revision))
                .await?;
        }
        Ok(true)
    }
}

impl A2aStore {
    async fn registration_unused(&self, entry: &RecoveryRegistration) -> Result<bool> {
        if chrono::Utc::now() - entry.allocation.created_at < chrono::Duration::seconds(300) {
            return Ok(false);
        }
        let storage = &entry.allocation.storage_key;
        if self.metadata().get(storage).await?.is_some() {
            return Ok(false);
        }
        if self.read_context(storage).await?.is_some() {
            return Ok(false);
        }
        let Some(first) = &entry.first else {
            return Ok(true);
        };
        match self
            .first_message_reservation(&first.identity, &first.fingerprint)
            .await
        {
            Ok(Some(winner)) => Ok(winner.allocation != entry.allocation),
            Ok(None) => Ok(true),
            Err(error)
                if error.downcast_ref::<super::super::StoreError>()
                    == Some(&super::super::StoreError::FingerprintMismatch) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}
