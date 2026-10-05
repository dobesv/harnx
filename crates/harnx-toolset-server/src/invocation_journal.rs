//! Durable requests and replies outlive a tool-server process and its reply cache.
//! Records are retained until the owning session is deleted, because a parent
//! may not have persisted a tool's response by the time the call finishes.
//!
//! A call's row is keyed `sessions.<session>.<round>.<tool call>.<call>`: the
//! transcript round and tool-call id that recovery knows the call by, then the
//! wire id each dispatch attempt mints. Each part is escaped into one subject
//! token, so a lookup lists a single round, or one call in it, rather than
//! every session's rows. A call with no parent session is keyed
//! `standalone/<call>`. Rows written before this layout, keyed
//! `sessions/<session>/<call>`, are no longer read, but deleting their
//! session still deletes them.
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, kv};
use harnx_toolset::{ToolReply, ToolRequest};
use serde::{Deserialize, Serialize};

mod leader_reads;
mod replies;
pub use replies::{JournalCheckpointStore, JournalPartialResultStore};

pub const BUCKET: &str = "harnx_tool_invocations";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedInvocation {
    pub request: ToolRequest,
    pub tool_name: String,
    pub server: String,
    pub server_scope: String,
    pub tool_round: u64,
    pub started_at_ms: u64,
    /// The call's outcome. The first writer wins, so a reply recorded here is
    /// the one every later duplicate, replay and cancel observes.
    pub reply: Option<ToolReply>,
    /// Opaque handle the tool recorded so an orphaned call can still be
    /// cancelled after the process that started it is gone.
    #[serde(default)]
    pub checkpoint: Option<serde_json::Value>,
    /// What the tool reported it had produced so far. Every non-success output
    /// written for the call carries it; once `reply` is recorded it is frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_result: Option<serde_json::Value>,
}

impl RecordedInvocation {
    /// Whether this row is one `round` made for the transcript call `call_id`.
    /// Only that pair survives a worker restart, and a call retried inside
    /// one round has a row per attempt answering to it.
    pub fn answers(&self, round: u64, call_id: &str) -> bool {
        self.tool_round == round && self.request.tool_call_id.as_deref() == Some(call_id)
    }
}

/// The journal's KV store, and the JetStream context its key listings are
/// requested through.
#[derive(Clone)]
pub struct InvocationJournal(kv::Store, jetstream::Context);

/// Reconcile durability away from the request path, so a replica count raised
/// after this server started still reaches the journal bucket. Retain the
/// future across incoming requests so busy traffic cannot starve reconciliation.
pub(crate) fn replica_reconciliations(
    js: jetstream::Context,
    replicas: usize,
    interval: std::time::Duration,
) -> impl futures_util::Stream<Item = Result<()>> {
    futures_util::stream::unfold(js, move |js| async move {
        tokio::time::sleep(interval).await;
        let result =
            harnx_nats_common::registry::reconcile_bucket_replicas(&js, BUCKET, replicas).await;
        Some((result, js))
    })
}

impl InvocationJournal {
    /// Open the journal bucket, creating it at `replicas` durability. This
    /// bucket carries the only recoverable copy of results that have not been
    /// appended to a session log yet, so it follows the connection's configured
    /// replica count rather than a bucket-local default.
    pub async fn ensure(js: &jetstream::Context, replicas: usize) -> Result<Self> {
        let store = match js
            .create_key_value(kv::Config {
                bucket: BUCKET.into(),
                num_replicas: replicas,
                ..Default::default()
            })
            .await
        {
            Ok(store) => store,
            Err(_) => {
                harnx_nats_common::registry::reconcile_bucket_replicas(js, BUCKET, replicas)
                    .await?;
                js.get_key_value(BUCKET).await?
            }
        };
        Ok(Self(store, js.clone()))
    }

    pub fn from_store(js: &jetstream::Context, store: kv::Store) -> Self {
        Self(store, js.clone())
    }

    /// Journal `request` before it is dispatched, as round
    /// `request.tool_round` of its session made it, or round zero for a call
    /// no round made.
    pub async fn record(&self, request: &ToolRequest, tool: (&str, &str, &str)) -> Result<()> {
        self.check_session_retained(request).await?;
        let record = RecordedInvocation {
            request: request.clone(),
            tool_name: tool.0.into(),
            server_scope: tool.1.into(),
            server: tool.2.into(),
            tool_round: request.tool_round.unwrap_or(0),
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            reply: None,
            checkpoint: None,
            partial_result: None,
        };
        self.0
            .create(key(request), serde_json::to_vec(&record)?.into())
            .await?;
        // A caller may have loaded ToolCalls before deletion removed the
        // transcript. It must not dispatch a late request after journal cleanup.
        if let Err(error) = self.check_session_retained(request).await {
            self.0.purge(key(request)).await?;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn check_session_retained(&self, request: &ToolRequest) -> Result<()> {
        if let Some(session) = &request.parent_session_id {
            ensure!(
                self.live_entry(&format!("deleted/{session}"))
                    .await?
                    .is_none(),
                "tool invocation session was deleted"
            );
        }
        Ok(())
    }

    pub async fn validate_replay(&self, request: &ToolRequest) -> Result<()> {
        let record = self
            .get(request)
            .await?
            .context("replay has no durable invocation")?;
        let mut original = request.clone();
        original.replay = None;
        ensure!(
            original == record.request,
            "replay does not match the original invocation"
        );
        Ok(())
    }

    pub async fn get(&self, request: &ToolRequest) -> Result<Option<RecordedInvocation>> {
        self.read(&key(request)).await
    }

    /// The row of the call a cancel names by session and wire id alone. Each
    /// dispatch attempt mints its own wire id, so the round and tool-call
    /// parts of the key are left for the leader to match.
    pub async fn recorded(
        &self,
        session: &str,
        call_id: &str,
    ) -> Result<Option<RecordedInvocation>> {
        let pattern = format!("{}.*.*.{}", session_prefix(session), token(call_id));
        decode(self.live_entry_matching(&pattern).await?)
    }

    async fn read(&self, key: &str) -> Result<Option<RecordedInvocation>> {
        decode(self.live_entry(key).await?)
    }

    /// The row `round` made for the transcript call `call_id`. A call retried
    /// inside its round has a row per attempt, and replay cannot tell which
    /// of them to resume, so more than one is an error.
    pub async fn find(
        &self,
        session: &str,
        round: u64,
        call_id: &str,
    ) -> Result<Option<RecordedInvocation>> {
        let attempts = format!("{}.{round}.{}.*", session_prefix(session), token(call_id));
        let mut found = None;
        for key in self.keys(&attempts).await? {
            let record = self
                .read(&key)
                .await?
                .filter(|record| record.answers(round, call_id));
            if let Some(record) = record {
                ensure!(found.is_none(), "ambiguous durable tool invocation");
                found = Some(record);
            }
        }
        Ok(found)
    }

    /// Every row this session holds.
    ///
    /// A row that will not deserialize is skipped with a warning: it cannot
    /// hold a usable reply, and failing the listing over one unreadable row
    /// would wedge every later reader of the session. A read that fails in
    /// transport still propagates, because that row may well hold a reply the
    /// caller simply could not see.
    pub async fn records_for_session(&self, session: &str) -> Result<Vec<RecordedInvocation>> {
        self.records_under(&format!("{}.>", session_prefix(session)))
            .await
    }

    /// `records_for_session`, narrowed to the rows made in one of `rounds`.
    /// Wind-up only ever needs the interrupted rounds' rows, not a whole
    /// session's worth of request args and replies.
    pub async fn records_in_rounds(
        &self,
        session: &str,
        rounds: &[u64],
    ) -> Result<Vec<RecordedInvocation>> {
        let mut found = Vec::new();
        for round in rounds.iter().collect::<std::collections::BTreeSet<_>>() {
            let pattern = format!("{}.{round}.>", session_prefix(session));
            found.extend(self.records_under(&pattern).await?);
        }
        Ok(found)
    }

    /// Shared listing behind `records_for_session` and `records_in_rounds`:
    /// the rows under every key `pattern` matches, with an unreadable row
    /// skipped and a failed read propagated, as `records_for_session`
    /// describes.
    async fn records_under(&self, pattern: &str) -> Result<Vec<RecordedInvocation>> {
        let mut found = Vec::new();
        for key in self.keys(pattern).await? {
            let Some(entry) = self.live_entry(&key).await? else {
                continue;
            };
            match serde_json::from_slice::<RecordedInvocation>(&entry.value) {
                Ok(record) => found.push(record),
                Err(error) => {
                    log::warn!("skipping unreadable tool invocation: key={key} error={error}");
                }
            }
        }
        Ok(found)
    }

    async fn first_value<T: Clone>(
        &self,
        key: String,
        value: T,
        field: impl Fn(&mut RecordedInvocation) -> &mut Option<T>,
    ) -> Result<T> {
        loop {
            if let Some(saved) = self.try_first_value(&key, &value, &field).await? {
                return Ok(saved);
            }
        }
    }

    async fn try_first_value<T: Clone>(
        &self,
        key: &str,
        value: &T,
        field: &impl Fn(&mut RecordedInvocation) -> &mut Option<T>,
    ) -> Result<Option<T>> {
        let entry = self
            .entry(key)
            .await?
            .context("durable tool invocation missing")?;
        ensure!(
            entry.operation == kv::Operation::Put,
            "tool invocation was deleted"
        );
        let mut record: RecordedInvocation = serde_json::from_slice(&entry.value)?;
        let slot = field(&mut record);
        if let Some(existing) = slot {
            return Ok(Some(existing.clone()));
        }
        *slot = Some(value.clone());
        match self
            .0
            .update(key, serde_json::to_vec(&record)?.into(), entry.revision)
            .await
        {
            Ok(_) => Ok(Some(value.clone())),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(None),
            Err(error) => Err(error).context("persist tool invocation state"),
        }
    }

    pub async fn purge_session(&self, session: &str) -> Result<()> {
        // Retain a small tombstone after deleting the runnable session. Delayed
        // writers must not resurrect its journal or start another invocation.
        self.0
            .put(format!("deleted/{session}"), "deleted".into())
            .await?;
        for key in self.keys(&format!("{}.>", session_prefix(session))).await? {
            self.0.purge(key).await?;
        }
        // Rows from before the session was a token of its own are keyed
        // `sessions/<session>/<call>`. A worker's wire ids are UUIDs, so each
        // of its rows' keys is one token, and listing single-token keys
        // leaves out every row in the current layout.
        let legacy = format!("sessions/{session}/");
        for key in self.keys("*").await? {
            if key.starts_with(&legacy) {
                self.0.purge(key).await?;
            }
        }
        Ok(())
    }
}

fn decode(entry: Option<kv::Entry>) -> Result<Option<RecordedInvocation>> {
    entry
        .map(|entry| serde_json::from_slice(&entry.value).map_err(Into::into))
        .transpose()
}

/// The key every reader and writer of `request`'s row rebuilds from the
/// request alone.
fn key(request: &ToolRequest) -> String {
    match &request.parent_session_id {
        Some(session) => format!(
            "{}.{}.{}.{}",
            session_prefix(session),
            request.tool_round.unwrap_or(0),
            token(request.tool_call_id.as_deref().unwrap_or_default()),
            token(&request.call_id),
        ),
        None => format!("standalone/{}", request.call_id),
    }
}

/// The tokens every key of `session`'s rows starts with.
fn session_prefix(session: &str) -> String {
    format!("sessions.{}", token(session))
}

/// `id` as one subject token that no other id shares. Bytes outside
/// `[A-Za-z0-9_-]` are written as `=` and two hex digits, so a `.` cannot
/// split an id across tokens, `*` or `>` cannot make it a wildcard, and no
/// escape reads as the character it stands for. The empty id, which no escape
/// produces, is `=` alone. Stored keys are built with this, so changing it
/// strands every row already written.
fn token(id: &str) -> String {
    if id.is_empty() {
        return "=".into();
    }
    let mut token = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || b"_-".contains(&byte) {
            token.push(char::from(byte));
        } else {
            token.push_str(&format!("={byte:02X}"));
        }
    }
    token
}
