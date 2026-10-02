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
use async_nats::jetstream::{
    message::StreamMessage, response::Response, stream::LastRawMessageErrorKind,
};
use std::collections::{BTreeSet, HashMap};

/// The header a KV store writes on the markers that delete and purge a key.
const KV_OPERATION: &str = "KV-Operation";

impl InvocationJournal {
    /// The latest revision of `key`, which may be a delete or purge marker.
    pub(super) async fn entry(&self, key: &str) -> Result<Option<kv::Entry>> {
        // The leader takes this subject as a filter, so a wildcard in a call
        // or session id would answer with some other key's row.
        ensure!(
            is_valid_key(key),
            "invalid tool invocation journal key {key}"
        );
        let store = &self.0;
        let subject = format!("{}{key}", store.prefix);
        let message = match store.stream.get_last_raw_message_by_subject(&subject).await {
            Ok(message) => message,
            Err(error) if matches!(error.kind(), LastRawMessageErrorKind::NoMessageFound) => {
                return Ok(None);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read tool invocation journal key {key}"))
            }
        };
        Ok(Some(kv::Entry {
            bucket: store.name.clone(),
            key: key.to_owned(),
            operation: operation(&message),
            value: message.payload,
            revision: message.sequence,
            delta: 0,
            created: message.time,
            seen_current: false,
        }))
    }

    /// `key`'s latest revision, or `None` once it has been deleted or purged.
    pub(super) async fn live_entry(&self, key: &str) -> Result<Option<kv::Entry>> {
        Ok(self
            .entry(key)
            .await?
            .filter(|entry| entry.operation == kv::Operation::Put))
    }

    /// Every key `session` has a row under, including rows already purged.
    pub(super) async fn session_keys(&self, session: &str) -> Result<Vec<String>> {
        let prefix = self.0.prefix.as_str();
        let session_prefix = format!("sessions/{session}/");
        // Pages are sorted by subject, so a key created between two page
        // requests shifts the next page and lists one key twice.
        let mut keys = BTreeSet::new();
        let mut offset = 0;
        loop {
            let page = self
                .subject_page(offset)
                .await
                .context("list tool invocation journal keys")?;
            ensure!(
                page.answered_by_leader(),
                "the tool invocation journal's stream has no leader"
            );
            let subjects = page.state.subjects.unwrap_or_default();
            offset += subjects.len();
            let listed_all = subjects.is_empty() || offset >= page.total;
            keys.extend(subjects.into_keys().filter_map(|subject| {
                let key = subject.strip_prefix(prefix)?;
                key.starts_with(&session_prefix).then(|| key.to_owned())
            }));
            if listed_all {
                return Ok(keys.into_iter().collect());
            }
        }
    }

    async fn subject_page(&self, offset: usize) -> Result<SubjectPage> {
        let Self(store, js) = self;
        let request = serde_json::json!({
            "subjects_filter": format!("{}>", store.prefix),
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

/// What a revision did to its key, decoded as `kv::Store` decodes it. The
/// server marks a key it removed itself, for an age limit say, with a marker
/// reason rather than the KV operation header.
fn operation(message: &StreamMessage) -> kv::Operation {
    if let Some(operation) = message.headers.get(KV_OPERATION) {
        return operation.as_str().parse().unwrap_or(kv::Operation::Put);
    }
    let reason = message.headers.get(async_nats::header::NATS_MARKER_REASON);
    match reason.map(|reason| reason.as_str()) {
        Some("MaxAge" | "Purge") => kv::Operation::Purge,
        Some("Remove") => kv::Operation::Delete,
        _ => kv::Operation::Put,
    }
}

/// The keys `kv::Store` itself accepts.
fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('.')
        && !key.ends_with('.')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-/_=.".contains(&byte))
}
