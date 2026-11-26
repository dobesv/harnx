use super::store::CheckpointStore;
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::response::Response;
use harnx_nats_common::leader_reads;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

impl CheckpointStore {
    pub async fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let filter = format!("{}{pattern}", self.kv.prefix);
        let mut keys = BTreeSet::new();
        let mut offset = 0;
        loop {
            let request = serde_json::json!({"subjects_filter": filter, "offset": offset});
            let page: SubjectPage = match self
                .js
                .request(format!("STREAM.INFO.{}", self.kv.stream_name), &request)
                .await?
            {
                Response::Ok(page) => page,
                Response::Err { error } => return Err(error.into()),
            };
            ensure!(
                page.cluster
                    .as_ref()
                    .is_none_or(|c| c.leader.as_deref().is_some_and(|s| !s.is_empty())),
                "interaction checkpoint stream has no leader"
            );
            let subjects = page.state.subjects.unwrap_or_default();
            offset += subjects.len();
            let done = subjects.is_empty() || offset >= page.total;
            keys.extend(subjects.into_keys().filter_map(|subject| {
                subject
                    .strip_prefix(self.kv.prefix.as_str())
                    .map(str::to_owned)
            }));
            if done {
                return Ok(keys.into_iter().collect());
            }
        }
    }

    /// Per-key cleanup for completed rounds; session deletion uses `purge_session` to remove subjects physically.
    pub async fn purge_prefix(&self, prefix: &str) -> Result<()> {
        for key in self.keys(&format!("{prefix}>")).await? {
            let entry =
                leader_reads::retry_transient(|| leader_reads::entry(&self.kv, &key)).await?;
            if entry.is_some_and(|e| e.operation == async_nats::jetstream::kv::Operation::Put) {
                self.kv.purge(&key).await?;
            }
        }
        Ok(())
    }

    /// Physically remove every message under the session's prefix, leaving
    /// no per-key purge markers. Session deletion only; one stream purge request
    /// means deletion can retry the operation if it fails.
    pub async fn purge_session(&self, session_storage_key: &str) -> Result<()> {
        let subject = format!(
            "{}{}>",
            self.kv.prefix,
            super::records::session_prefix(session_storage_key)
        );
        self.kv
            .stream
            .purge()
            .filter(subject)
            .await
            .map(|_| ())
            .context("purge interaction checkpoint session prefix")
    }

    /// How many live checkpoint records `session_storage_key` holds, for
    /// read-only storage stats. The key listing keeps purge markers left by
    /// per-key cleanup, so only entries whose latest revision is a Put count.
    pub async fn live_session_record_count(&self, session_storage_key: &str) -> Result<usize> {
        let pattern = format!("{}>", super::records::session_prefix(session_storage_key));
        let mut count = 0;
        for key in self.keys(&pattern).await? {
            let live = leader_reads::retry_transient(|| leader_reads::entry(&self.kv, &key))
                .await?
                .is_some_and(|entry| entry.operation == async_nats::jetstream::kv::Operation::Put);
            if live {
                count += 1;
            }
        }
        Ok(count)
    }
}

#[derive(Deserialize)]
struct SubjectPage {
    #[serde(default)]
    total: usize,
    cluster: Option<Cluster>,
    #[serde(default)]
    state: State,
}
#[derive(Deserialize)]
struct Cluster {
    leader: Option<String>,
}
#[derive(Default, Deserialize)]
struct State {
    subjects: Option<HashMap<String, u64>>,
}
