//! Real-process SIGTERM coverage for serve readiness and AG-UI stream drain.

#![cfg(unix)]

#[allow(dead_code)]
#[path = "../../harnx-runtime/tests/common/mod.rs"]
mod common;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::{
    event::{AgentEvent, ContentBlock, ModelEvent},
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_event_sink::NatsEventSink,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const CLUSTER: &str = "sigterm";
const AGENT: &str = "plain";
const CI_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_COUNT: usize = 4;

struct ChildGuard(Child);

impl ChildGuard {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    async fn wait_for_exit(&mut self) -> Result<std::process::ExitStatus> {
        tokio::time::timeout(CI_TIMEOUT, async {
            loop {
                if let Some(status) = self.0.try_wait()? {
                    return Ok::<_, std::io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("harnx-serve did not exit after SIGTERM")?
        .context("poll harnx-serve exit")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RemoteTurn {
    session_id: String,
    storage_key: String,
    sink: NatsEventSink,
    lease: Arc<NatsSessionLease>,
}

struct StreamOutcome {
    body: String,
    closed_at: Instant,
}

async fn read_ag_ui_stream(
    response: reqwest::Response,
    attached: oneshot::Sender<()>,
    text_open: oneshot::Sender<()>,
) -> StreamOutcome {
    let mut stream = response.bytes_stream();
    let mut body = String::new();
    let mut attached = Some(attached);
    let mut text_open = Some(text_open);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                body.push_str(std::str::from_utf8(&chunk).expect("AG-UI stream is UTF-8"));
                if body.contains("RUN_STARTED") {
                    if let Some(attached) = attached.take() {
                        let _ = attached.send(());
                    }
                }
                if body.contains("TEXT_MESSAGE_START") {
                    if let Some(text_open) = text_open.take() {
                        let _ = text_open.send(());
                    }
                }
            }
            Err(_) => break,
        }
    }
    StreamOutcome {
        body,
        closed_at: Instant::now(),
    }
}

fn create_config(root: &Path, nats_url: &str) -> Result<()> {
    for path in [
        root.join("config/nats_servers"),
        root.join("config/clients"),
        root.join("data"),
        root.join("state"),
    ] {
        std::fs::create_dir_all(path)?;
    }
    std::fs::write(
        root.join("config/config.yaml"),
        "save: false\nclient: mock\nmodel: mock:test\n",
    )?;
    std::fs::write(
        root.join("config/clients/mock.yaml"),
        "type: openai-compatible\nname: mock\napi_base: http://127.0.0.1:1/v1\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n",
    )?;
    std::fs::write(
        root.join(format!("config/nats_servers/{CLUSTER}.yaml")),
        format!(
            "url: {nats_url:?}\nagents:\n  - name: {AGENT}\n    description: SIGTERM test agent\n"
        ),
    )?;
    Ok(())
}

async fn seed_remote_turns(
    client: &async_nats::Client,
    jetstream: &async_nats::jetstream::Context,
) -> Result<Vec<RemoteTurn>> {
    let metadata = SessionMetadataStore::ensure(jetstream, 1).await?;
    let mut turns = Vec::new();
    for index in 0..STREAM_COUNT {
        let session_id = format!("sigterm-stream-{index}");
        let record = SessionMetadata::new(
            &session_id,
            SessionInitializer::named(AGENT, Default::default()),
        );
        metadata.create(&record).await?;
        let storage_key = harnx_core::session_identity::session_key(Some(AGENT), &session_id);
        let log = NatsSessionLog::new_with_replicas(jetstream.clone(), &storage_key, 1);
        log.append_event_async(&SessionLogEntry::Message {
            id: Some(format!("message-{index}")),
            role: MessageRole::User,
            content: MessageContent::Text(format!("hold stream {index}")),
            timestamp: None,
            fence_token: None,
        })
        .await?;
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: &storage_key,
            worker_id: "remote-sigterm-worker".into(),
            generation: u64::try_from(index + 1)?,
            config: NatsLeaseConfig::default(),
            session_metadata: Some(metadata.clone()),
        })
        .await?
        .context("acquire remote turn lease")?;
        let sink = NatsEventSink::new(client.clone(), jetstream.clone(), storage_key.clone()).await;
        turns.push(RemoteTurn {
            session_id,
            storage_key,
            sink,
            lease: Arc::new(lease),
        });
    }
    Ok(turns)
}

fn reserve_address() -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    drop(listener);
    Ok(address)
}

fn serve_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harnx-serve"))
}

fn spawn_serve(root: &Path, server_addr: &str, health_addr: &str) -> Result<ChildGuard> {
    let child = Command::new(serve_binary())
        .arg("--addr")
        .arg(server_addr)
        .arg("--healthz-addr")
        .arg(health_addr)
        .arg("--drain-timeout-seconds")
        .arg("5")
        .arg("--stream-drain-min-jitter-ms")
        .arg("50")
        .arg("--stream-drain-max-jitter-ms")
        .arg("750")
        .env("HARNX_CONFIG_DIR", root.join("config"))
        .env("HARNX_DATA_DIR", root.join("data"))
        .env("HARNX_STATE_DIR", root.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn harnx-serve")?;
    Ok(ChildGuard(child))
}

async fn wait_for_health(url: &str, expected: reqwest::StatusCode) -> Result<()> {
    let client = reqwest::Client::new();
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            if let Ok(response) = client.get(url).send().await {
                if response.status() == expected {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .with_context(|| format!("health endpoint never returned {expected}"))??;
    Ok(())
}

async fn observe_not_ready(url: &str, armed: oneshot::Sender<()>) -> Result<()> {
    let client = reqwest::Client::new();
    let mut armed = Some(armed);
    tokio::time::timeout(CI_TIMEOUT, async {
        loop {
            match client.get(url).send().await {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    if let Some(armed) = armed.take() {
                        let _ = armed.send(());
                    }
                }
                Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    return Ok::<_, anyhow::Error>(());
                }
                Ok(_) | Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .context("serve readiness never returned 503 after SIGTERM")??;
    Ok(())
}

fn send_sigterm(pid: u32) {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        result,
        0,
        "send SIGTERM to harnx-serve {pid}: {}",
        std::io::Error::last_os_error()
    );
}

async fn open_and_prime_streams(
    server_url: &str,
    turns: &[RemoteTurn],
) -> Result<Vec<tokio::task::JoinHandle<StreamOutcome>>> {
    let http = reqwest::Client::new();
    let mut readers = Vec::new();
    let mut attached = Vec::new();
    let mut text_open = Vec::new();
    for turn in turns {
        let response = http
            .post(format!(
                "{server_url}/v1/agents/{AGENT}%40{CLUSTER}/sessions/{}",
                turn.session_id
            ))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&json!({
                "threadId": format!("thread-{}", turn.session_id),
                "runId": format!("run-{}", turn.session_id),
                "messages": []
            }))
            .send()
            .await
            .context("open remote AG-UI stream")?;
        anyhow::ensure!(
            response.status().is_success(),
            "AG-UI request failed: {}",
            response.status()
        );
        let (attached_tx, attached_rx) = oneshot::channel();
        let (text_tx, text_rx) = oneshot::channel();
        readers.push(tokio::spawn(read_ag_ui_stream(
            response,
            attached_tx,
            text_tx,
        )));
        attached.push(attached_rx);
        text_open.push(text_rx);
    }
    for attached in attached {
        tokio::time::timeout(CI_TIMEOUT, attached)
            .await
            .context("remote AG-UI stream never attached")??;
    }
    for (index, turn) in turns.iter().enumerate() {
        turn.sink
            .publish_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(format!("partial-{index}"))],
            }))
            .await;
    }
    for text_open in text_open {
        tokio::time::timeout(CI_TIMEOUT, text_open)
            .await
            .context("AG-UI stream never opened a text lifecycle")??;
    }
    Ok(readers)
}

fn assert_backend_leases_held(turns: &[RemoteTurn], message: &str) {
    for turn in turns {
        assert!(turn.lease.is_held(), "{message}: {}", turn.session_id);
    }
}

async fn collect_stream_outcomes(
    readers: Vec<tokio::task::JoinHandle<StreamOutcome>>,
) -> Result<Vec<StreamOutcome>> {
    let mut outcomes = Vec::with_capacity(readers.len());
    for reader in readers {
        outcomes.push(tokio::time::timeout(CI_TIMEOUT, reader).await??);
    }
    Ok(outcomes)
}

fn assert_stream_outcomes(outcomes: &[StreamOutcome]) {
    for outcome in outcomes {
        assert!(outcome.body.contains("TEXT_MESSAGE_START"));
        let end = outcome
            .body
            .find("TEXT_MESSAGE_END")
            .expect("text end frame");
        let error = outcome.body.find("RUN_ERROR").expect("run error frame");
        assert!(end < error, "lifecycle must finish before RUN_ERROR");
    }
    let earliest = outcomes
        .iter()
        .map(|outcome| outcome.closed_at)
        .min()
        .expect("stream outcome");
    let latest = outcomes
        .iter()
        .map(|outcome| outcome.closed_at)
        .max()
        .expect("stream outcome");
    assert!(
        latest.duration_since(earliest) >= Duration::from_millis(20),
        "independent stream jitter collapsed to one close time"
    );
}

async fn verify_backend_turns(
    turns: Vec<RemoteTurn>,
    jetstream: &async_nats::jetstream::Context,
) -> Result<()> {
    for turn in turns {
        assert!(
            turn.lease.is_held(),
            "backend lease changed during serve drain"
        );
        let entries = NatsSessionLog::new(jetstream.clone(), turn.storage_key)
            .load_events_latest_async()
            .await?;
        assert!(
            !entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
            "serve shutdown appended backend Cancel for {}",
            turn.session_id
        );
        turn.lease.release().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_sigterm_jitters_ag_ui_drain_without_cancelling_backend_turns() -> Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let turns = seed_remote_turns(&client, &jetstream).await?;
    let root = tempfile::tempdir()?;
    create_config(root.path(), nats.url())?;
    let server_addr = reserve_address()?;
    let health_addr = reserve_address()?;
    let server_url = format!("http://{server_addr}");
    let health_url = format!("http://{health_addr}/healthz");
    let mut serve = spawn_serve(root.path(), &server_addr, &health_addr)?;
    wait_for_health(&health_url, reqwest::StatusCode::OK).await?;
    let readers = open_and_prime_streams(&server_url, &turns).await?;

    let (armed_tx, armed_rx) = oneshot::channel();
    let readiness = tokio::spawn({
        let health_url = health_url.clone();
        async move { observe_not_ready(&health_url, armed_tx).await }
    });
    armed_rx.await.context("arm serve readiness observer")?;
    let shutdown_at = Instant::now();
    send_sigterm(serve.pid());
    readiness.await.context("join readiness observer")??;
    assert_backend_leases_held(&turns, "serve SIGTERM released or cancelled backend turn");

    let outcomes = collect_stream_outcomes(readers).await?;
    assert_stream_outcomes(&outcomes);
    let status = serve.wait_for_exit().await?;
    assert!(
        status.success(),
        "harnx-serve exited unsuccessfully: {status}"
    );
    assert!(
        shutdown_at.elapsed() < Duration::from_secs(5),
        "serve exceeded configured drain ceiling"
    );
    verify_backend_turns(turns, &jetstream).await
}
