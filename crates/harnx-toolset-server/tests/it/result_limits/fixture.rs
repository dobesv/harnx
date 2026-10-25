use crate::common;
use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use async_trait::async_trait;
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{
    InterruptedCall, ToolInvocation, ToolInvokeError, ToolReply, ToolRequest, ToolSpec, Toolset,
};
use harnx_toolset_server::{
    invocation_journal::{InvocationJournal, RecordedInvocation, BUCKET},
    serve_with_shutdown, ServeLifecycle,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub(super) struct OutputToolset {
    pub calls: Arc<AtomicUsize>,
    pub entered: Arc<Notify>,
    pub release: Arc<Notify>,
    pub preset: Arc<std::sync::Mutex<Option<Value>>>,
}

pub(super) fn output(args: &Value) -> Value {
    let count = args["count"].as_u64().unwrap_or(1) as usize;
    let unit = args["unit"].as_str().unwrap_or("x");
    if let Some(value) = args.get("value") {
        return value.clone();
    }
    if args["shape"] == "mcp" {
        return json!({"content": [{"type": "text", "text": unit.repeat(count)}, {"type": "text", "text": "small sibling"}], "structuredContent": {"schema": "unchanged"}, "_meta": {"extra": "kept"}, "isError": true});
    }
    json!({"output": unit.repeat(count)})
}

#[async_trait]
impl Toolset for OutputToolset {
    fn name(&self) -> &str {
        "test"
    }
    fn tools(&self) -> Vec<ToolSpec> {
        vec![common::tool_spec("echo")]
    }
    async fn invoke(
        &self,
        _: &str,
        args: Value,
        _: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if args["park"] == true {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let reason = match args["reason_count"].as_u64() {
            Some(count) => "x".repeat(count as usize),
            None => args["reason"]
                .as_str()
                .unwrap_or("original failure")
                .to_string(),
        };
        match args["error"].as_str() {
            Some("fatal") => Err(ToolInvokeError::Fatal(reason)),
            Some("interrupted") => Err(ToolInvokeError::Interrupted(InterruptedCall {
                cancellation_id: Some("cancel-original".into()),
                reason,
            })),
            Some("recoverable") => Err(ToolInvokeError::Recoverable(reason)),
            _ => Ok(self
                .preset
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| output(&args))),
        }
    }
    async fn invoke_with_context(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Value, ToolInvokeError> {
        let context = &invocation.context;
        if let Some(value) = invocation.args.get("checkpoint") {
            context
                .checkpoint_store
                .as_ref()
                .unwrap()
                .checkpoint(value.clone())
                .await
                .map_err(|e| ToolInvokeError::Fatal(format!("{e:#}")))?;
        }
        if let Some(value) = invocation.args.get("partial") {
            context
                .record_partial_result(value.clone())
                .await
                .map_err(|e| ToolInvokeError::Fatal(format!("{e:#}")))?;
        }
        if let Some(count) = invocation.args["progress_bytes"].as_u64() {
            context.progress.update(harnx_toolset::ToolProgressPatch {
                markdown: Some("p".repeat(count as usize)),
                ..Default::default()
            });
        }
        self.invoke(&invocation.tool, invocation.args, invocation.cancel)
            .await
    }
}

pub(super) struct Fixture {
    pub broker: common::NatsServerHandle,
    pub client: async_nats::Client,
    pub js: jetstream::Context,
    pub store: kv::Store,
    pub journal: InvocationJournal,
    pub scope: ServerScope,
    pub toolset: OutputToolset,
    shutdown: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Fixture {
    pub async fn start(wire_limit: Option<usize>, kv_limit: Option<i32>) -> Result<Self> {
        let config = wire_limit.map(|limit| format!("max_payload: {limit}\n"));
        Self::configured(config.as_deref(), kv_limit).await
    }

    pub async fn configured(config: Option<&str>, kv_limit: Option<i32>) -> Result<Self> {
        let broker = common::spawn_configured_nats_server(config)
            .await?
            .context("nats-server required")?;
        let client = connect(&broker.url).await?;
        let js = jetstream::new(client.clone());
        let store = js
            .create_key_value(kv::Config {
                bucket: BUCKET.into(),
                max_value_size: kv_limit.unwrap_or(-1),
                ..Default::default()
            })
            .await?;
        let journal = InvocationJournal::from_store(&js, store.clone());
        let mut fixture = Self {
            broker,
            client,
            js,
            store,
            journal,
            scope: ServerScope::new(),
            toolset: OutputToolset::default(),
            shutdown: CancellationToken::new(),
            task: None,
        };
        fixture.restart().await?;
        Ok(fixture)
    }

    pub async fn restart(&mut self) -> Result<()> {
        let client = connect(&self.broker.url).await?;
        self.serve_client(client).await
    }

    pub async fn serve_client(&mut self, client: async_nats::Client) -> Result<()> {
        self.stop().await?;
        let previous = self.registration_revision().await?;
        self.shutdown = CancellationToken::new();
        let toolset = self.toolset.clone();
        let scope = self.scope.clone();
        let shutdown = self.shutdown.clone();
        let ready = harnx_healthz::Readiness::default();
        let server_ready = ready.clone();
        self.task = Some(tokio::spawn(async move {
            serve_with_shutdown(
                Arc::new(toolset),
                scope,
                NatsConnection {
                    client,
                    replicas: 1,
                },
                ServeLifecycle::new(shutdown, Some(server_ready)),
            )
            .await
        }));
        // Readiness is set before opening the journal. Wait for this server's
        // registration revision too, so tests changing limits start with a known
        // cached config. A previous registration survives shutdown until its TTL.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                anyhow::ensure!(
                    !self.task.as_ref().unwrap().is_finished(),
                    "tool server exited before registration"
                );
                if ready.is_ready() && self.registration_revision().await? > previous {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("tool server registration timed out")??;
        Ok(())
    }

    async fn registration_revision(&self) -> Result<u64> {
        let Ok(registry) = self
            .js
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await
        else {
            return Ok(0);
        };
        let key = harnx_toolset_server::registration_key(&self.scope, "____test");
        Ok(registry
            .entry(&key)
            .await?
            .map(|entry| entry.revision)
            .unwrap_or(0))
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(60), task).await???;
        }
        Ok(())
    }

    pub async fn call(&self, request: &ToolRequest) -> Result<async_nats::Message> {
        let request = async_nats::Request::new()
            .headers(common::request_headers(&request.call_id, &request.call_id))
            .payload(serde_json::to_vec(request)?.into())
            .timeout(Some(Duration::from_secs(60)));
        harnx_nats_common::rpc::request(
            &self.client,
            self.scope.tool_subject("____test", "echo"),
            request,
        )
        .await
    }

    pub async fn check_stored(
        &self,
        request: &ToolRequest,
        message: &async_nats::Message,
        kv_limit: usize,
    ) -> Result<RecordedInvocation> {
        assert!(
            message.headers.is_none(),
            "reply must add no unbudgeted headers"
        );
        assert!(message.payload.len() <= self.client.server_info().max_payload);
        let entry = self
            .store
            .entry(key(request))
            .await?
            .context("stored invocation")?;
        let raw = self.store.stream.get_raw_message(entry.revision).await?;
        let header_bytes = 12
            + raw
                .headers
                .iter()
                .flat_map(|(name, values)| {
                    values
                        .iter()
                        .map(move |value| name.to_string().len() + 2 + value.as_str().len() + 2)
                })
                .sum::<usize>();
        assert!(raw
            .headers
            .get("Nats-Expected-Last-Subject-Sequence")
            .is_some());
        assert!(
            raw.payload.len() + header_bytes <= kv_limit,
            "row={} headers={} cap={kv_limit}",
            raw.payload.len(),
            header_bytes
        );
        assert!(raw.payload.len() + header_bytes <= self.client.server_info().max_payload);
        let record: RecordedInvocation = serde_json::from_slice(&raw.payload)?;
        assert_eq!(serde_json::to_vec(&record)?, raw.payload);
        assert_eq!(
            serde_json::to_vec(record.reply.as_ref().context("terminal reply")?)?,
            message.payload
        );
        eprintln!(
            "{}: stored={} headers={} published={}",
            request.call_id,
            raw.payload.len(),
            header_bytes,
            message.payload.len()
        );
        Ok(record)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn connect(url: &str) -> Result<async_nats::Client> {
    Ok(async_nats::ConnectOptions::new()
        .token(common::TOKEN.into())
        .request_timeout(Some(Duration::from_secs(60)))
        .connect(url)
        .await?)
}

pub(super) fn request(call_id: &str, count: usize, owned: bool) -> ToolRequest {
    let mut request = common::request("output-session", call_id);
    request.args = json!({"count": count});
    if owned {
        request.parent_agent = Some("test/owner".into());
        request.parent_local_session_id = Some("out123".into());
    }
    request
}

pub(super) fn key(request: &ToolRequest) -> String {
    format!("sessions.output-session.0.model-call.{}", request.call_id)
}

pub(super) fn reply(message: &async_nats::Message) -> ToolReply {
    serde_json::from_slice(&message.payload).unwrap()
}

pub(super) async fn retained(fixture: &Fixture, reply: &ToolReply) -> Result<ToolReply> {
    let message = match reply.result.as_ref().unwrap_err() {
        harnx_toolset::ToolErrorPayload::Recoverable(message)
        | harnx_toolset::ToolErrorPayload::Fatal(message) => message,
        harnx_toolset::ToolErrorPayload::Interrupted(interrupted) => &interrupted.reason,
    };
    let cid = message
        .split_whitespace()
        .find(|word| word.starts_with("cid:media:"))
        .context("attachment reference")?;
    let url = harnx_core::cid_url::CidUrl::parse(cid.trim_end_matches('.'))?;
    let store = fixture
        .js
        .get_object_store(harnx_blob_store::ATTACHMENTS_BUCKET)
        .await?;
    let (bytes, mime) = harnx_blob_store::get_media(&store, &url)
        .await?
        .context("stored original")?;
    assert_eq!(mime, "application/json");
    let original: ToolReply = serde_json::from_slice(&bytes)?;
    assert_eq!(serde_json::to_vec(&original)?, bytes);
    Ok(original)
}
