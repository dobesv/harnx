//! Lease regressions with real JetStream storage and controlled API replies.
use crate::common::{spawn_configured_nats_server, NatsServerHandle};
use crate::worker::CI_SAFE_TIMEOUT;
use anyhow::{Context, Result};
use async_nats::header::NATS_MESSAGE_TTL;
use async_nats::jetstream::{self, kv};
use async_nats::{Client, Message, Subscriber};
use futures_util::StreamExt;
use harnx_nats_common::leader_reads::{self, EntryWithHeaders};
use harnx_runtime::nats_lease::{ensure_lease_bucket, LeaseRecord, NatsLeaseConfig};
use serde_json::{json, Value};
use std::time::Duration;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

mod activation;
mod claim;

const INFO: &str = "fixture.info";
const READ: &str = "fixture.read";
const WRITE: &str = "fixture.write";
const REAL_INFO: &str = "fixture.real.info";
const REAL_READ: &str = "fixture.real.read";
const REAL_WRITE: &str = "fixture.real.write";
const WRITER_TTL: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum Intercept<'a> {
    Clock,
    Read,
    Write(&'a str),
}

struct LeaseBroker {
    _server: NatsServerHandle,
    client: Client,
    js: jetstream::Context,
    bucket: kv::Store,
    clock: mpsc::Receiver<Message>,
    _info_task: AbortOnDropHandle<Result<()>>,
}

impl LeaseBroker {
    async fn start(intercept: Intercept<'_>) -> Result<Option<Self>> {
        harnx_core::require_nextest();
        let config = NatsLeaseConfig::default();
        let stream = format!("KV_{}", config.bucket);
        let extra = extra_mappings(intercept, &config.bucket, &stream);
        let server_config = format!(
            r#"mappings {{
                   "$JS.API.STREAM.INFO.{stream}": "{INFO}"
                   "{REAL_INFO}": "$JS.API.STREAM.INFO.{stream}"
                   {extra}
               }}"#
        );
        let Some(server) = spawn_configured_nats_server(&server_config).await? else {
            return Ok(None);
        };
        let client = async_nats::connect(server.url()).await?;
        let js = jetstream::new(client.clone());
        let info = client.subscribe(INFO).await?;
        let (tx, clock) = mpsc::channel(8);
        let info_task = AbortOnDropHandle::new(tokio::spawn(relay_info(client.clone(), info, tx)));
        // A server round trip, not a local flush, orders subscription
        // registration before the worker can send its first request.
        js.query_account().await?;
        let bucket = ensure_lease_bucket(&js, &lease_config(WRITER_TTL)).await?;
        Ok(Some(Self {
            _server: server,
            client,
            js,
            bucket,
            clock,
            _info_task: info_task,
        }))
    }

    async fn next_clock(&mut self) -> Result<Message> {
        tokio::time::timeout(CI_SAFE_TIMEOUT, self.clock.recv())
            .await
            .context("no lease clock request")?
            .context("lease INFO relay stopped")
    }

    async fn reply_clock(&mut self, ts: OffsetDateTime) -> Result<()> {
        let request = self.next_clock().await?;
        reply(&self.client, request, clock_answer(ts)?).await
    }
}

async fn seed_put(bucket: &kv::Store, key: &str, value: Vec<u8>) -> Result<u64> {
    Ok(bucket
        .create_with_ttl(key, value.into(), WRITER_TTL)
        .await?)
}

async fn observe_put(bucket: &kv::Store, key: &str) -> Result<EntryWithHeaders> {
    let entry = leader_reads::entry_with_headers(bucket, key)
        .await?
        .context("expected a lease entry")?;
    assert_eq!(entry.entry.operation, kv::Operation::Put);
    Ok(entry)
}

fn extra_mappings(intercept: Intercept<'_>, bucket: &str, stream: &str) -> String {
    match intercept {
        Intercept::Clock => String::new(),
        Intercept::Read => format!(
            r#""$JS.API.STREAM.MSG.GET.{stream}": "{READ}"
               "{REAL_READ}": "$JS.API.STREAM.MSG.GET.{stream}""#
        ),
        Intercept::Write(key) => format!(
            r#""$KV.{bucket}.{key}": "{WRITE}"
               "{REAL_WRITE}": "$KV.{bucket}.{key}""#
        ),
    }
}

/// NATS applies a mapping once. The alias goes straight to the real API,
/// retaining its reply inbox. async-nats sends `null` for ordinary INFO;
/// only lease_clock sends `{}`. Leave bucket open/reconcile on the real API.
async fn relay_info(
    client: Client,
    mut info: Subscriber,
    clock: mpsc::Sender<Message>,
) -> Result<()> {
    while let Some(request) = info.next().await {
        if request.payload.as_ref() == b"{}" {
            clock.send(request).await?;
        } else {
            forward(&client, REAL_INFO, request).await?;
        }
    }
    Ok(())
}

async fn next_request(sub: &mut Subscriber) -> Result<Message> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, sub.next())
        .await
        .context("expected a mapped broker request")?
        .context("mapped subscription closed")
}

async fn forward(client: &Client, alias: &'static str, request: Message) -> Result<()> {
    let inbox = request
        .reply
        .context("mapped request needs a reply inbox")?;
    client
        .publish_with_reply_and_headers(
            alias,
            inbox,
            request.headers.unwrap_or_default(),
            request.payload,
        )
        .await?;
    Ok(())
}

async fn reply(client: &Client, request: Message, response: Value) -> Result<()> {
    client
        .publish(
            request
                .reply
                .context("mapped request needs a reply inbox")?,
            serde_json::to_vec(&response)?.into(),
        )
        .await?;
    Ok(())
}

fn clock_answer(ts: OffsetDateTime) -> Result<Value> {
    Ok(json!({"ts": ts.format(&Rfc3339)?}))
}

fn lease_config(ttl: Duration) -> NatsLeaseConfig {
    NatsLeaseConfig {
        ttl,
        renew_interval: ttl / 2,
        replicas: 1,
        ..Default::default()
    }
}

fn foreign_record() -> LeaseRecord {
    // Acquired at real worker UTC, not backdated. Use JSON to supply the
    // foreign host identity on every platform without accessing private APIs.
    serde_json::from_value(json!({
        "worker_id": "foreign-worker",
        "generation": 1,
        "holder": {"host": "other-machine-other-boot-other-pidns", "pid": 123, "start_ticks": 42},
        "acquired_at": chrono::Utc::now().to_rfc3339(),
    }))
    .expect("foreign lease wire record")
}

fn assert_ttl(entry: &EntryWithHeaders, ttl: Duration) {
    assert_eq!(
        entry.headers.get(NATS_MESSAGE_TTL).map(|v| v.as_str()),
        Some(ttl.as_secs().to_string().as_str())
    );
}

fn stale_sample(created: OffsetDateTime) -> OffsetDateTime {
    created + WRITER_TTL * 3 + Duration::from_secs(1)
}
