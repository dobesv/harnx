//! Durable requests and replies outlive a tool-server process and its reply cache.
//! Records are retained until the owning session is deleted: execution graph
//! nodes may be pruned before the parent has persisted a tool's response.
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, kv};
use futures_util::TryStreamExt;
use harnx_toolset::{ToolReply, ToolRequest};
use serde::{Deserialize, Serialize};

pub const BUCKET: &str = "harnx_tool_invocations";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedInvocation {
    pub request: ToolRequest,
    pub tool_name: String,
    pub server: String,
    pub server_scope: String,
    pub tool_round: u64,
    pub started_at_ms: u64,
    pub reply: Option<ToolReply>,
    #[serde(default)]
    pub checkpoint: Option<serde_json::Value>,
}

#[derive(Clone)]
pub struct InvocationJournal(kv::Store);

impl InvocationJournal {
    pub async fn ensure(js: &jetstream::Context) -> Result<Self> {
        // Use the execution graph's durability policy; this bucket carries the
        // only recoverable copy of results not yet appended to the transcript.
        let execution = js
            .get_stream(format!("KV_{}", harnx_execution_control::BUCKET))
            .await?;
        let replicas = execution.cached_info().config.num_replicas;
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
        Ok(Self(store))
    }

    pub fn from_store(store: kv::Store) -> Self {
        Self(store)
    }

    pub async fn record(
        &self,
        request: &ToolRequest,
        tool: (&str, &str, &str),
        tool_round: u64,
    ) -> Result<()> {
        self.check_session_retained(request).await?;
        let record = RecordedInvocation {
            request: request.clone(),
            tool_name: tool.0.into(),
            server_scope: tool.1.into(),
            server: tool.2.into(),
            tool_round,
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            reply: None,
            checkpoint: None,
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
                self.0.get(format!("deleted/{session}")).await?.is_none(),
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

    /// Publish a durable job handle before starting work. Concurrent replay
    /// observers all receive the first handle; no second job may be started.
    pub async fn checkpoint(
        &self,
        session: &str,
        call_id: &str,
        value: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.first_value(format!("sessions/{session}/{call_id}"), value, |record| {
            &mut record.checkpoint
        })
        .await
    }

    pub async fn recorded(
        &self,
        session: &str,
        call_id: &str,
    ) -> Result<Option<RecordedInvocation>> {
        self.read(&format!("sessions/{session}/{call_id}")).await
    }

    async fn read(&self, key: &str) -> Result<Option<RecordedInvocation>> {
        self.0
            .get(key)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    pub async fn find(
        &self,
        session: &str,
        round: u64,
        call_id: &str,
    ) -> Result<Option<RecordedInvocation>> {
        let mut found = None;
        for key in self.session_keys(session).await? {
            let record = self.read(&key).await?.filter(|record| {
                record.tool_round == round
                    && record.request.tool_call_id.as_deref() == Some(call_id)
            });
            if let Some(record) = record {
                ensure!(found.is_none(), "ambiguous durable tool invocation");
                found = Some(record);
            }
        }
        Ok(found)
    }

    /// The first persisted result is authoritative for every replay observer.
    pub async fn complete(&self, request: &ToolRequest, reply: ToolReply) -> Result<ToolReply> {
        self.first_value(key(request), reply, |record| &mut record.reply)
            .await
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
            .0
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

    async fn session_keys(&self, session: &str) -> Result<Vec<String>> {
        let prefix = format!("sessions/{session}/");
        let mut keys = self.0.keys().await?;
        let mut selected = Vec::new();
        while let Some(key) = keys.try_next().await? {
            if key.starts_with(&prefix) {
                selected.push(key);
            }
        }
        Ok(selected)
    }

    pub async fn purge_session(&self, session: &str) -> Result<()> {
        // Retain a small tombstone after deleting the runnable session. Delayed
        // writers must not resurrect its journal or start another invocation.
        self.0
            .put(format!("deleted/{session}"), "deleted".into())
            .await?;
        for key in self.session_keys(session).await? {
            self.0.purge(key).await?;
        }
        Ok(())
    }
}

fn key(request: &ToolRequest) -> String {
    match &request.parent_session_id {
        Some(session) => format!("sessions/{session}/{}", request.call_id),
        None => format!("standalone/{}", request.call_id),
    }
}
