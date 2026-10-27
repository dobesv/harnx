//! Keep distinct error and retained-mapping checks on the same store fixture.
use super::*;
use harnx_a2a_server::store::{DedupeMappingKind, DedupeMappingTarget};

pub(super) struct DedupeCase<'a> {
    pub store: &'a A2aStore,
    pub export: &'a Export,
    pub key: &'a str,
    pub task_id: &'a str,
}

impl DedupeCase<'_> {
    pub(super) async fn assert_new_id_misses(&self, original: &str) -> Result<()> {
        assert!(self
            .store
            .dedupe_task(
                ContextAccess {
                    export: self.export,
                    owner: &alice().into(),
                    local_id: "abc123"
                },
                MessageIdentity {
                    message_id: "new",
                    fingerprint: original
                },
            )
            .await?
            .is_none());
        Ok(())
    }

    pub(super) async fn record_original(&self, original: &str) -> Result<()> {
        self.store
            .put_dedupe_mapping(
                DedupeMappingKind::Message,
                self.key,
                DedupeMappingTarget::new(
                    MessageIdentity {
                        message_id: "message",
                        fingerprint: original,
                    },
                    self.task_id,
                ),
            )
            .await
    }

    pub(super) async fn assert_mismatch_is_distinct(&self, changed: &str) -> Result<()> {
        let error = self
            .store
            .dedupe_task(
                ContextAccess {
                    export: self.export,
                    owner: &alice().into(),
                    local_id: "abc123",
                },
                MessageIdentity {
                    message_id: "message",
                    fingerprint: changed,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StoreError>(),
            Some(&StoreError::FingerprintMismatch)
        );
        Ok(())
    }

    pub(super) async fn assert_mapping_not_overwritten(
        &self,
        fingerprints: (&str, &str),
    ) -> Result<()> {
        let (original, changed) = fingerprints;
        assert!(self
            .store
            .put_dedupe_mapping(
                DedupeMappingKind::Message,
                self.key,
                DedupeMappingTarget::new(
                    MessageIdentity {
                        message_id: "message",
                        fingerprint: changed
                    },
                    self.task_id
                ),
            )
            .await
            .is_err());
        assert_eq!(
            self.store
                .get_message_dedupe(self.key, "message")
                .await?
                .unwrap()
                .fingerprint,
            original
        );
        Ok(())
    }
}
