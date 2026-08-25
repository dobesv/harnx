use super::{mutation::is_wrong_last_revision, *};
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use chrono::Utc;

impl SessionMetadataStore {
    pub async fn get_activity(&self, session_id: &str) -> Result<Option<SessionActivity>> {
        let key = activity_key(session_id);
        match self.store.entry(key.clone()).await {
            Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
                serde_json::from_slice(&entry.value)
                    .with_context(|| format!("Failed to deserialize session activity '{key}'"))
                    .map(Some)
            }
            Ok(Some(_)) | Ok(None) => Ok(None),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to read session activity key '{key}'")),
        }
    }

    /// Reads lifecycle state for retention cleanup without allowing malformed
    /// activity JSON to exempt a session from cleanup forever. Transport and
    /// JetStream errors remain fatal so cleanup never deletes through an
    /// uncertain store read.
    pub(crate) async fn get_activity_for_cleanup(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionActivity>> {
        let key = activity_key(session_id);
        match self.store.entry(key.clone()).await {
            Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
                match serde_json::from_slice(&entry.value) {
                    Ok(activity) => Ok(Some(activity)),
                    Err(error) => {
                        log::warn!(
                            "ignoring malformed session activity during retention cleanup: key={key} error={error}"
                        );
                        Ok(None)
                    }
                }
            }
            Ok(Some(_)) | Ok(None) => Ok(None),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to read session activity key '{key}'")),
        }
    }

    pub async fn mark_activated(&self, session_id: &str) -> Result<u64> {
        self.update_activity(session_id, true).await
    }

    pub async fn touch_activity(&self, session_id: &str) -> Result<u64> {
        self.update_activity(session_id, false).await
    }

    async fn update_activity(&self, session_id: &str, activation: bool) -> Result<u64> {
        let key = activity_key(session_id);
        for attempt in 0..CAS_RETRY_LIMIT {
            let (previous, revision) = self.activity_snapshot(&key).await?;
            let activity = next_activity(previous, activation);
            match self.write_activity(&key, &activity, revision).await {
                Ok(revision) => return Ok(revision),
                Err(error) if is_cas_conflict(&error) && attempt + 1 < CAS_RETRY_LIMIT => {
                    tokio::task::yield_now().await;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to update session activity '{key}'"))
                }
            }
        }
        unreachable!("bounded CAS loop always returns")
    }

    async fn activity_snapshot(&self, key: &str) -> Result<(Option<SessionActivity>, u64)> {
        let Some(entry) = self.store.entry(key.to_string()).await? else {
            return Ok((None, 0));
        };
        if !matches!(entry.operation, kv::Operation::Put) {
            return Ok((None, 0));
        }
        let activity = serde_json::from_slice(&entry.value)
            .with_context(|| format!("Failed to deserialize session activity '{key}'"))?;
        Ok((Some(activity), entry.revision))
    }

    async fn write_activity(
        &self,
        key: &str,
        activity: &SessionActivity,
        revision: u64,
    ) -> Result<u64> {
        let payload = serde_json::to_vec(activity)?;
        if revision == 0 {
            return self
                .store
                .create(key, payload.into())
                .await
                .map_err(anyhow::Error::from);
        }
        self.store
            .update(key, payload.into(), revision)
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn ensure_reserved_activity(&self, session_id: &str) -> Result<()> {
        let key = activity_key(session_id);
        let payload = serde_json::to_vec(&SessionActivity::reserved())?;
        match self.store.create(&key, payload.into()).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == kv::CreateErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to create session activity key '{key}'")),
        }
    }
}

fn next_activity(previous: Option<SessionActivity>, activation: bool) -> SessionActivity {
    if activation {
        return SessionActivity::activated(previous);
    }
    SessionActivity {
        first_activation_at: previous.and_then(|activity| activity.first_activation_at),
        last_activity_at: Utc::now(),
    }
}

fn is_cas_conflict(error: &anyhow::Error) -> bool {
    is_wrong_last_revision(error)
        || error
            .chain()
            .find_map(|cause| cause.downcast_ref::<kv::CreateError>())
            .is_some_and(|error| error.kind() == kv::CreateErrorKind::AlreadyExists)
}
