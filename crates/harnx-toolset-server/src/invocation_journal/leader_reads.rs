//! Journal reads, each answered by the stream leader.
//!
//! The bucket is replicated, and NATS lets a follower answer a KV direct get
//! from whatever it has applied so far, so `kv::Store::get` can miss a row
//! written a moment earlier, such as the one a worker records just before it
//! dispatches the call. `kv::Store::keys` lists from a consumer, which NATS
//! can place on any replica as well. These reads go to the stream leader
//! instead, which holds every acknowledged write: rows through
//! `STREAM.MSG.GET`, key listings through `STREAM.INFO`. Neither depends on
//! the bucket's `allow_direct`, so they hold for buckets created with it on.
//!
//! While the stream has no leader, `STREAM.MSG.GET` is refused, but every
//! replica answers `STREAM.INFO` from its own store and names no leader in
//! the answer. A listing page that names none is refused here.
use super::*;
use async_nats::jetstream::response::Response;
use harnx_nats_common::leader_reads;
use std::collections::{BTreeSet, HashMap};

impl InvocationJournal {
    /// The latest revision of `key`, which may be a delete or purge marker.
    pub(super) async fn entry(&self, key: &str) -> Result<Option<kv::Entry>> {
        // Refuses a key with a wildcard in its call or session id, which the
        // leader would take as a filter and answer with some other call's row.
        leader_reads::entry(&self.0, key)
            .await
            .with_context(|| format!("read tool invocation journal key {key}"))
    }

    /// `key`'s latest revision, or `None` once it has been deleted or purged.
    pub(super) async fn live_entry(&self, key: &str) -> Result<Option<kv::Entry>> {
        Ok(self
            .entry(key)
            .await?
            .filter(|entry| entry.operation == kv::Operation::Put))
    }

    /// The latest revision of the one key `pattern` matches, or `None` once
    /// it has been deleted or purged. Each `*` token in the pattern matches
    /// any one token of a key.
    pub(super) async fn live_entry_matching(&self, pattern: &str) -> Result<Option<kv::Entry>> {
        let entry = leader_reads::entry_matching(&self.0, pattern)
            .await
            .with_context(|| format!("read tool invocation journal key {pattern}"))?;
        Ok(entry.filter(|entry| entry.operation == kv::Operation::Put))
    }

    /// Every key `pattern` matches, including keys already purged. A `*`
    /// token matches any one token of a key, and a final `>` any number.
    pub(super) async fn keys(&self, pattern: &str) -> Result<Vec<String>> {
        let prefix = self.0.prefix.as_str();
        let filter = format!("{prefix}{pattern}");
        // Pages are sorted by subject, so a key created between two page
        // requests shifts the next page and lists one key twice.
        let mut keys = BTreeSet::new();
        let mut offset = 0;
        loop {
            let page = self
                .subject_page(&filter, offset)
                .await
                .context("list tool invocation journal keys")?;
            ensure!(
                page.answered_by_leader(),
                "the tool invocation journal's stream has no leader"
            );
            let subjects = page.state.subjects.unwrap_or_default();
            offset += subjects.len();
            let listed_all = subjects.is_empty() || offset >= page.total;
            keys.extend(
                subjects
                    .into_keys()
                    .filter_map(|subject| subject.strip_prefix(prefix).map(str::to_owned)),
            );
            if listed_all {
                return Ok(keys.into_iter().collect());
            }
        }
    }

    async fn subject_page(&self, filter: &str, offset: usize) -> Result<SubjectPage> {
        let Self(store, js) = self;
        let request = serde_json::json!({
            "subjects_filter": filter,
            "offset": offset,
        });
        match js
            .request(format!("STREAM.INFO.{}", store.stream_name), &request)
            .await?
        {
            Response::Ok(page) => Ok(page),
            Response::Err { error } => Err(error.into()),
        }
    }
}

/// One page of the stream's subjects, as `STREAM.INFO` lists them.
#[derive(Deserialize)]
struct SubjectPage {
    /// How many subjects match the filter across every page.
    #[serde(default)]
    total: usize,
    cluster: Option<PageCluster>,
    #[serde(default)]
    state: PageState,
}

#[derive(Deserialize)]
struct PageCluster {
    #[serde(default)]
    leader: Option<String>,
}

#[derive(Default, Deserialize)]
struct PageState {
    #[serde(default)]
    subjects: Option<HashMap<String, u64>>,
}

impl SubjectPage {
    /// A server running the stream on its own names itself as the leader, and
    /// one that reports no cluster at all has no other replica to lag it.
    fn answered_by_leader(&self) -> bool {
        self.cluster.as_ref().is_none_or(|cluster| {
            cluster
                .leader
                .as_deref()
                .is_some_and(|leader| !leader.is_empty())
        })
    }
}
