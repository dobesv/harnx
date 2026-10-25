use crate::bridge_roundtrip::{
    spawn_nats_server_with_payload_limit, tool_request, wait_for_registration, NatsServerHandle,
    TOKEN,
};
use anyhow::{Context, Result};
use base64::Engine;
use harnx_core::{
    attachments::cid_for_data_url_with_session,
    cid_url::{CidUrl, SessionRef},
    instance::ServerScope,
};
use harnx_toolset::{
    server_identity_token, ToolReply, ToolRequest, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
};
use harnx_toolset_server::invocation_journal::{InvocationJournal, RecordedInvocation, BUCKET};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(60);

pub(super) struct Fixture {
    broker: NatsServerHandle,
    _dir: tempfile::TempDir,
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    store: async_nats::jetstream::kv::Store,
    session: SessionRef,
    script: PathBuf,
    audit: PathBuf,
    pub request: ToolRequest,
}

impl Fixture {
    pub async fn new(
        payload_limit: Option<usize>,
        call_id: &str,
        original: &Value,
    ) -> Result<Self> {
        let broker = spawn_nats_server_with_payload_limit(payload_limit)
            .await?
            .context("nats-server required")?;
        let client = async_nats::ConnectOptions::with_token(TOKEN.into())
            .connect(&broker.url)
            .await?;
        let js = async_nats::jetstream::new(client.clone());
        let store = js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: BUCKET.into(),
                max_value_size: 8192,
                ..Default::default()
            })
            .await?;
        let session = SessionRef::new(Some("bridge/owner".into()), "srv123".into())?;
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fixture.json");
        let audit = dir.path().join("requests.jsonl");
        std::fs::write(
            &script,
            serde_json::to_vec(
                &json!({"tools": [{"name": "large"}], "call_result": original, "responses": ["unused"]}),
            )?,
        )?;
        let mut request = tool_request(call_id, "large");
        request.parent_session_id = Some("bridge-output-session".into());
        request.parent_agent = session.agent.clone();
        request.parent_local_session_id = Some(session.session_id.clone());
        Ok(Self {
            broker,
            _dir: dir,
            client,
            js,
            store,
            session,
            script,
            audit,
            request,
        })
    }

    pub async fn start(&self) -> Result<(tokio::process::Child, ServerScope)> {
        let scope = ServerScope::new();
        let child = start_bridge(&self.broker.url, &scope, &self.script, &self.audit).await?;
        let identity = server_identity_token(None, "", "shared-limit");
        wait_for_registration(&self.client, &scope, &identity).await?;
        Ok((child, scope))
    }

    pub async fn send(&self, scope: &ServerScope) -> Result<async_nats::Message> {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(HDR_CALL_ID, self.request.call_id.as_str());
        headers.insert(HDR_IDEMPOTENCY_KEY, self.request.call_id.as_str());
        let identity = server_identity_token(None, "", "shared-limit");
        Ok(self
            .client
            .send_request(
                scope.tool_subject(&identity, "large"),
                async_nats::Request::new()
                    .headers(headers)
                    .payload(serde_json::to_vec(&self.request)?.into())
                    .timeout(Some(DEADLINE)),
            )
            .await?)
    }

    pub async fn assert_durable(&self, message: &async_nats::Message) -> Result<()> {
        let journal = InvocationJournal::from_store(&self.js, self.store.clone());
        let record = journal.get(&self.request).await?.context("durable reply")?;
        assert_eq!(
            serde_json::to_vec(record.reply.as_ref().unwrap())?,
            message.payload
        );
        let key = format!(
            "sessions.bridge-output-session.0.=.{}",
            self.request.call_id
        );
        let entry = self.store.entry(key).await?.context("stored invocation")?;
        let raw = self.store.stream.get_raw_message(entry.revision).await?;
        let header_bytes = 51
            + raw
                .headers
                .get("Nats-Expected-Last-Subject-Sequence")
                .context("CAS header")?
                .as_str()
                .len();
        assert!(raw.payload.len() + header_bytes <= 8192);
        let row: RecordedInvocation = serde_json::from_slice(&raw.payload)?;
        assert_eq!(
            serde_json::to_vec(row.reply.as_ref().unwrap())?,
            message.payload
        );
        Ok(())
    }

    pub async fn assert_original_reply(&self, original: &Value) -> Result<()> {
        let reply = ToolReply {
            call_id: self.request.call_id.clone(),
            result: Ok(original.clone()),
            final_progress: None,
        };
        let bytes = serde_json::to_vec(&reply)?;
        let url = self.cid("application/json", &bytes)?;
        assert_eq!(
            self.attachment(&url).await?,
            (bytes, "application/json".into())
        );
        Ok(())
    }

    pub async fn retained_bridge_result(&self, error: &str) -> Result<Value> {
        let url = error
            .split_whitespace()
            .find(|word| word.starts_with("cid:media:"))
            .context("full reply reference")?
            .trim_end_matches('.');
        let (bytes, mime) = self.attachment(&CidUrl::parse(url)?).await?;
        assert_eq!(mime, "application/json");
        let original: ToolReply = serde_json::from_slice(&bytes)?;
        let normalized = original
            .result
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert!(serde_json::to_vec(&normalized)?.len() <= 32_768);
        Ok(normalized)
    }

    pub async fn assert_text_original(&self, normalized: &Value, text: &str) -> Result<()> {
        let url = self.cid("text/plain", text.as_bytes())?;
        assert!(normalized["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&url.to_string()));
        assert_eq!(
            self.attachment(&url).await?,
            (text.as_bytes().to_vec(), "text/plain".into())
        );
        Ok(())
    }

    pub fn assert_child_calls(&self, expected: usize) -> Result<()> {
        let calls = std::fs::read_to_string(&self.audit)?
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry["method"] == "tools/call")
            .count();
        assert_eq!(
            calls, expected,
            "a new bridge process replays the durable completion without calling its child"
        );
        Ok(())
    }

    async fn attachment(&self, url: &CidUrl) -> Result<(Vec<u8>, String)> {
        let store = self
            .js
            .get_object_store(harnx_blob_store::ATTACHMENTS_BUCKET)
            .await?;
        harnx_blob_store::get_media(&store, url)
            .await?
            .context("stored original")
    }

    fn cid(&self, mime: &str, bytes: &[u8]) -> Result<CidUrl> {
        CidUrl::parse(&cid_for_data_url_with_session(
            &self.session,
            &format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
        ))
    }
}

async fn start_bridge(
    url: &str,
    scope: &ServerScope,
    script: &Path,
    audit: &Path,
) -> Result<tokio::process::Child> {
    let mock = Path::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
        .parent()
        .unwrap()
        .join("harnx-mock-mcp");
    anyhow::ensure!(
        mock.is_file(),
        "build harnx-mock-mcp before bridge integration tests"
    );
    Ok(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
            .args(["--name", "shared-limit", "--"])
            .arg(mock)
            .arg("--script")
            .arg(script)
            .arg("--request-log")
            .arg(audit)
            .env("HARNX_SERVER_SCOPE", scope.as_str())
            .env("HARNX_NATS_URL", url)
            .env("HARNX_NATS_TOKEN", TOKEN)
            .env_remove("HARNX_MCP_MAX_RESULT_BYTES")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()?,
    )
}

pub(super) async fn stop_bridge(child: &mut tokio::process::Child) -> Result<()> {
    let pid = child.id().context("running bridge pid")?;
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
    assert!(tokio::time::timeout(DEADLINE, child.wait())
        .await??
        .success());
    Ok(())
}
