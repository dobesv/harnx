use super::*;
use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;

mod activity;
mod extension_validation;
mod extensions;
mod keys;
mod lookup;
mod mutation;

pub(super) use extension_validation::validate_extensions;
pub use extensions::SessionExtensionUpdate;
pub use keys::{activity_key, invalidation_subject, metadata_key, read_cursor_key, session_prefix};
pub(super) use lookup::metadata_belongs_to_agent;
pub(in crate::nats_session_metadata) use mutation::PatchGuard;

#[derive(Clone, Debug)]
pub struct SessionMetadataStore {
    store: kv::Store,
    client: async_nats::Client,
}

impl SessionMetadataStore {
    pub async fn ensure(jetstream: &jetstream::Context, replicas: usize) -> Result<Self> {
        let create = jetstream
            .create_key_value(kv::Config {
                bucket: SESSION_METADATA_BUCKET.to_string(),
                history: 1,
                num_replicas: replicas,
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await;
        let store = match create {
            Ok(store) => store,
            Err(_) => {
                if let Err(error) = harnx_nats_common::registry::reconcile_bucket_replicas(
                    jetstream,
                    SESSION_METADATA_BUCKET,
                    replicas,
                )
                .await
                {
                    log::warn!(
                        "could not reconcile replicas for bucket '{SESSION_METADATA_BUCKET}': {error:#}"
                    );
                }
                jetstream
                    .get_key_value(SESSION_METADATA_BUCKET)
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| {
                        format!(
                            "Failed to open session metadata bucket '{SESSION_METADATA_BUCKET}'"
                        )
                    })?
            }
        };
        Ok(Self {
            store,
            client: jetstream.client().clone(),
        })
    }

    pub fn from_store(store: kv::Store, client: async_nats::Client) -> Self {
        Self { store, client }
    }

    pub fn kv_store(&self) -> &kv::Store {
        &self.store
    }

    pub async fn create(&self, metadata: &SessionMetadata) -> Result<Option<u64>> {
        metadata.validate(&metadata.session_id)?;
        let key = metadata_key(&metadata.session_id);
        let payload = serde_json::to_vec(metadata).with_context(|| {
            format!(
                "Failed to serialize session metadata '{}'",
                metadata.session_id
            )
        })?;
        match self.store.create(&key, payload.into()).await {
            Ok(revision) => {
                if let Err(error) = self.ensure_reserved_activity(&metadata.session_id).await {
                    // Metadata + activity span two KV keys. Roll back only the
                    // exact revision we created so a concurrent winner or patch
                    // can never be deleted by this failed reservation.
                    if let Err(rollback_error) = self
                        .store
                        .delete_expect_revision(&key, Some(revision))
                        .await
                    {
                        log::warn!(
                            "failed to roll back incomplete session metadata: key={key} revision={revision} error={rollback_error:#}"
                        );
                    }
                    return Err(error);
                }
                Ok(Some(revision))
            }
            Err(error) if error.kind() == kv::CreateErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to create session metadata key '{key}'")),
        }
    }

    pub async fn get(&self, session_id: &str) -> Result<Option<MetadataRecord>> {
        let key = metadata_key(session_id);
        match self.store.entry(key.clone()).await {
            Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
                let metadata: SessionMetadata = serde_json::from_slice(&entry.value)
                    .with_context(|| format!("Failed to deserialize session metadata '{key}'"))?;
                metadata.validate(session_id)?;
                Ok(Some(MetadataRecord {
                    metadata,
                    revision: entry.revision,
                }))
            }
            Ok(Some(_)) | Ok(None) => Ok(None),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to read session metadata key '{key}'")),
        }
    }

    pub async fn list(&self) -> Result<Vec<ListedSession>> {
        let mut keys = self.store.keys().await.map_err(anyhow::Error::from)?;
        let mut sessions = Vec::new();
        while let Some(key) = keys.next().await {
            let key = key.map_err(anyhow::Error::from)?;
            let Some(session_id) = key
                .strip_prefix("sessions/")
                .and_then(|key| key.strip_suffix("/meta"))
            else {
                continue;
            };
            match self.get(session_id).await {
                Ok(Some(record)) => {
                    let activity = match self.get_activity(session_id).await {
                        Ok(activity) => activity,
                        Err(error) => {
                            log::warn!(
                                "could not read session activity; using metadata timestamp: bucket={} key={} error={error:#}",
                                SESSION_METADATA_BUCKET,
                                activity_key(session_id)
                            );
                            None
                        }
                    };
                    sessions.push(ListedSession {
                        activity,
                        metadata: record.metadata,
                        metadata_revision: record.revision,
                    });
                }
                Ok(None) => {}
                Err(error) => log::warn!(
                    "skipping invalid session metadata: bucket={} key={} error={error:#}",
                    SESSION_METADATA_BUCKET,
                    key
                ),
            }
        }
        sessions.sort_by(|left, right| {
            let left_activity = left
                .activity
                .as_ref()
                .map(|activity| activity.last_activity_at)
                .unwrap_or(left.metadata.created_at);
            let right_activity = right
                .activity
                .as_ref()
                .map(|activity| activity.last_activity_at)
                .unwrap_or(right.metadata.created_at);
            right_activity
                .cmp(&left_activity)
                .then_with(|| right.metadata.session_id.cmp(&left.metadata.session_id))
        });
        Ok(sessions)
    }

    pub async fn purge_session_prefix(&self, session_id: &str) -> Result<usize> {
        let prefix = session_prefix(session_id);
        let mut keys = self.store.keys().await.map_err(anyhow::Error::from)?;
        let mut deleted = 0;
        while let Some(key) = keys.next().await {
            let key = key.map_err(anyhow::Error::from)?;
            if key == prefix || key.starts_with(&format!("{prefix}/")) {
                self.store.purge(&key).await?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}
