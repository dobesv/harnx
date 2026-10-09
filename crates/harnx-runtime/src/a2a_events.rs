//! Shared stream policy for A2A publishers and session GC.
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{
    self,
    stream::{Config, DiscardPolicy, RetentionPolicy, StorageType},
};

pub const STREAM: &str = "HARNX_A2A_TASK_EVENTS";
pub const DEFAULT_MAX_BYTES: i64 = 128 * 1024 * 1024;
pub const SUBJECT_CAPACITY: i64 = 128;
pub const CHECKPOINT_HISTORY: usize = 65;

pub fn config(replicas: usize, max_bytes: i64) -> Result<Config> {
    ensure!(max_bytes > 0, "A2A event byte budget must be positive");
    Ok(Config {
        name: STREAM.into(),
        subjects: vec!["a2a.tasks.>".into()],
        retention: RetentionPolicy::Limits,
        discard: DiscardPolicy::New,
        discard_new_per_subject: true,
        max_messages_per_subject: SUBJECT_CAPACITY,
        max_bytes,
        num_replicas: replicas,
        storage: StorageType::File,
        ..Default::default()
    })
}

pub async fn ensure(js: &jetstream::Context, replicas: usize) -> Result<jetstream::stream::Stream> {
    let budget = std::env::var("HARNX_A2A_EVENT_MAX_BYTES")
        .ok()
        .map(|value| {
            value
                .parse::<i64>()
                .context("invalid HARNX_A2A_EVENT_MAX_BYTES")
        })
        .transpose()?
        .unwrap_or(DEFAULT_MAX_BYTES);
    let mut requested = config(replicas, budget)?;
    requested.max_message_size =
        js.client().server_info().max_payload.min(i32::MAX as usize) as i32;
    let stream = js.get_or_create_stream(requested).await?;
    validate(&stream.cached_info().config, replicas)?;
    Ok(stream)
}

pub fn validate(config: &Config, replicas: usize) -> Result<()> {
    ensure!(config.retention == RetentionPolicy::Limits
        && config.discard == DiscardPolicy::New
        && config.discard_new_per_subject
        && capacity_is_safe(config)
        && config.subjects == ["a2a.tasks.>"]
        && config.storage == StorageType::File
        && config.num_replicas == replicas,
        "unsafe task event stream configuration; drain writers and provision documented Limits/DiscardNew policy");
    Ok(())
}

fn capacity_is_safe(config: &Config) -> bool {
    config.max_messages <= 0
        && config.max_messages_per_subject > CHECKPOINT_HISTORY as i64
        && config.max_bytes > 0
        && config.max_age.is_zero()
        && !config.allow_message_ttl
}
