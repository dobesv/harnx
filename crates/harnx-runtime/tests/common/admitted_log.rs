//! Worker fixtures append new prompts through the same reservation contract as
//! NatsSession. Raw transcript writes are not recovery admission evidence.
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use harnx_execution_control::ExecutionStore;
use harnx_runtime::nats_session_log::NatsSessionLog;

#[derive(Clone)]
pub struct AdmittedSessionLog {
    inner: NatsSessionLog,
    js: async_nats::jetstream::Context,
    session: String,
}

impl std::ops::Deref for AdmittedSessionLog {
    type Target = NatsSessionLog;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AdmittedSessionLog {
    pub fn new(js: async_nats::jetstream::Context, session: impl Into<String>) -> Self {
        let session = session.into();
        Self {
            inner: NatsSessionLog::new(js.clone(), &session),
            js,
            session,
        }
    }

    pub async fn append_event_async(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut entry = entry.clone();
        let SessionLogEntry::Message { id, role, .. } = &mut entry else {
            return self.inner.append_event_async(&entry).await;
        };
        if !role.is_user() {
            return self.inner.append_event_async(&entry).await;
        }
        let id = id
            .get_or_insert_with(|| uuid::Uuid::now_v7().to_string())
            .clone();
        let store = ExecutionStore::ensure(&self.js, 1).await?;
        let operation = store.session(&self.session, None, None).await?;
        store.reserve_prompt(&operation.reference, &id).await?;
        let seq = self
            .inner
            .append_event_async(&entry)
            .await
            .context("append admitted fixture prompt")?;
        store.commit_prompt(&operation.reference, &id, seq).await?;
        Ok(seq)
    }
}
